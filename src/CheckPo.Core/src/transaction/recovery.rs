use super::*;

const QUARANTINE_RECORD_SCHEMA_VERSION_V1: u32 = 1;
const MAX_QUARANTINE_RECORD_BYTES: u64 = 1024 * 1024;

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
        match recover_one(&project, &pending) {
            Ok(()) => {
                result.recovered_transaction_count += 1;
                result
                    .recovered_transaction_ids
                    .push(pending.transaction_id);
            }
            Err(error) => {
                crate::log_operation_error("transaction-recovery", &error.to_string());
                result.failed_transaction_count += 1;
                result.failed_transactions.push(TransactionRecoveryFailure {
                    transaction_id: pending.transaction_id,
                    error: error.to_string(),
                });
            }
        }
    }
    Ok(result)
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
                JournalState::Committed | JournalState::Recovered
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
    let preserved_bytes = match dir_size(&tx_root) {
        Ok(size) => size,
        Err(error) => {
            warnings.push(format!(
                "Preserved byte count could not be calculated: {error}"
            ));
            0
        }
    };

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

    for directory in journal.directories_to_create.iter().rev() {
        if !before_paths.iter().any(|before| {
            directory == before
                || directory
                    .as_str()
                    .strip_prefix(before.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            continue;
        }
        let path = directory.to_project_path(project.project_root.as_path());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
                remove_project_directory(project, directory)?;
            }
            Ok(metadata)
                if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {}
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
    let actual = file.hash()?.object_id;
    if &actual != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected_hash,
            actual
        )));
    }
    parent.unlink_file_if_bound(&leaf, file)?;
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
    crate::diagnostics::log_warning(
        "transaction-recovery",
        &format!(
            "{reason}; transaction {transaction_id} was quarantined at {}",
            quarantine_path.display()
        ),
    );
    Ok(quarantine_path)
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
