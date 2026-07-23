use super::*;

const QUARANTINE_RECORD_SCHEMA_VERSION_V1: u32 = 1;
const MAX_QUARANTINE_RECORD_BYTES: u64 = 1024 * 1024;
const RECOVERY_RESCUE_RECORD_SCHEMA_VERSION: u32 = 1;
const RECOVERY_EXPORT_MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_RECOVERY_RESCUE_RECORD_BYTES: u64 = 8 * 1024 * 1024;
const RECOVERY_EXPORT_MANIFEST_FILE: &str = "CheckPo-Recovery.json";
const RECOVERY_EXPORT_COMPLETE_FILE: &str = "保存が完了しました.txt";
const TARGET_RECONCILE_MAX_ROUNDS: usize = 5;
const TARGET_RECONCILE_MAX_ELAPSED: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RecoveryRescueState {
    Prepared,
    Resolving,
    Recovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryRescueEntry {
    conflict: TransactionRecoveryConflict,
    exported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryRescueRecord {
    schema_version: u32,
    transaction_id: String,
    checkpoint_id: SnapshotId,
    plan_id: String,
    created_at_utc: String,
    state: RecoveryRescueState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    export_directory: Option<PathBuf>,
    entries: Vec<RecoveryRescueEntry>,
    completed_paths: Vec<TrackedUnityFilePath>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryExportManifest<'a> {
    schema_version: u32,
    transaction_id: &'a str,
    created_at_utc: String,
    files: Vec<&'a TransactionRecoveryConflict>,
}

struct RecoveryExportStage {
    export_root: PathBuf,
    staging_name: std::ffi::OsString,
    final_name: std::ffi::OsString,
    staging_directory: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineRecordEnvelope {
    schema_version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuarantineRecord {
    schema_version: u32,
    transaction_id: String,
    quarantined_at_utc: String,
    original_journal_path: PathBuf,
    project_was_verified_in_before_state: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_at_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_checkpoint_id: Option<SnapshotId>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuarantineResolutionRecord {
    schema_version: u32,
    resolved_at_utc: String,
    resolved_checkpoint_id: SnapshotId,
    quarantine_record_digest: String,
}

pub fn unresolved_transaction_quarantines(
    project_path: impl AsRef<Path>,
) -> Result<Vec<UnresolvedTransactionQuarantine>> {
    let project = crate::load_project(project_path)?;
    let _lock =
        crate::acquire_project_repository_shared_lock(&project, "transaction-quarantine-status")?;
    unresolved_transaction_quarantines_for_project(&project)
}

pub fn unresolved_transaction_quarantines_for_project(
    project: &ProjectContext,
) -> Result<Vec<UnresolvedTransactionQuarantine>> {
    let Some((quarantine_root, record_paths)) = quarantine_record_paths(project)? else {
        return Ok(Vec::new());
    };
    let record_names = record_paths
        .iter()
        .filter_map(|path| path.file_stem().and_then(|value| value.to_str()))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let mut unresolved = Vec::new();
    for record_path in record_paths {
        let record_name = record_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string();
        let quarantine_path = quarantine_root.join(&record_name);
        let fallback_transaction_id = record_name
            .split('-')
            .next()
            .unwrap_or(&record_name)
            .to_string();
        let metadata = match fs::symlink_metadata(&record_path) {
            Ok(metadata) => metadata,
            Err(error) => return Err(crate::io_error(&record_path, error)),
        };
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: fallback_transaction_id,
                quarantined_at_utc: None,
                quarantine_path,
                reason: "quarantine record is not a regular file".to_string(),
            });
            continue;
        }
        if quarantine_record_has_valid_resolution(&record_path)? {
            continue;
        }
        match read_quarantine_record(&record_path) {
            Ok(record)
                if !record.project_was_verified_in_before_state
                    && record.resolved_at_utc.is_none() =>
            {
                unresolved.push(UnresolvedTransactionQuarantine {
                    transaction_id: record.transaction_id,
                    quarantined_at_utc: Some(record.quarantined_at_utc),
                    quarantine_path,
                    reason: "the Unity project could not be verified in its pre-transaction state"
                        .to_string(),
                });
            }
            Ok(_) => {}
            Err(error) => unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: fallback_transaction_id,
                quarantined_at_utc: None,
                quarantine_path,
                reason: format!("quarantine record could not be verified: {error}"),
            }),
        }
    }
    for entry in
        fs::read_dir(&quarantine_root).map_err(|error| crate::io_error(&quarantine_root, error))?
    {
        let entry = entry.map_err(|error| crate::io_error(&quarantine_root, error))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if metadata.is_dir() && !record_names.contains(&name) {
            unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: name.split('-').next().unwrap_or(&name).to_string(),
                quarantined_at_utc: None,
                quarantine_path: entry.path(),
                reason: "quarantined transaction payload has no matching record".to_string(),
            });
        } else if crate::metadata_is_link_or_reparse(&metadata) {
            unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: name,
                quarantined_at_utc: None,
                quarantine_path: entry.path(),
                reason: "quarantine entry is a symbolic link or reparse point".to_string(),
            });
        }
    }
    unresolved.sort_by(|left, right| {
        left.quarantined_at_utc
            .cmp(&right.quarantined_at_utc)
            .then_with(|| left.transaction_id.cmp(&right.transaction_id))
    });
    Ok(unresolved)
}

pub fn ensure_no_unresolved_transaction_quarantines(project: &ProjectContext) -> Result<()> {
    let unresolved = unresolved_transaction_quarantines_for_project(project)?;
    if unresolved.is_empty() {
        return Ok(());
    }
    Err(CheckPoError::UnresolvedTransactionQuarantine(format!(
        "{} unresolved quarantined transaction(s); restore a known good checkpoint before changing this project",
        unresolved.len()
    )))
}

pub fn recover_transactions(project_path: impl AsRef<Path>) -> Result<TransactionRecoveryResult> {
    let project = crate::load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = crate::acquire_project_repository_lock(&project, "transaction-recover")?;
    let mut result = TransactionRecoveryResult {
        recovered_transaction_count: 0,
        failed_transaction_count: 0,
        recovered_transaction_ids: Vec::new(),
        failed_transactions: Vec::new(),
    };
    for pending in pending_transactions_for_project(&project)? {
        match recover_one_with_active_rescue(&project, &pending) {
            Ok(()) => {
                result.recovered_transaction_count += 1;
                result
                    .recovered_transaction_ids
                    .push(pending.transaction_id);
            }
            Err(error) => {
                crate::log_operation_error("transaction-recovery", &error.to_string());
                let failed_journal = read_transaction_journal(&pending.journal_path).ok();
                let awaiting_unity = failed_journal
                    .as_ref()
                    .is_some_and(|journal| journal.state == JournalState::AwaitingUnity);
                let recovery_conflict_count = if failed_journal
                    .as_ref()
                    .is_some_and(|journal| journal.intent == TransactionIntent::CompleteToTarget)
                {
                    0
                } else {
                    analyze_transaction_recovery_conflicts_locked(&project, &pending.transaction_id)
                        .map(|plan| plan.conflicts.len())
                        .unwrap_or(0)
                };
                result.failed_transaction_count += 1;
                result.failed_transactions.push(TransactionRecoveryFailure {
                    transaction_id: pending.transaction_id,
                    error: error.to_string(),
                    recovery_conflict_count,
                    awaiting_unity,
                });
            }
        }
    }
    Ok(result)
}

pub(super) fn resume_complete_to_target_after_apply_error(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
    kind: OperationPlanKind,
) -> Result<Option<(String, PathBuf, OperationPlan)>> {
    super::unity_guard::ensure_unity_editor_is_closed(project)?;
    let matching = pending_transactions_for_project(project)?
        .into_iter()
        .find_map(|pending| {
            let journal = read_transaction_journal(&pending.journal_path).ok()?;
            (journal.intent == TransactionIntent::CompleteToTarget
                && &journal.checkpoint_id == checkpoint_id
                && journal.kind == kind)
                .then_some((pending.transaction_id, pending.journal_path))
        });
    let Some((transaction_id, journal_path)) = matching else {
        return Ok(None);
    };
    let recovered = recover_transactions(project.project_root.as_path())?;
    if recovered
        .recovered_transaction_ids
        .iter()
        .any(|candidate| candidate == &transaction_id)
    {
        let journal = read_transaction_journal(&journal_path)?;
        let effective_plan = operation_plan_from_journal(&journal);
        return Ok(Some((transaction_id, journal_path, effective_plan)));
    }
    let detail = recovered
        .failed_transactions
        .iter()
        .find(|failure| failure.transaction_id == transaction_id)
        .map(|failure| failure.error.as_str())
        .unwrap_or("the target-authoritative transaction is still pending");
    Err(CheckPoError::WorkingTreeChanged(format!(
        "Unity is still updating the project. Close Unity and continue recovery: {detail}"
    )))
}

fn operation_plan_from_journal(journal: &TransactionJournal) -> OperationPlan {
    OperationPlan::new(
        journal.checkpoint_id.clone(),
        journal.kind,
        journal.selected_paths.clone(),
        journal.operations.clone(),
    )
    .with_directory_changes(
        journal.directories_to_remove.clone(),
        journal.directories_to_create.clone(),
    )
}

pub fn analyze_transaction_recovery_conflicts(
    project_path: impl AsRef<Path>,
    transaction_id: &str,
) -> Result<TransactionRecoveryConflictPlan> {
    validate_transaction_id(transaction_id)?;
    let project = crate::load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(
        &project,
        "transaction-recovery-conflict-analyze",
    )?;
    analyze_transaction_recovery_conflicts_locked(&project, transaction_id)
}

pub fn recover_transaction_with_conflict_export(
    project_path: impl AsRef<Path>,
    transaction_id: &str,
    expected_plan_id: &str,
    selected_paths: &[TrackedUnityFilePath],
    export_root: &Path,
    options: ApplyOptions,
) -> Result<TransactionRecoveryConflictResult> {
    if !options.yes {
        return Err(crate::user_error(
            "transaction conflict recovery requires --yes.",
        ));
    }
    validate_transaction_id(transaction_id)?;
    validate_recovery_conflict_plan_id(expected_plan_id)?;
    let project = crate::load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    super::unity_guard::ensure_unity_editor_is_closed(&project)?;
    let _lock =
        crate::acquire_project_repository_lock(&project, "transaction-recovery-conflict-apply")?;
    let plan = analyze_transaction_recovery_conflicts_locked(&project, transaction_id)?;
    if plan.plan_id != expected_plan_id {
        return Err(CheckPoError::WorkingTreeChanged(
            "recovery conflict files changed after preview".to_string(),
        ));
    }
    if plan.conflicts.is_empty() {
        return Err(crate::user_error(
            "the transaction no longer has file conflicts; run normal recovery again.",
        ));
    }

    let conflict_paths = plan
        .conflicts
        .iter()
        .map(|conflict| conflict.path.clone())
        .collect::<BTreeSet<_>>();
    let selected = selected_paths.iter().cloned().collect::<BTreeSet<_>>();
    if selected.len() != selected_paths.len() || !selected.is_subset(&conflict_paths) {
        return Err(crate::user_error(
            "selected recovery files are not part of the analyzed conflict plan.",
        ));
    }

    let export_directory = if selected.is_empty() {
        None
    } else {
        let export_stage = create_recovery_export_stage(&project, export_root, transaction_id)?;
        for conflict in plan
            .conflicts
            .iter()
            .filter(|conflict| selected.contains(&conflict.path))
        {
            copy_recovery_conflict_to_export(&project, conflict, &export_stage.staging_directory)?;
        }
        Some(complete_recovery_export(
            export_stage,
            transaction_id,
            plan.conflicts
                .iter()
                .filter(|conflict| selected.contains(&conflict.path)),
        )?)
    };

    let pending = pending_transaction_by_id(&project, transaction_id)?;
    let journal = read_valid_recovery_journal(&project, &pending)?;
    prepare_recovery_conflict_rescue(
        &project,
        &journal,
        &plan,
        &selected,
        export_directory.as_deref(),
    )?;
    recover_one_with_active_rescue(&project, &pending)?;
    Ok(TransactionRecoveryConflictResult {
        transaction_id: transaction_id.to_string(),
        recovered: true,
        export_directory,
        exported_paths: selected.into_iter().collect(),
        restored_without_export_count: plan.conflicts.len().saturating_sub(selected_paths.len()),
    })
}

fn analyze_transaction_recovery_conflicts_locked(
    project: &ProjectContext,
    transaction_id: &str,
) -> Result<TransactionRecoveryConflictPlan> {
    let pending = pending_transaction_by_id(project, transaction_id)?;
    let journal = read_valid_recovery_journal(project, &pending)?;
    let before_paths = journal_before_paths(&journal.operations);
    let mut conflicts = Vec::new();
    for operation in &journal.operations {
        let Some(current) =
            current_file_state_for_recovery(project, &operation.path, &before_paths)?
        else {
            if operation.operation_type == FileOperationType::SetMetadata {
                return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
            }
            continue;
        };
        let matches_before = operation.before_hash.as_ref() == Some(&current.hash)
            && operation.before_size_bytes == Some(current.size_bytes);
        let matches_after = operation.after_hash.as_ref() == Some(&current.hash)
            && operation.after_size_bytes == Some(current.size_bytes);
        let metadata_only = operation.operation_type == FileOperationType::SetMetadata
            && matches_before
            && operation.before_modified_at_utc.as_deref()
                != Some(current.modified_at_utc.as_str())
            && operation.after_modified_at_utc.as_deref() != Some(current.modified_at_utc.as_str());
        let content_conflict = match operation.operation_type {
            FileOperationType::Restore => !matches_after,
            FileOperationType::Replace => !matches_before && !matches_after,
            FileOperationType::Delete => !matches_before,
            FileOperationType::SetMetadata => {
                if !matches_before {
                    return Err(CheckPoError::WorkingTreeChanged(format!(
                        "{} changed content during a metadata-only operation",
                        operation.path
                    )));
                }
                false
            }
        };
        if content_conflict || metadata_only {
            conflicts.push(TransactionRecoveryConflict {
                path: operation.path.clone(),
                current_hash: current.hash,
                size_bytes: current.size_bytes,
                modified_at_utc: current.modified_at_utc,
                metadata_only,
            });
        }
    }
    conflicts.sort_by(|left, right| left.path.cmp(&right.path));
    let journal_bytes = serde_json::to_vec(&journal)
        .map_err(|error| CheckPoError::Corruption(error.to_string()))?;
    let journal_digest = blake3::hash(&journal_bytes).to_hex().to_string();
    let plan_id = recovery_conflict_plan_id(
        &project.project_id,
        &journal.transaction_id,
        &journal.checkpoint_id,
        &journal_digest,
        &conflicts,
    )?;
    Ok(TransactionRecoveryConflictPlan {
        schema_version: crate::TRANSACTION_RECOVERY_CONFLICT_PLAN_SCHEMA_VERSION,
        plan_id,
        transaction_id: journal.transaction_id,
        checkpoint_id: journal.checkpoint_id,
        conflicts,
    })
}

fn pending_transaction_by_id(
    project: &ProjectContext,
    transaction_id: &str,
) -> Result<PendingTransaction> {
    pending_transactions_for_project(project)?
        .into_iter()
        .find(|pending| pending.transaction_id == transaction_id)
        .ok_or_else(|| crate::user_error("the interrupted transaction is no longer pending."))
}

fn read_valid_recovery_journal(
    project: &ProjectContext,
    pending: &PendingTransaction,
) -> Result<TransactionJournal> {
    let tx_root = pending
        .journal_path
        .parent()
        .ok_or_else(|| CheckPoError::Corruption("invalid journal path".into()))?;
    ensure_regular_transaction_directory(tx_root)?;
    let metadata = fs::symlink_metadata(&pending.journal_path)
        .map_err(|error| crate::io_error(&pending.journal_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(
            "transaction journal is not a regular file".to_string(),
        ));
    }
    let journal = read_transaction_journal(&pending.journal_path)?;
    validate_transaction_journal_identity(tx_root, &journal)?;
    if journal.operations.is_empty() {
        return Err(CheckPoError::Corruption(
            "transaction journal contains no operations".to_string(),
        ));
    }
    validate_journal_operations(project, &journal.checkpoint_id, &journal.operations)?;
    validate_journal_directory_topology(
        &journal.operations,
        &journal.directories_to_remove,
        &journal.directories_to_create,
    )?;
    match (journal.kind, journal.selected_paths.as_deref()) {
        (OperationPlanKind::Restore, None) => {}
        (OperationPlanKind::Discard, Some(selected)) if !selected.is_empty() => {
            let selected = selected.iter().collect::<BTreeSet<_>>();
            if journal
                .operations
                .iter()
                .any(|operation| !selected.contains(&operation.path))
            {
                return Err(CheckPoError::Corruption(
                    "discard journal operations exceed the selected target scope".to_string(),
                ));
            }
        }
        _ => {
            return Err(CheckPoError::Corruption(
                "transaction target scope does not match its operation kind".to_string(),
            ))
        }
    }
    let backup_root = tx_root.join("backup");
    validate_transaction_payload(
        &backup_root,
        journal
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Delete | FileOperationType::Replace
                )
            })
            .map(|operation| operation.path.clone())
            .collect(),
    )?;
    let staged_root = tx_root.join("staged");
    validate_transaction_payload(
        &staged_root,
        journal
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Restore | FileOperationType::Replace
                )
            })
            .map(|operation| operation.path.clone())
            .collect(),
    )?;
    Ok(journal)
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if transaction_id.len() == 32
        && transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(crate::user_error("invalid transaction id."))
    }
}

fn validate_recovery_conflict_plan_id(plan_id: &str) -> Result<()> {
    if plan_id.len() == 64
        && plan_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(crate::user_error("invalid recovery conflict plan id."))
    }
}

fn recovery_conflict_plan_id(
    project_id: &crate::ProjectId,
    transaction_id: &str,
    checkpoint_id: &SnapshotId,
    journal_digest: &str,
    conflicts: &[TransactionRecoveryConflict],
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"checkpo.transaction-recovery-conflict-plan.v1\0");
    hash_recovery_plan_field(&mut hasher, project_id.as_str().as_bytes())?;
    hash_recovery_plan_field(&mut hasher, transaction_id.as_bytes())?;
    hash_recovery_plan_field(&mut hasher, checkpoint_id.as_str().as_bytes())?;
    hash_recovery_plan_field(&mut hasher, journal_digest.as_bytes())?;
    for conflict in conflicts {
        hash_recovery_plan_field(&mut hasher, conflict.path.as_str().as_bytes())?;
        hash_recovery_plan_field(&mut hasher, conflict.current_hash.as_str().as_bytes())?;
        hasher.update(&conflict.size_bytes.to_be_bytes());
        hash_recovery_plan_field(&mut hasher, conflict.modified_at_utc.as_bytes())?;
        hasher.update(&[u8::from(conflict.metadata_only)]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_recovery_plan_field(hasher: &mut blake3::Hasher, value: &[u8]) -> Result<()> {
    let length = u64::try_from(value.len())
        .map_err(|_| CheckPoError::Corruption("recovery plan field is too large".into()))?;
    hasher.update(&length.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn create_recovery_export_stage(
    project: &ProjectContext,
    export_root: &Path,
    transaction_id: &str,
) -> Result<RecoveryExportStage> {
    if !export_root.is_absolute() {
        return Err(crate::user_error(
            "recovery save location must be an absolute path.",
        ));
    }
    let export_root = export_root
        .canonicalize()
        .map_err(|error| crate::io_error(export_root, error))?;
    let project_root = project
        .project_root
        .as_path()
        .canonicalize()
        .map_err(|error| crate::io_error(project.project_root.as_path(), error))?;
    let repo_root = project
        .repo_root
        .canonicalize()
        .map_err(|error| crate::io_error(&project.repo_root, error))?;
    if export_root.starts_with(&project_root) || export_root.starts_with(&repo_root) {
        return Err(crate::user_error(
            "recovery files must be saved outside the Unity project and CheckPo storage.",
        ));
    }
    let root = crate::storage::AnchoredRoot::open(&export_root)?;
    for _ in 0..16 {
        let suffix = &Uuid::new_v4().simple().to_string()[..8];
        let final_name = std::ffi::OsString::from(format!(
            "CheckPo-Recovery-{}-{}",
            &transaction_id[..8],
            suffix
        ));
        let staging_name = std::ffi::OsString::from(format!(
            ".CheckPo-Recovery-{}-{}-incomplete",
            &transaction_id[..8],
            suffix
        ));
        let relative = Path::new(&staging_name);
        let (parent, leaf) = root.open_parent_for_mutation(relative, false)?;
        match parent.create_directory(&leaf) {
            Ok(_) => {
                parent.sync_all()?;
                root.verify_root_binding()?;
                return Ok(RecoveryExportStage {
                    staging_directory: export_root.join(&staging_name),
                    export_root,
                    staging_name,
                    final_name,
                });
            }
            Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(CheckPoError::Unexpected(
        "could not create a unique recovery export directory".to_string(),
    ))
}

fn complete_recovery_export<'a>(
    stage: RecoveryExportStage,
    transaction_id: &str,
    conflicts: impl Iterator<Item = &'a TransactionRecoveryConflict>,
) -> Result<PathBuf> {
    let files = conflicts.collect::<Vec<_>>();
    let staging_root = crate::storage::AnchoredRoot::open(&stage.staging_directory)?;
    staging_root.write_json_atomic_new(
        Path::new(RECOVERY_EXPORT_MANIFEST_FILE),
        &RecoveryExportManifest {
            schema_version: RECOVERY_EXPORT_MANIFEST_SCHEMA_VERSION,
            transaction_id,
            created_at_utc: crate::now_utc_string(),
            files,
        },
    )?;
    staging_root.write_bytes_atomic_new(
        Path::new(RECOVERY_EXPORT_COMPLETE_FILE),
        "このフォルダーの保存は完了しています。\r\n".as_bytes(),
    )?;
    staging_root.verify_root_binding()?;

    let export_root = crate::storage::AnchoredRoot::open(&stage.export_root)?;
    let (parent, staging_leaf) =
        export_root.open_parent_for_mutation(Path::new(&stage.staging_name), false)?;
    let staging_directory = parent.open_directory(&staging_leaf)?;
    parent.rename_directory_no_replace_to_owned(
        &staging_leaf,
        staging_directory,
        &parent,
        &stage.final_name,
    )?;
    parent.sync_all()?;
    export_root.verify_root_binding()?;
    Ok(stage.export_root.join(stage.final_name))
}

fn copy_recovery_conflict_to_export(
    project: &ProjectContext,
    conflict: &TransactionRecoveryConflict,
    export_directory: &Path,
) -> Result<()> {
    let source_root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(conflict.path.as_str());
    let (source_parent, source_leaf) = source_root.open_parent(relative, false)?;
    let mut source = source_parent.open_file(&source_leaf)?;
    let source_hash = source.hash()?;
    let source_modified_at_utc = source_hash
        .metadata
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(conflict.path.to_string(), error))?;
    if source_hash.object_id != conflict.current_hash
        || source_hash.metadata.len() != conflict.size_bytes
        || source_modified_at_utc != conflict.modified_at_utc
    {
        return Err(CheckPoError::WorkingTreeChanged(conflict.path.to_string()));
    }

    let export_root = crate::storage::AnchoredRoot::open(export_directory)?;
    let (destination_parent, destination_leaf) =
        export_root.open_parent_for_mutation(relative, true)?;
    let (temporary_leaf, mut output) =
        destination_parent.create_unique_temporary_file("recovery-export")?;
    let copy_result = (|| -> Result<()> {
        let copied = source.copy_and_hash_to(&mut output, &export_directory.join(relative))?;
        if copied.object_id != conflict.current_hash || copied.metadata.len() != conflict.size_bytes
        {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery export mismatch for {}",
                conflict.path
            )));
        }
        let modified = chrono::DateTime::parse_from_rfc3339(&conflict.modified_at_utc)
            .map_err(|error| CheckPoError::Corruption(error.to_string()))?
            .with_timezone(&chrono::Utc);
        output.set_mtime(modified.into())?;
        output.sync_all()?;
        let readback = output.hash()?;
        if readback.object_id != conflict.current_hash
            || readback.metadata.len() != conflict.size_bytes
        {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery export readback mismatch for {}",
                conflict.path
            )));
        }
        source_parent.verify_file_binding(&source_leaf, &source)?;
        destination_parent.verify_file_binding(&temporary_leaf, &output)?;
        destination_parent.rename_no_replace_to(
            &temporary_leaf,
            &output,
            &destination_parent,
            &destination_leaf,
        )?;
        destination_parent.verify_file_binding(&destination_leaf, &output)?;
        destination_parent.sync_all()?;
        source_root.verify_root_binding()?;
        export_root.verify_root_binding()
    })();
    if let Err(error) = copy_result {
        let cleanup_leaf = if destination_parent
            .verify_file_binding(&destination_leaf, &output)
            .is_ok()
        {
            destination_leaf.as_os_str()
        } else {
            temporary_leaf.as_os_str()
        };
        let _ = destination_parent.unlink_file_if_bound(cleanup_leaf, output);
        return Err(error);
    }
    Ok(())
}

fn ensure_recovery_rescue_capacity(
    project: &ProjectContext,
    plan: &TransactionRecoveryConflictPlan,
) -> Result<()> {
    let root = project
        .repo_root
        .join(recovery_rescue_files_relative_for_transaction(
            &plan.transaction_id,
        ));
    let mut required_bytes = 0_u64;
    let mut seen = BTreeSet::new();
    for conflict in plan
        .conflicts
        .iter()
        .filter(|conflict| !conflict.metadata_only)
    {
        if !seen.insert(conflict.current_hash.clone()) {
            continue;
        }
        let path = root.join(conflict.current_hash.as_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "recovery rescue object is unsafe: {}",
                    path.display()
                )))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                required_bytes =
                    required_bytes
                        .checked_add(conflict.size_bytes)
                        .ok_or_else(|| {
                            CheckPoError::Corruption("recovery rescue size overflow".to_string())
                        })?;
            }
            Err(error) => return Err(crate::io_error(&path, error)),
        }
    }
    super::apply::ensure_available_space("checkpoint storage", &project.repo_root, required_bytes)
}

fn prepare_recovery_conflict_rescue(
    project: &ProjectContext,
    journal: &TransactionJournal,
    plan: &TransactionRecoveryConflictPlan,
    selected: &BTreeSet<TrackedUnityFilePath>,
    export_directory: Option<&Path>,
) -> Result<()> {
    if journal.transaction_id != plan.transaction_id || journal.checkpoint_id != plan.checkpoint_id
    {
        return Err(CheckPoError::WorkingTreeChanged(
            "recovery transaction changed while preparing rescue data".to_string(),
        ));
    }
    ensure_recovery_rescue_capacity(project, plan)?;
    let rescue_files_root = project
        .repo_root
        .join(recovery_rescue_files_relative_for_transaction(
            &plan.transaction_id,
        ));
    let before_paths = journal_before_paths(&journal.operations);
    for conflict in &plan.conflicts {
        verify_recovery_conflict_is_current(project, conflict, &before_paths)?;
        if !conflict.metadata_only {
            let source = conflict
                .path
                .to_project_path(project.project_root.as_path());
            preserve_after_file_for_recovery(
                project,
                &source,
                &conflict.current_hash,
                &rescue_files_root,
            )?;
        }
    }
    // Publish the durable path-to-copy mapping only after every private copy
    // and any user-visible export have been fully verified.
    for conflict in &plan.conflicts {
        verify_recovery_conflict_is_current(project, conflict, &before_paths)?;
    }
    write_recovery_rescue_record(
        project,
        &RecoveryRescueRecord {
            schema_version: RECOVERY_RESCUE_RECORD_SCHEMA_VERSION,
            transaction_id: plan.transaction_id.clone(),
            checkpoint_id: plan.checkpoint_id.clone(),
            plan_id: plan.plan_id.clone(),
            created_at_utc: crate::now_utc_string(),
            state: RecoveryRescueState::Prepared,
            export_directory: export_directory.map(Path::to_path_buf),
            entries: plan
                .conflicts
                .iter()
                .cloned()
                .map(|conflict| RecoveryRescueEntry {
                    exported: selected.contains(&conflict.path),
                    conflict,
                })
                .collect(),
            completed_paths: Vec::new(),
        },
    )
}

#[cfg(test)]
pub(super) fn prepare_recovery_conflict_rescue_for_test(
    project: &ProjectContext,
    plan: &TransactionRecoveryConflictPlan,
) -> Result<()> {
    let pending = pending_transaction_by_id(project, &plan.transaction_id)?;
    let journal = read_valid_recovery_journal(project, &pending)?;
    prepare_recovery_conflict_rescue(project, &journal, plan, &BTreeSet::new(), None)
}

#[cfg(test)]
pub(super) fn prepare_recovery_conflict_rescue_and_remove_first_for_test(
    project: &ProjectContext,
    plan: &TransactionRecoveryConflictPlan,
) -> Result<()> {
    prepare_recovery_conflict_rescue_for_test(project, plan)?;
    let conflict = plan
        .conflicts
        .iter()
        .find(|conflict| !conflict.metadata_only)
        .ok_or_else(|| CheckPoError::Unexpected("test plan has no content conflict".to_string()))?;
    let source = conflict
        .path
        .to_project_path(project.project_root.as_path());
    remove_anchored_project_file(project, &source, &conflict.current_hash)
}

fn verify_recovery_conflict_is_current(
    project: &ProjectContext,
    conflict: &TransactionRecoveryConflict,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<()> {
    let current = current_file_state_for_recovery(project, &conflict.path, before_paths)?
        .ok_or_else(|| CheckPoError::WorkingTreeChanged(conflict.path.to_string()))?;
    if current.hash != conflict.current_hash
        || current.size_bytes != conflict.size_bytes
        || current.modified_at_utc != conflict.modified_at_utc
    {
        return Err(CheckPoError::WorkingTreeChanged(conflict.path.to_string()));
    }
    Ok(())
}

fn recovery_rescue_record_relative(record: &RecoveryRescueRecord) -> PathBuf {
    Path::new("recovery-rescues")
        .join(&record.transaction_id)
        .join("records")
        .join(format!("{}.json", record.plan_id))
}

fn recovery_rescue_files_relative(record: &RecoveryRescueRecord) -> PathBuf {
    recovery_rescue_files_relative_for_transaction(&record.transaction_id)
}

fn recovery_rescue_files_relative_for_transaction(transaction_id: &str) -> PathBuf {
    Path::new("recovery-rescues")
        .join(transaction_id)
        .join("objects")
}

fn recovery_rescue_active_relative(transaction_id: &str) -> PathBuf {
    Path::new("recovery-rescues")
        .join(transaction_id)
        .join("active.json")
}

fn write_recovery_rescue_record(
    project: &ProjectContext,
    record: &RecoveryRescueRecord,
) -> Result<()> {
    validate_transaction_id(&record.transaction_id)?;
    validate_recovery_conflict_plan_id(&record.plan_id)?;
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    repo.write_json_atomic(&recovery_rescue_record_relative(record), record)?;
    repo.write_json_atomic(
        &recovery_rescue_active_relative(&record.transaction_id),
        record,
    )
}

fn read_active_recovery_rescue_record(
    project: &ProjectContext,
    transaction_id: &str,
) -> Result<Option<RecoveryRescueRecord>> {
    let path = project
        .repo_root
        .join(recovery_rescue_active_relative(transaction_id));
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(&path, error)),
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            return Err(CheckPoError::Corruption(format!(
                "recovery rescue record is not a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => {}
    }
    let bytes = crate::storage::AnchoredRoot::open(&project.repo_root)?
        .read_bytes_bounded_path(&path, MAX_RECOVERY_RESCUE_RECORD_BYTES)?;
    let record: RecoveryRescueRecord =
        serde_json::from_slice(&bytes).map_err(|error| crate::json_error(&path, error))?;
    validate_recovery_rescue_record(&record, transaction_id)?;
    Ok(Some(record))
}

fn validate_recovery_rescue_record(
    record: &RecoveryRescueRecord,
    transaction_id: &str,
) -> Result<()> {
    if record.schema_version != RECOVERY_RESCUE_RECORD_SCHEMA_VERSION {
        return Err(CheckPoError::Corruption(format!(
            "unsupported recovery rescue schema version: {}",
            record.schema_version
        )));
    }
    validate_transaction_id(&record.transaction_id)?;
    validate_recovery_conflict_plan_id(&record.plan_id)?;
    if record.transaction_id != transaction_id || record.entries.is_empty() {
        return Err(CheckPoError::Corruption(
            "recovery rescue record identity is invalid".to_string(),
        ));
    }
    let entry_paths = record
        .entries
        .iter()
        .map(|entry| entry.conflict.path.clone())
        .collect::<BTreeSet<_>>();
    if entry_paths.len() != record.entries.len()
        || record
            .completed_paths
            .iter()
            .any(|path| !entry_paths.contains(path))
        || record.completed_paths.iter().collect::<BTreeSet<_>>().len()
            != record.completed_paths.len()
    {
        return Err(CheckPoError::Corruption(
            "recovery rescue record contains invalid paths".to_string(),
        ));
    }
    if record
        .export_directory
        .as_deref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(CheckPoError::Corruption(
            "recovery rescue export path is not absolute".to_string(),
        ));
    }
    Ok(())
}

fn recover_one_with_active_rescue(
    project: &ProjectContext,
    pending: &PendingTransaction,
) -> Result<()> {
    if pending.state != JOURNAL_STATE_UNREADABLE {
        if let Ok(journal) = read_transaction_journal(&pending.journal_path) {
            if journal.intent == TransactionIntent::CompleteToTarget {
                return recover_one(project, pending);
            }
        }
    }
    let Some(mut rescue) = read_active_recovery_rescue_record(project, &pending.transaction_id)?
    else {
        return recover_one(project, pending);
    };
    if rescue.state == RecoveryRescueState::Recovered {
        return recover_one(project, pending);
    }
    let journal = read_valid_recovery_journal(project, pending)?;
    resolve_recovery_conflict_rescue(project, &journal, &mut rescue)?;
    recover_one(project, pending)?;
    rescue.state = RecoveryRescueState::Recovered;
    write_recovery_rescue_record(project, &rescue)
}

fn resolve_recovery_conflict_rescue(
    project: &ProjectContext,
    journal: &TransactionJournal,
    rescue: &mut RecoveryRescueRecord,
) -> Result<()> {
    if rescue.transaction_id != journal.transaction_id
        || rescue.checkpoint_id != journal.checkpoint_id
    {
        return Err(CheckPoError::Corruption(
            "recovery rescue record does not match its transaction journal".to_string(),
        ));
    }
    verify_recovery_rescue_payload(project, rescue)?;
    rescue.state = RecoveryRescueState::Resolving;
    write_recovery_rescue_record(project, rescue)?;
    let before_paths = journal_before_paths(&journal.operations);
    for entry in rescue.entries.clone() {
        if rescue.completed_paths.contains(&entry.conflict.path) {
            continue;
        }
        let operation = journal
            .operations
            .iter()
            .find(|operation| operation.path == entry.conflict.path)
            .ok_or_else(|| {
                CheckPoError::Corruption(format!(
                    "recovery conflict operation is missing for {}",
                    entry.conflict.path
                ))
            })?;
        let current =
            current_file_state_for_recovery(project, &entry.conflict.path, &before_paths)?;
        let matches_before = current.as_ref().map(|state| &state.hash)
            == operation.before_hash.as_ref()
            && current.as_ref().map(|state| state.size_bytes) == operation.before_size_bytes
            && current.as_ref().map(|state| state.modified_at_utc.as_str())
                == operation.before_modified_at_utc.as_deref();
        let matches_rescued = current.as_ref().is_some_and(|state| {
            state.hash == entry.conflict.current_hash
                && state.size_bytes == entry.conflict.size_bytes
                && state.modified_at_utc == entry.conflict.modified_at_utc
        });
        if entry.conflict.metadata_only {
            if !matches_before {
                if !matches_rescued {
                    return Err(CheckPoError::WorkingTreeChanged(
                        entry.conflict.path.to_string(),
                    ));
                }
                restore_before_mtime_for_recovery(project, operation)?;
            }
        } else if !matches_before {
            if current.is_none() {
                // A crash may occur after the identity-bound unlink and before
                // the completion record update. The durable rescue copy makes
                // treating that state as completed safe and repeatable.
            } else if matches_rescued {
                let source = entry
                    .conflict
                    .path
                    .to_project_path(project.project_root.as_path());
                remove_anchored_project_file(project, &source, &entry.conflict.current_hash)?;
            } else {
                return Err(CheckPoError::WorkingTreeChanged(
                    entry.conflict.path.to_string(),
                ));
            }
        }
        rescue.completed_paths.push(entry.conflict.path);
        rescue.completed_paths.sort();
        write_recovery_rescue_record(project, rescue)?;
    }
    Ok(())
}

fn verify_recovery_rescue_payload(
    project: &ProjectContext,
    rescue: &RecoveryRescueRecord,
) -> Result<()> {
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let files_root = recovery_rescue_files_relative(rescue);
    for entry in rescue
        .entries
        .iter()
        .filter(|entry| !entry.conflict.metadata_only)
    {
        let relative = files_root.join(entry.conflict.current_hash.as_str());
        let (parent, leaf) = repo.open_parent(&relative, false)?;
        let mut file = parent.open_file(&leaf).map_err(|error| {
            CheckPoError::Corruption(format!(
                "recovery rescue copy is unavailable for {}: {error}",
                entry.conflict.path
            ))
        })?;
        let hashed = file.hash()?;
        if hashed.object_id != entry.conflict.current_hash
            || hashed.metadata.len() != entry.conflict.size_bytes
        {
            return Err(CheckPoError::Corruption(format!(
                "recovery rescue copy is damaged for {}",
                entry.conflict.path
            )));
        }
        parent.verify_file_binding(&leaf, &file)?;
    }
    repo.verify_root_binding()
}

pub fn quarantine_transaction(
    project_path: impl AsRef<Path>,
    transaction_id: &str,
    options: ApplyOptions,
) -> Result<TransactionQuarantineResult> {
    if !options.yes {
        return Err(crate::user_error("transaction quarantine requires --yes."));
    }
    if transaction_id.len() != 32
        || !transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(crate::user_error("invalid transaction id."));
    }

    let project = crate::load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = crate::acquire_project_repository_lock(&project, "transaction-quarantine")?;
    let tx_root = journals_dir(&project.repo_root).join(transaction_id);
    ensure_regular_transaction_directory(&tx_root)?;

    let journal_path = tx_root.join("journal.json");
    if let Ok(journal) = read_transaction_journal(&journal_path) {
        if validate_transaction_journal_identity(&tx_root, &journal).is_ok()
            && matches!(
                journal.state,
                JournalState::CommittedTarget
                    | JournalState::RolledBack
                    | JournalState::Committed
                    | JournalState::Recovered
            )
        {
            return Err(crate::user_error(
                "completed or recovered transactions cannot be quarantined; run transaction cleanup instead.",
            ));
        }
    }
    let (project_was_verified_in_before_state, mut warnings) =
        inspect_project_before_state(&project, &tx_root, &journal_path);
    if !project_was_verified_in_before_state {
        warnings.push(
            "The Unity project may contain a partially applied transaction. Restore a known good checkpoint before creating a new checkpoint."
                .to_string(),
        );
    }
    let rescue_root = project
        .repo_root
        .join("recovery-rescues")
        .join(transaction_id);
    let transaction_bytes = match dir_size(&tx_root) {
        Ok(size) => size,
        Err(error) => {
            warnings.push(format!(
                "Preserved transaction byte count could not be calculated: {error}"
            ));
            0
        }
    };
    let rescue_bytes = match optional_regular_directory_size(&rescue_root) {
        Ok(size) => size,
        Err(error) => {
            warnings.push(format!(
                "Preserved recovery rescue byte count could not be calculated: {error}"
            ));
            0
        }
    };
    let preserved_bytes = transaction_bytes.saturating_add(rescue_bytes);

    let quarantine_root = project.repo_root.join("quarantined-journals");
    crate::create_dir_all_no_follow(&project.repo_root, &quarantine_root)?;
    crate::sync_parent_dir(&quarantine_root)?;

    let quarantine_name = format!("{transaction_id}-{}", Uuid::new_v4().simple());
    let quarantine_path = quarantine_root.join(&quarantine_name);
    let record_path = quarantine_root.join(format!("{quarantine_name}.json"));
    write_quarantine_json(
        &project.repo_root,
        &record_path,
        &QuarantineRecord {
            schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
            transaction_id: transaction_id.to_string(),
            quarantined_at_utc: crate::now_utc_string(),
            original_journal_path: journal_path,
            project_was_verified_in_before_state,
            resolved_at_utc: None,
            resolved_checkpoint_id: None,
        },
    )?;
    if let Err(error) = move_repo_directory_anchored(&project.repo_root, &tx_root, &quarantine_path)
    {
        let _ = remove_repo_file_if_exists_anchored(&project.repo_root, &record_path);
        return Err(error);
    }
    if let Err(error) =
        move_recovery_rescue_into_quarantine(&project, transaction_id, &quarantine_path)
    {
        warnings.push(format!(
            "Recovery rescue data remains in CheckPo storage because it could not be bundled into the quarantine: {error}"
        ));
    }

    crate::diagnostics::log_warning(
        "transaction-quarantine",
        &format!(
            "transaction {transaction_id} was preserved at {}",
            quarantine_path.display()
        ),
    );
    Ok(TransactionQuarantineResult {
        transaction_id: transaction_id.to_string(),
        quarantine_path,
        preserved_bytes,
        warnings,
    })
}

pub(super) fn resolve_unverified_transaction_quarantines_unlocked(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
) -> Result<usize> {
    let Some((quarantine_root, record_paths)) = quarantine_record_paths(project)? else {
        return Ok(0);
    };
    let resolved_at_utc = crate::now_utc_string();
    let mut resolved_count = 0;
    for record_path in record_paths {
        let metadata = fs::symlink_metadata(&record_path)
            .map_err(|error| crate::io_error(&record_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            continue;
        }
        if quarantine_record_has_valid_resolution(&record_path)? {
            continue;
        }
        write_quarantine_resolution(
            &project.repo_root,
            &record_path,
            checkpoint_id,
            &resolved_at_utc,
        )?;
        resolved_count += 1;
    }
    for entry in
        fs::read_dir(&quarantine_root).map_err(|error| crate::io_error(&quarantine_root, error))?
    {
        let entry = entry.map_err(|error| crate::io_error(&quarantine_root, error))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let record_path = quarantine_root.join(format!("{name}.json"));
        if fs::symlink_metadata(&record_path).is_ok() {
            continue;
        }
        write_quarantine_json(
            &project.repo_root,
            &record_path,
            &QuarantineRecord {
                schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
                transaction_id: name.split('-').next().unwrap_or(&name).to_string(),
                quarantined_at_utc: resolved_at_utc.clone(),
                original_journal_path: entry.path().join("journal.json"),
                project_was_verified_in_before_state: false,
                resolved_at_utc: None,
                resolved_checkpoint_id: None,
            },
        )?;
        write_quarantine_resolution(
            &project.repo_root,
            &record_path,
            checkpoint_id,
            &resolved_at_utc,
        )?;
        resolved_count += 1;
    }
    Ok(resolved_count)
}

fn write_quarantine_resolution(
    repo_root: &Path,
    record_path: &Path,
    checkpoint_id: &SnapshotId,
    resolved_at_utc: &str,
) -> Result<()> {
    let anchored_repo = crate::storage::AnchoredRoot::open(repo_root)?;
    let bytes = anchored_repo.read_bytes_bounded_path(record_path, MAX_QUARANTINE_RECORD_BYTES)?;
    write_quarantine_json(
        repo_root,
        &quarantine_resolution_path(record_path),
        &QuarantineResolutionRecord {
            schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
            resolved_at_utc: resolved_at_utc.to_string(),
            resolved_checkpoint_id: checkpoint_id.clone(),
            quarantine_record_digest: blake3::hash(&bytes).to_hex().to_string(),
        },
    )
}

fn write_quarantine_json<T: serde::Serialize>(
    repo_root: &Path,
    path: &Path,
    value: &T,
) -> Result<()> {
    crate::storage::AnchoredRoot::open(repo_root)?.write_json_atomic_path(path, value)
}

fn quarantine_resolution_path(record_path: &Path) -> PathBuf {
    record_path.with_extension("resolved")
}

fn quarantine_record_has_valid_resolution(record_path: &Path) -> Result<bool> {
    let resolution_path = quarantine_resolution_path(record_path);
    let metadata = match fs::symlink_metadata(&resolution_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(crate::io_error(&resolution_path, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Ok(false);
    }
    let repo_root = quarantine_repo_root_for_record_path(record_path)?;
    let anchored_repo = crate::storage::AnchoredRoot::open(repo_root)?;
    let resolution =
        match anchored_repo.read_json_path::<QuarantineResolutionRecord>(&resolution_path) {
            Ok(resolution) if resolution.schema_version == QUARANTINE_RECORD_SCHEMA_VERSION_V1 => {
                resolution
            }
            Ok(_) | Err(_) => return Ok(false),
        };
    let record_bytes =
        anchored_repo.read_bytes_bounded_path(record_path, MAX_QUARANTINE_RECORD_BYTES)?;
    Ok(resolution.quarantine_record_digest == blake3::hash(&record_bytes).to_hex().as_str())
}

fn quarantine_record_paths(project: &ProjectContext) -> Result<Option<(PathBuf, Vec<PathBuf>)>> {
    let quarantine_root = project.repo_root.join("quarantined-journals");
    let root_metadata = match fs::symlink_metadata(&quarantine_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(&quarantine_root, error)),
    };
    if crate::metadata_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction quarantine root is not a regular directory: {}",
            quarantine_root.display()
        )));
    }
    let mut record_paths = Vec::new();
    for entry in
        fs::read_dir(&quarantine_root).map_err(|error| crate::io_error(&quarantine_root, error))?
    {
        let entry = entry.map_err(|error| crate::io_error(&quarantine_root, error))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            record_paths.push(path);
        }
    }
    record_paths.sort();
    Ok(Some((quarantine_root, record_paths)))
}

fn read_quarantine_record(path: &Path) -> Result<QuarantineRecord> {
    let repo_root = quarantine_repo_root_for_record_path(path)?;
    let bytes = crate::storage::AnchoredRoot::open(repo_root)?
        .read_bytes_bounded_path(path, MAX_QUARANTINE_RECORD_BYTES)?;
    let envelope: QuarantineRecordEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))?;
    if envelope.schema_version > QUARANTINE_RECORD_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction quarantine record schema".to_string(),
            found: envelope.schema_version,
            supported: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
        });
    }
    if envelope.schema_version != QUARANTINE_RECORD_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::Corruption(format!(
            "invalid transaction quarantine record schema: {}",
            envelope.schema_version
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))
}

fn quarantine_repo_root_for_record_path(path: &Path) -> Result<&Path> {
    let quarantine_root = path.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "quarantine record has no parent: {}",
            path.display()
        ))
    })?;
    if quarantine_root.file_name() != Some(std::ffi::OsStr::new("quarantined-journals")) {
        return Err(CheckPoError::Corruption(format!(
            "quarantine record is outside the canonical repository namespace: {}",
            path.display()
        )));
    }
    quarantine_root.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "quarantine record has no repository root: {}",
            path.display()
        ))
    })
}

fn inspect_project_before_state(
    project: &ProjectContext,
    tx_root: &Path,
    journal_path: &Path,
) -> (bool, Vec<String>) {
    let result = (|| -> Result<bool> {
        let metadata = fs::symlink_metadata(journal_path)
            .map_err(|error| crate::io_error(journal_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "transaction journal is not a regular file: {}",
                journal_path.display()
            )));
        }
        let journal = read_transaction_journal(journal_path)?;
        validate_transaction_journal_identity(tx_root, &journal)?;
        if journal.operations.is_empty() {
            return Err(CheckPoError::Corruption(
                "transaction journal contains no operations".to_string(),
            ));
        }
        for operation in &journal.operations {
            let current = current_file_state(project, &operation.path)?;
            if current.as_ref().map(|state| &state.hash) != operation.before_hash.as_ref()
                || current.as_ref().map(|state| state.size_bytes) != operation.before_size_bytes
                || current.as_ref().map(|state| state.modified_at_utc.as_str())
                    != operation.before_modified_at_utc.as_deref()
            {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    match result {
        Ok(is_before) => (is_before, Vec::new()),
        Err(error) => (
            false,
            vec![format!(
                "Transaction state could not be verified before quarantine: {error}"
            )],
        ),
    }
}

fn recover_one(project: &ProjectContext, pending: &PendingTransaction) -> Result<()> {
    let tx_root = pending
        .journal_path
        .parent()
        .ok_or_else(|| CheckPoError::Corruption("invalid journal path".into()))?;
    ensure_regular_transaction_directory(tx_root)?;
    if pending.state == JOURNAL_STATE_UNREADABLE {
        let quarantine_path = quarantine_unknown_transaction_locked(
            project,
            tx_root,
            &pending.journal_path,
            &pending.transaction_id,
            "transaction journal is unreadable",
        )?;
        return Err(CheckPoError::Corruption(format!(
            "unreadable transaction was quarantined at {}",
            quarantine_path.display()
        )));
    }
    match fs::symlink_metadata(&pending.journal_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            let quarantine_path = quarantine_unknown_transaction_locked(
                project,
                tx_root,
                &pending.journal_path,
                &pending.transaction_id,
                "transaction journal is not a regular file",
            )?;
            return Err(CheckPoError::Corruption(format!(
                "transaction with a non-regular journal was quarantined at {}",
                quarantine_path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let quarantine_path = quarantine_unknown_transaction_locked(
                project,
                tx_root,
                &pending.journal_path,
                &pending.transaction_id,
                "transaction journal is missing",
            )?;
            return Err(CheckPoError::Corruption(format!(
                "transaction with a missing journal was quarantined at {}",
                quarantine_path.display()
            )));
        }
        Err(error) => return Err(crate::io_error(&pending.journal_path, error)),
    }
    let mut journal = read_transaction_journal(&pending.journal_path)?;
    validate_transaction_journal_identity(tx_root, &journal)?;
    if journal.intent == TransactionIntent::CompleteToTarget {
        return recover_complete_to_target(project, pending, tx_root, journal);
    }
    if journal.operations.is_empty() {
        return Err(CheckPoError::Corruption(
            "transaction journal contains no operations".to_string(),
        ));
    }
    validate_journal_operations(project, &journal.checkpoint_id, &journal.operations)?;
    validate_journal_directory_topology(
        &journal.operations,
        &journal.directories_to_remove,
        &journal.directories_to_create,
    )?;
    let backup_root = tx_root.join("backup");
    let staged_root = tx_root.join("staged");
    let backup_paths = validate_transaction_payload(
        &backup_root,
        journal
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Delete | FileOperationType::Replace
                )
            })
            .map(|operation| operation.path.clone())
            .collect(),
    )?;
    let staged_paths = validate_transaction_payload(
        &staged_root,
        journal
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Restore | FileOperationType::Replace
                )
            })
            .map(|operation| operation.path.clone())
            .collect(),
    )?;

    cleanup_transaction_materialization_temps(project, &journal)?;

    if transaction_needs_rollback(project, &journal, &backup_paths, &staged_paths)? {
        invalidate_operation_fingerprints(project, &journal.operations)?;
        recover_topology_transaction(project, &backup_root, &journal)?;
    }
    remove_repository_tree_if_exists(&project.repo_root, &staged_root)?;
    journal.state = JournalState::Recovered;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&pending.journal_path, &journal)?;
    Ok(())
}

fn recover_complete_to_target(
    project: &ProjectContext,
    pending: &PendingTransaction,
    tx_root: &Path,
    mut journal: TransactionJournal,
) -> Result<()> {
    super::unity_guard::ensure_unity_editor_is_closed(project)?;
    if journal.operations.is_empty() {
        return Err(CheckPoError::Corruption(
            "complete-to-target journal contains no original operations".to_string(),
        ));
    }
    validate_journal_operations(project, &journal.checkpoint_id, &journal.operations)?;
    validate_journal_directory_topology(
        &journal.operations,
        &journal.directories_to_remove,
        &journal.directories_to_create,
    )?;
    validate_transaction_payload_unscoped(&tx_root.join("backup"))?;
    validate_transaction_payload_unscoped(&tx_root.join("staged"))?;
    cleanup_transaction_materialization_temps(project, &journal)?;
    match journal.state {
        JournalState::Created | JournalState::Staged => {
            remove_repository_tree_if_exists(&project.repo_root, &tx_root.join("staged"))?;
            journal.state = JournalState::RolledBack;
            journal.updated_at_utc = crate::now_utc_string();
            write_journal(&pending.journal_path, &journal)?;
            return Ok(());
        }
        JournalState::ApplyingTarget
        | JournalState::VerifyingTarget
        | JournalState::AwaitingUnity => {
            remove_repository_tree_if_exists(&project.repo_root, &tx_root.join("staged"))?;
            remove_repository_tree_if_exists(&project.repo_root, &tx_root.join("forward-staged"))?;
        }
        JournalState::CommittedTarget | JournalState::RolledBack => return Ok(()),
        state => {
            return Err(CheckPoError::Corruption(format!(
                "complete-to-target transaction has incompatible state: {state:?}"
            )))
        }
    }

    let started = std::time::Instant::now();
    let mut last_retryable_error = None;
    for round in 0..TARGET_RECONCILE_MAX_ROUNDS {
        if started.elapsed() >= TARGET_RECONCILE_MAX_ELAPSED {
            break;
        }
        let plan = super::plan::build_plan(
            project,
            journal.checkpoint_id.clone(),
            journal.kind,
            journal.selected_paths.as_deref(),
        )?;
        if !plan.warnings.is_empty() {
            return Err(crate::user_error(format!(
                "target verification cannot continue while scan warnings exist: {}",
                plan.warnings.join("; ")
            )));
        }
        if !plan.has_changes {
            journal.state = JournalState::VerifyingTarget;
            journal.updated_at_utc = crate::now_utc_string();
            write_journal(&pending.journal_path, &journal)?;
            match super::unity_guard::verify_target_is_stable(
                project,
                &journal.checkpoint_id,
                journal.kind,
                journal.selected_paths.as_deref(),
            ) {
                Ok(()) => {
                    remove_repository_tree_if_exists(&project.repo_root, &tx_root.join("staged"))?;
                    remove_repository_tree_if_exists(
                        &project.repo_root,
                        &tx_root.join("forward-staged"),
                    )?;
                    journal.state = JournalState::CommittedTarget;
                    journal.updated_at_utc = crate::now_utc_string();
                    write_journal(&pending.journal_path, &journal)?;
                    if let Err(error) =
                        invalidate_operation_fingerprints(project, &journal.operations)
                    {
                        crate::diagnostics::log_warning(
                            "transaction-recovery",
                            &format!(
                                "transaction {} reached its target state, but fingerprint cache invalidation failed; the cache will be rebuilt when needed: {error}",
                                pending.transaction_id
                            ),
                        );
                    }
                    return Ok(());
                }
                Err(error) if target_reconcile_error_is_retryable(&error) => {
                    last_retryable_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            }
        }

        validate_journal_operations(project, &journal.checkpoint_id, &plan.operations)?;
        validate_journal_directory_topology(
            &plan.operations,
            &plan.directories_to_remove,
            &plan.directories_to_create,
        )?;
        journal.operations = plan.operations;
        journal.directories_to_remove = plan.directories_to_remove;
        journal.directories_to_create = plan.directories_to_create;
        journal.state = JournalState::ApplyingTarget;
        journal.updated_at_utc = crate::now_utc_string();
        write_journal(&pending.journal_path, &journal)?;
        invalidate_operation_fingerprints(project, &journal.operations)?;

        match apply_target_reconcile_round(project, tx_root, &journal, round) {
            Ok(()) => {
                last_retryable_error = None;
            }
            Err(error) if target_reconcile_error_is_retryable(&error) => {
                last_retryable_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(75));
            }
            Err(error) => return Err(error),
        }
    }

    journal.state = JournalState::AwaitingUnity;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&pending.journal_path, &journal)?;
    Err(CheckPoError::WorkingTreeChanged(format!(
        "Unity is still updating the project; close Unity and continue recovery{}",
        last_retryable_error
            .as_ref()
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    )))
}

fn apply_target_reconcile_round(
    project: &ProjectContext,
    tx_root: &Path,
    journal: &TransactionJournal,
    round: usize,
) -> Result<()> {
    let round_root = tx_root
        .join("forward-staged")
        .join(format!("{round}-{}", Uuid::new_v4().simple()));
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut stage_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    let target_operations = journal
        .operations
        .iter()
        .filter(|operation| operation.after_hash.is_some())
        .collect::<Vec<_>>();
    let mut destination_parents = BTreeSet::new();
    for operation in &target_operations {
        let destination = staged_path(&round_root, &operation.path);
        let parent = destination.parent().ok_or_else(|| {
            CheckPoError::Corruption(format!(
                "forward stage destination has no parent: {}",
                destination.display()
            ))
        })?;
        destination_parents.insert(parent.to_path_buf());
    }
    for parent in destination_parents {
        prepare_stage_destination_parent(project, &anchored_repo, &parent, &mut stage_sync_batch)?;
    }
    for operation in &target_operations {
        stage_object_for_transaction_prepared(
            project,
            &anchored_repo,
            required_after_hash(operation)?,
            &staged_path(&round_root, &operation.path),
            operation.after_size_bytes.ok_or_else(|| {
                CheckPoError::Corruption(format!(
                    "target operation missing size for {}",
                    operation.path
                ))
            })?,
            operation.after_modified_at_utc.as_deref(),
            &mut stage_sync_batch,
            None,
        )?;
    }
    stage_sync_batch.flush()?;
    anchored_repo.verify_root_binding()?;

    let recovery_scope_paths = journal_before_paths(&journal.operations);
    let mut conflicts = Vec::new();
    for operation in &journal.operations {
        let Some(current) =
            current_file_state_for_recovery(project, &operation.path, &recovery_scope_paths)?
        else {
            continue;
        };
        let matches_target = operation.after_hash.as_ref() == Some(&current.hash)
            && operation.after_size_bytes == Some(current.size_bytes);
        if operation.after_hash.is_none() || !matches_target {
            conflicts.push(TransactionRecoveryConflict {
                path: operation.path.clone(),
                current_hash: current.hash,
                size_bytes: current.size_bytes,
                modified_at_utc: current.modified_at_utc,
                metadata_only: false,
            });
        }
    }
    if !conflicts.is_empty() {
        preserve_and_remove_target_conflicts(project, journal, conflicts)?;
    }

    for directory in &journal.directories_to_remove {
        remove_project_directory(project, directory)?;
    }
    for directory in &journal.directories_to_create {
        create_project_directory(project, directory)?;
    }

    let mut project_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    let mut staged_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    for operation in target_operations {
        let current =
            current_file_state_for_recovery(project, &operation.path, &recovery_scope_paths)?;
        let matches_target = current.as_ref().is_some_and(|state| {
            operation.after_hash.as_ref() == Some(&state.hash)
                && operation.after_size_bytes == Some(state.size_bytes)
        });
        if matches_target {
            set_project_file_mtime_to_target(project, operation)?;
            continue;
        }
        if current.is_some() {
            return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
        }
        let mut publish_operation = (*operation).clone();
        publish_operation.operation_type = FileOperationType::Restore;
        publish_operation.before_hash = None;
        publish_operation.before_size_bytes = None;
        publish_operation.before_modified_at_utc = None;
        restore_new_staged_file_to_project_deferred(
            project,
            &publish_operation,
            &staged_path(&round_root, &operation.path),
            &operation
                .path
                .to_project_path(project.project_root.as_path()),
            &journal.transaction_id,
            &mut staged_sync_batch,
            &mut project_sync_batch,
        )?;
    }
    project_sync_batch.flush()?;
    staged_sync_batch.flush()?;
    remove_repository_tree_if_exists(&project.repo_root, &round_root)?;
    Ok(())
}

fn preserve_and_remove_target_conflicts(
    project: &ProjectContext,
    journal: &TransactionJournal,
    mut conflicts: Vec<TransactionRecoveryConflict>,
) -> Result<()> {
    conflicts.sort_by(|left, right| left.path.cmp(&right.path));
    let journal_bytes =
        serde_json::to_vec(journal).map_err(|error| CheckPoError::Corruption(error.to_string()))?;
    let journal_digest = blake3::hash(&journal_bytes).to_hex().to_string();
    let plan_id = recovery_conflict_plan_id(
        &project.project_id,
        &journal.transaction_id,
        &journal.checkpoint_id,
        &journal_digest,
        &conflicts,
    )?;
    let plan = TransactionRecoveryConflictPlan {
        schema_version: crate::TRANSACTION_RECOVERY_CONFLICT_PLAN_SCHEMA_VERSION,
        plan_id,
        transaction_id: journal.transaction_id.clone(),
        checkpoint_id: journal.checkpoint_id.clone(),
        conflicts,
    };
    prepare_recovery_conflict_rescue(project, journal, &plan, &BTreeSet::new(), None)?;
    let mut rescue = read_active_recovery_rescue_record(project, &journal.transaction_id)?
        .ok_or_else(|| {
            CheckPoError::Corruption("target reconcile rescue record was not published".to_string())
        })?;
    verify_recovery_rescue_payload(project, &rescue)?;
    rescue.state = RecoveryRescueState::Resolving;
    write_recovery_rescue_record(project, &rescue)?;
    let target_paths = journal
        .operations
        .iter()
        .filter(|operation| operation.after_hash.is_some())
        .map(|operation| operation.path.clone())
        .collect::<BTreeSet<_>>();
    for entry in rescue.entries.clone() {
        if rescue.completed_paths.contains(&entry.conflict.path) {
            continue;
        }
        let current =
            current_file_state_for_recovery(project, &entry.conflict.path, &target_paths)?;
        match current {
            None => {}
            Some(current)
                if current.hash == entry.conflict.current_hash
                    && current.size_bytes == entry.conflict.size_bytes
                    && current.modified_at_utc == entry.conflict.modified_at_utc =>
            {
                let source = entry
                    .conflict
                    .path
                    .to_project_path(project.project_root.as_path());
                remove_anchored_project_file(project, &source, &entry.conflict.current_hash)?;
            }
            Some(_) => {
                return Err(CheckPoError::WorkingTreeChanged(
                    entry.conflict.path.to_string(),
                ))
            }
        }
        rescue.completed_paths.push(entry.conflict.path);
        rescue.completed_paths.sort();
        write_recovery_rescue_record(project, &rescue)?;
    }
    rescue.state = RecoveryRescueState::Recovered;
    write_recovery_rescue_record(project, &rescue)
}

fn target_reconcile_error_is_retryable(error: &CheckPoError) -> bool {
    match error {
        CheckPoError::WorkingTreeChanged(_) => true,
        CheckPoError::Io { source, .. } => matches!(
            source.kind(),
            ErrorKind::PermissionDenied
                | ErrorKind::WouldBlock
                | ErrorKind::AlreadyExists
                | ErrorKind::NotFound
                | ErrorKind::NotADirectory
                | ErrorKind::DirectoryNotEmpty
        ),
        _ => false,
    }
}

fn remove_repository_tree_if_exists(repo_root: &Path, directory: &Path) -> Result<()> {
    let relative = directory.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "transaction cleanup is outside repository: {}",
            directory.display()
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(repo_root)?;
    let (parent, leaf) = match root.open_parent_for_mutation(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    let tree = match parent.open_directory_for_mutation(&leaf) {
        Ok(tree) => tree,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    tree.remove_tree_contents()?;
    drop(tree);
    parent.unlink_dir(&leaf)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

fn cleanup_transaction_materialization_temps(
    project: &ProjectContext,
    journal: &TransactionJournal,
) -> Result<()> {
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    for operation in journal.operations.iter().filter(|operation| {
        matches!(
            operation.operation_type,
            FileOperationType::Restore | FileOperationType::Replace
        )
    }) {
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        let temporary = transaction_materialization_temp_path(
            &destination,
            &operation.path,
            &journal.transaction_id,
        )?;
        let relative = temporary
            .strip_prefix(project.project_root.as_path())
            .map_err(|_| {
                CheckPoError::Corruption(format!(
                    "transaction materialization temp is outside project: {}",
                    temporary.display()
                ))
            })?;
        let (parent, leaf) = match anchored_project.open_parent_for_mutation(relative, false) {
            Ok(value) => value,
            Err(CheckPoError::Io { source, .. })
                if matches!(
                    source.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        match parent.open_file(&leaf) {
            Ok(file) => {
                parent.unlink_file_if_bound(&leaf, file)?;
                parent.sync_all()?;
            }
            Err(CheckPoError::Corruption(_)) => {
                return Err(CheckPoError::Corruption(format!(
                    "transaction materialization temp is not a regular file: {}",
                    temporary.display()
                )))
            }
            Err(CheckPoError::Io { source, .. })
                if matches!(
                    source.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error),
        }
    }
    anchored_project.verify_root_binding()
}

fn ensure_regular_transaction_directory(tx_root: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(tx_root).map_err(|error| crate::io_error(tx_root, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction root is not a regular directory: {}",
            tx_root.display()
        )));
    }
    Ok(())
}

fn validate_transaction_payload(
    root: &Path,
    allowed_paths: BTreeSet<TrackedUnityFilePath>,
) -> Result<BTreeSet<TrackedUnityFilePath>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(crate::io_error(root, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction payload root is not a regular directory: {}",
            root.display()
        )));
    }

    let mut present = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| CheckPoError::Corruption(error.to_string()))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        let file_type = metadata.file_type();
        if crate::metadata_is_link_or_reparse(&metadata) {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a symlink: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            continue;
        }
        if !file_type.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a non-regular file: {}",
                entry.path().display()
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| CheckPoError::Corruption(error.to_string()))?;
        let relative = relative.to_str().ok_or_else(|| {
            CheckPoError::Corruption(format!(
                "transaction payload path is not valid UTF-8: {}",
                entry.path().display()
            ))
        })?;
        let path = TrackedUnityFilePath::parse(&relative.replace('\\', "/"))?;
        if allowed_paths.contains(&path) {
            present.insert(path);
            continue;
        }
        if crate::is_checkpo_atomic_materialization_temporary_file(entry.path()) {
            continue;
        }
        return Err(CheckPoError::Corruption(format!(
            "transaction payload contains an unexpected path: {path}"
        )));
    }
    Ok(present)
}

fn validate_transaction_payload_unscoped(root: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::io_error(root, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction payload root is not a regular directory: {}",
            root.display()
        )));
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| CheckPoError::Corruption(error.to_string()))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&metadata) {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a symlink: {}",
                entry.path().display()
            )));
        }
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a non-regular file: {}",
                entry.path().display()
            )));
        }
        if crate::is_checkpo_atomic_materialization_temporary_file(entry.path()) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| CheckPoError::Corruption(error.to_string()))?
            .to_str()
            .ok_or_else(|| {
                CheckPoError::Corruption(format!(
                    "transaction payload path is not valid UTF-8: {}",
                    entry.path().display()
                ))
            })?
            .replace('\\', "/");
        TrackedUnityFilePath::parse(&relative)?;
    }
    Ok(())
}

fn transaction_needs_rollback(
    project: &ProjectContext,
    journal: &TransactionJournal,
    backup_paths: &BTreeSet<TrackedUnityFilePath>,
    staged_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<bool> {
    if journal.state == JournalState::Applying || !backup_paths.is_empty() {
        return Ok(true);
    }
    if journal.state != JournalState::Staged {
        return Ok(false);
    }
    for operation in &journal.operations {
        let Some(after_hash) = operation.after_hash.as_ref() else {
            continue;
        };
        if !staged_paths.contains(&operation.path)
            && current_hash(project, &operation.path)?.as_ref() == Some(after_hash)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_before_state_restored(
    project: &ProjectContext,
    operations: &[FileOperation],
) -> Result<()> {
    for operation in operations {
        let current = current_file_state_for_recovery(
            project,
            &operation.path,
            &journal_before_paths(operations),
        )?;
        if current.as_ref().map(|state| &state.hash) != operation.before_hash.as_ref()
            || current.as_ref().map(|state| state.size_bytes) != operation.before_size_bytes
            || current.as_ref().map(|state| state.modified_at_utc.as_str())
                != operation.before_modified_at_utc.as_deref()
        {
            return Err(CheckPoError::Corruption(format!(
                "transaction recovery did not restore before state for {}",
                operation.path
            )));
        }
    }
    Ok(())
}

fn journal_before_paths(operations: &[FileOperation]) -> BTreeSet<TrackedUnityFilePath> {
    operations
        .iter()
        .filter(|operation| operation.before_hash.is_some())
        .map(|operation| operation.path.clone())
        .collect()
}

fn recover_topology_transaction(
    project: &ProjectContext,
    backup_root: &Path,
    journal: &TransactionJournal,
) -> Result<()> {
    let before_paths = journal_before_paths(&journal.operations);
    let transaction_root = backup_root.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!("invalid backup root: {}", backup_root.display()))
    })?;
    let recovery_after_root = transaction_root.join("recovery-after");
    for operation in journal.operations.iter().filter(|operation| {
        matches!(
            operation.operation_type,
            FileOperationType::Restore | FileOperationType::Replace
        )
    }) {
        let Some(after_hash) = operation.after_hash.as_ref() else {
            continue;
        };
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        remove_existing_held_after_file(
            project,
            operation,
            &destination,
            &journal.transaction_id,
            after_hash,
            &recovery_after_root,
        )?;
        match current_hash_for_recovery(project, &operation.path, &before_paths)? {
            Some(current) if &current == after_hash => {
                remove_after_file_for_recovery(
                    project,
                    operation,
                    &destination,
                    &journal.transaction_id,
                    after_hash,
                    &recovery_after_root,
                )?;
            }
            Some(current) if operation.before_hash.as_ref() == Some(&current) => {}
            None => {}
            Some(_) => return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string())),
        }
    }

    let mut created_directories = journal.directories_to_create.clone();
    created_directories.sort_by(|left, right| {
        right
            .as_str()
            .matches('/')
            .count()
            .cmp(&left.as_str().matches('/').count())
            .then_with(|| left.cmp(right))
    });
    for directory in &created_directories {
        let path = directory.to_project_path(project.project_root.as_path());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
                remove_project_directory(project, directory)?;
            }
            Ok(metadata)
                if metadata.is_file()
                    && !crate::metadata_is_link_or_reparse(&metadata)
                    && before_paths.contains(directory) => {}
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Ok(_) => return Err(CheckPoError::WorkingTreeChanged(directory.to_string())),
            Err(error) => return Err(crate::io_error(&path, error)),
        }
    }

    let mut removed_directories = journal.directories_to_remove.clone();
    removed_directories.sort();
    for directory in &removed_directories {
        ensure_project_directory_exists_for_recovery(project, directory)?;
    }

    let mut backed_up = journal
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.operation_type,
                FileOperationType::Delete | FileOperationType::Replace
            )
        })
        .collect::<Vec<_>>();
    backed_up.sort_by(|left, right| left.path.cmp(&right.path));
    for operation in backed_up {
        recover_before_file(
            project,
            backup_root,
            operation,
            &before_paths,
            &journal.transaction_id,
        )?;
    }
    for operation in journal
        .operations
        .iter()
        .filter(|operation| operation.operation_type == FileOperationType::SetMetadata)
    {
        recover_project_file_mtime(project, operation)?;
    }
    ensure_before_state_restored(project, &journal.operations)
}

fn recovery_after_path(
    destination: &Path,
    path: &TrackedUnityFilePath,
    transaction_id: &str,
) -> Result<PathBuf> {
    destination.file_name().ok_or_else(|| {
        CheckPoError::InvalidTrackedPath(format!("invalid path: {}", destination.display()))
    })?;
    let digest = blake3::hash(path.as_str().as_bytes()).to_hex();
    let held_name = format!(".checkpo-r-{}-{transaction_id}.tmp", &digest[..16]);
    Ok(destination.with_file_name(held_name))
}

fn existing_held_after_file(
    project: &ProjectContext,
    destination: &Path,
    path: &TrackedUnityFilePath,
    transaction_id: &str,
    expected_hash: &ObjectId,
) -> Result<Option<PathBuf>> {
    let held = recovery_after_path(destination, path, transaction_id)?;
    let relative = held
        .strip_prefix(project.project_root.as_path())
        .map_err(|_| {
            CheckPoError::Corruption(format!("invalid recovery quarantine: {}", held.display()))
        })?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (parent, leaf) = match root.open_parent(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. })
            if matches!(
                source.kind(),
                ErrorKind::NotFound | ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let mut file = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(CheckPoError::Corruption(_)) => {
            return Err(CheckPoError::Corruption(format!(
                "recovery quarantine is not a regular file: {}",
                held.display()
            )))
        }
        Err(error) => return Err(error),
    };
    let actual = file.hash()?.object_id;
    if &actual != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            held.display(),
            expected_hash,
            actual
        )));
    }
    parent.verify_file_binding(&leaf, &file)?;
    root.verify_root_binding()?;
    Ok(Some(held))
}

fn preserve_after_file_for_recovery(
    project: &ProjectContext,
    source: &Path,
    expected_hash: &ObjectId,
    recovery_after_root: &Path,
) -> Result<()> {
    let preserved = recovery_after_root.join(expected_hash.as_str());
    let preserved_relative = preserved.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "recovery copy is outside repository: {}",
            preserved.display()
        ))
    })?;
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let (preserved_parent, preserved_leaf) =
        repo.open_parent_for_mutation(preserved_relative, true)?;
    match preserved_parent.open_file(&preserved_leaf) {
        Ok(mut file) => {
            let actual = file.hash()?.object_id;
            if &actual != expected_hash {
                return Err(CheckPoError::ObjectHashMismatch(format!(
                    "{} expected {}, got {}",
                    preserved.display(),
                    expected_hash,
                    actual
                )));
            }
            preserved_parent.verify_file_binding(&preserved_leaf, &file)?;
            return repo.verify_root_binding();
        }
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {}
        Err(CheckPoError::Corruption(_)) => {
            return Err(CheckPoError::Corruption(format!(
                "recovery copy is not a regular file: {}",
                preserved.display()
            )))
        }
        Err(error) => return Err(error),
    }

    let temporary_leaf = std::ffi::OsString::from(format!(
        ".checkpo-preserve-{}.tmp",
        &expected_hash.as_str()[..16]
    ));
    if let Ok(file) = preserved_parent.open_file(&temporary_leaf) {
        preserved_parent.unlink_file_if_bound(&temporary_leaf, file)?;
        preserved_parent.sync_all()?;
    }
    let source_relative = source
        .strip_prefix(project.project_root.as_path())
        .map_err(|_| {
            CheckPoError::Corruption(format!(
                "recovery source is outside project: {}",
                source.display()
            ))
        })?;
    let project_root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (source_parent, source_leaf) = project_root.open_parent(source_relative, false)?;
    let mut source_file = source_parent.open_file(&source_leaf)?;
    let source_hash = source_file.hash()?;
    if &source_hash.object_id != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            source.display(),
            expected_hash,
            source_hash.object_id
        )));
    }
    let mut output = preserved_parent.create_new_file(&temporary_leaf)?;
    let result = (|| -> Result<()> {
        let copied = source_file.copy_and_hash_to(&mut output, &preserved)?;
        if &copied.object_id != expected_hash {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery preservation copy mismatch: {}",
                source.display()
            )));
        }
        output.sync_all()?;
        let readback = output.hash()?;
        if &readback.object_id != expected_hash {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery preservation readback mismatch: {}",
                preserved.display()
            )));
        }
        source_parent.verify_file_binding(&source_leaf, &source_file)?;
        preserved_parent.rename_no_replace_to(
            &temporary_leaf,
            &output,
            &preserved_parent,
            &preserved_leaf,
        )?;
        preserved_parent.sync_all()?;
        project_root.verify_root_binding()?;
        repo.verify_root_binding()
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let cleanup_leaf = if preserved_parent
                .verify_file_binding(&preserved_leaf, &output)
                .is_ok()
            {
                preserved_leaf.as_os_str()
            } else {
                temporary_leaf.as_os_str()
            };
            let _ = preserved_parent.unlink_file_if_bound(cleanup_leaf, output);
            Err(error)
        }
    }
}

fn remove_existing_held_after_file(
    project: &ProjectContext,
    operation: &FileOperation,
    destination: &Path,
    transaction_id: &str,
    expected_hash: &ObjectId,
    recovery_after_root: &Path,
) -> Result<()> {
    let Some(held) = existing_held_after_file(
        project,
        destination,
        &operation.path,
        transaction_id,
        expected_hash,
    )?
    else {
        return Ok(());
    };
    preserve_after_file_for_recovery(project, &held, expected_hash, recovery_after_root)?;
    remove_anchored_project_file(project, &held, expected_hash)
}

fn remove_anchored_project_file(
    project: &ProjectContext,
    path: &Path,
    expected_hash: &ObjectId,
) -> Result<()> {
    let relative = path
        .strip_prefix(project.project_root.as_path())
        .map_err(|_| {
            CheckPoError::Corruption(format!(
                "recovery removal is outside project: {}",
                path.display()
            ))
        })?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (parent, leaf) = root.open_parent_for_mutation(relative, false)?;
    let mut file = parent.open_file(&leaf)?;
    let hashed = file.hash()?;
    if &hashed.object_id != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected_hash,
            hashed.object_id
        )));
    }
    parent.verify_file_binding(&leaf, &file)?;
    #[cfg(windows)]
    {
        file = parent.open_file_without_write_sharing(&leaf, &file)?;
        file.verify_version(&hashed.version)?;
    }
    root.verify_root_binding()?;
    parent.unlink_file_if_bound_versioned(&leaf, file, hashed.version)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

fn remove_after_file_for_recovery(
    project: &ProjectContext,
    operation: &FileOperation,
    destination: &Path,
    transaction_id: &str,
    expected_hash: &ObjectId,
    recovery_after_root: &Path,
) -> Result<()> {
    if existing_held_after_file(
        project,
        destination,
        &operation.path,
        transaction_id,
        expected_hash,
    )?
    .is_some()
    {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    preserve_after_file_for_recovery(project, destination, expected_hash, recovery_after_root)?;
    let held = recovery_after_path(destination, &operation.path, transaction_id)?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(operation.path.as_str());
    let (parent, destination_leaf) = root.open_parent_for_mutation(relative, false)?;
    let held_leaf = held
        .file_name()
        .ok_or_else(|| CheckPoError::InvalidTrackedPath(held.display().to_string()))?;
    let mut file = parent.open_file(&destination_leaf)?;
    let actual = file.hash()?.object_id;
    if &actual != expected_hash {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    root.verify_parent_binding(parent_relative, &parent)?;
    parent.rename_no_replace_to(&destination_leaf, &file, &parent, held_leaf)?;
    if let Err(error) = root.verify_parent_binding(parent_relative, &parent) {
        let _ = parent.rename_no_replace_to(held_leaf, &file, &parent, &destination_leaf);
        return Err(error);
    }
    parent.sync_all()?;
    parent.unlink_file_if_bound(held_leaf, file)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

fn recover_before_file(
    project: &ProjectContext,
    backup_root: &Path,
    operation: &FileOperation,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
    transaction_id: &str,
) -> Result<()> {
    let destination = operation
        .path
        .to_project_path(project.project_root.as_path());
    let backup_path = staged_path(backup_root, &operation.path);
    let expected = required_before_hash(operation)?;
    match current_hash_for_recovery(project, &operation.path, before_paths)? {
        Some(current) if &current == expected => {
            restore_before_mtime_for_recovery(project, operation)
        }
        None if backup_regular_file_exists(&backup_path)? => {
            verify_path_hash(&project.repo_root, &backup_path, expected)?;
            copy_backup_file_to_project(
                project,
                operation,
                &backup_path,
                &destination,
                transaction_id,
            )
        }
        Some(_) => Err(CheckPoError::WorkingTreeChanged(operation.path.to_string())),
        None => Err(CheckPoError::Corruption(format!(
            "backup missing for applied operation {}",
            operation.path
        ))),
    }
}

fn current_hash_for_recovery(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<Option<ObjectId>> {
    let full_path = path.to_project_path(project.project_root.as_path());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
            Err(CheckPoError::WorkingTreeChanged(path.to_string()))
        }
        Ok(metadata) if metadata.is_file() => {
            current_file_state_from_anchor(&anchored_project, path).map(|state| Some(state.hash))
        }
        Ok(metadata) if metadata.is_dir() => Ok(None),
        Ok(_) => Err(CheckPoError::WorkingTreeChanged(path.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error)
            if error.kind() == ErrorKind::NotADirectory
                && before_paths.iter().any(|candidate| {
                    path.as_str()
                        .strip_prefix(candidate.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/'))
                }) =>
        {
            Ok(None)
        }
        Err(error) => Err(crate::io_error(&full_path, error)),
    }
}

fn current_file_state_for_recovery(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<Option<CurrentFileState>> {
    let full_path = path.to_project_path(project.project_root.as_path());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
            Err(CheckPoError::WorkingTreeChanged(path.to_string()))
        }
        Ok(metadata) if metadata.is_file() => {
            current_file_state_from_anchor(&anchored_project, path).map(Some)
        }
        Ok(metadata) if metadata.is_dir() => Ok(None),
        Ok(_) => Err(CheckPoError::WorkingTreeChanged(path.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error)
            if error.kind() == ErrorKind::NotADirectory
                && before_paths.iter().any(|candidate| {
                    path.as_str()
                        .strip_prefix(candidate.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/'))
                }) =>
        {
            Ok(None)
        }
        Err(error) => Err(crate::io_error(&full_path, error)),
    }
}

fn quarantine_unknown_transaction_locked(
    project: &ProjectContext,
    tx_root: &Path,
    journal_path: &Path,
    transaction_id: &str,
    reason: &str,
) -> Result<PathBuf> {
    let quarantine_root = project.repo_root.join("quarantined-journals");
    crate::create_dir_all_no_follow(&project.repo_root, &quarantine_root)?;
    let quarantine_name = format!("unknown-{}", Uuid::new_v4().simple());
    let quarantine_path = quarantine_root.join(&quarantine_name);
    let record_path = quarantine_root.join(format!("{quarantine_name}.json"));
    write_quarantine_json(
        &project.repo_root,
        &record_path,
        &QuarantineRecord {
            schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
            transaction_id: transaction_id.to_string(),
            quarantined_at_utc: crate::now_utc_string(),
            original_journal_path: journal_path.to_path_buf(),
            project_was_verified_in_before_state: false,
            resolved_at_utc: None,
            resolved_checkpoint_id: None,
        },
    )?;
    if let Err(error) = move_repo_directory_anchored(&project.repo_root, tx_root, &quarantine_path)
    {
        let _ = remove_repo_file_if_exists_anchored(&project.repo_root, &record_path);
        return Err(error);
    }
    if let Err(error) =
        move_recovery_rescue_into_quarantine(project, transaction_id, &quarantine_path)
    {
        crate::diagnostics::log_warning(
            "transaction-recovery",
            &format!(
                "transaction {transaction_id} was quarantined, but its recovery rescue data remains in CheckPo storage: {error}"
            ),
        );
    }
    crate::diagnostics::log_warning(
        "transaction-recovery",
        &format!(
            "{reason}; transaction {transaction_id} was quarantined at {}",
            quarantine_path.display()
        ),
    );
    Ok(quarantine_path)
}

fn optional_regular_directory_size(path: &Path) -> Result<u64> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(crate::io_error(path, error)),
        Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
            dir_size(path)
        }
        Ok(_) => Err(CheckPoError::Corruption(format!(
            "recovery rescue directory is unsafe: {}",
            path.display()
        ))),
    }
}

fn move_recovery_rescue_into_quarantine(
    project: &ProjectContext,
    transaction_id: &str,
    quarantine_path: &Path,
) -> Result<()> {
    let rescue_root = project
        .repo_root
        .join("recovery-rescues")
        .join(transaction_id);
    match fs::symlink_metadata(&rescue_root) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::io_error(&rescue_root, error)),
        Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(CheckPoError::Corruption(format!(
                "recovery rescue directory is unsafe: {}",
                rescue_root.display()
            )))
        }
    }
    move_repo_directory_anchored(
        &project.repo_root,
        &rescue_root,
        &quarantine_path.join("recovery-rescue"),
    )
}

fn move_repo_directory_anchored(repo_root: &Path, source: &Path, destination: &Path) -> Result<()> {
    let source_relative = source.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "transaction directory is outside repository: {}",
            source.display()
        ))
    })?;
    let destination_relative = destination.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "quarantine directory is outside repository: {}",
            destination.display()
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(repo_root)?;
    let (source_parent, source_leaf) = root.open_parent_for_mutation(source_relative, false)?;
    let source_directory = source_parent.open_directory(&source_leaf)?;
    let (destination_parent, destination_leaf) =
        root.open_parent_for_mutation(destination_relative, true)?;
    let source_parent_relative = source_relative.parent().unwrap_or_else(|| Path::new(""));
    let destination_parent_relative = destination_relative
        .parent()
        .unwrap_or_else(|| Path::new(""));
    root.verify_parent_binding(source_parent_relative, &source_parent)?;
    root.verify_parent_binding(destination_parent_relative, &destination_parent)?;
    source_parent.rename_directory_no_replace_to_owned(
        &source_leaf,
        source_directory,
        &destination_parent,
        &destination_leaf,
    )?;
    destination_parent.sync_all()?;
    source_parent.sync_all()?;
    root.verify_parent_binding(destination_parent_relative, &destination_parent)?;
    root.verify_root_binding()
}

fn remove_repo_file_if_exists_anchored(repo_root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "quarantine record is outside repository: {}",
            path.display()
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(repo_root)?;
    let (parent, leaf) = root.open_parent_for_mutation(relative, false)?;
    let file = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    parent.unlink_file_if_bound(&leaf, file)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

pub(super) fn invalidate_operation_fingerprints(
    project: &ProjectContext,
    operations: &[FileOperation],
) -> Result<()> {
    let paths = operations
        .iter()
        .map(|operation| operation.path.clone())
        .collect::<Vec<_>>();
    crate::invalidate_file_fingerprints(project, &paths)
}
