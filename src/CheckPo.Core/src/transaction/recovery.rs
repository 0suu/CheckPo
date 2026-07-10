use super::*;

const QUARANTINE_RECORD_SCHEMA_VERSION_V1: u32 = 1;

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
    unresolved_transaction_quarantines_for_project(&project)
}

pub fn unresolved_transaction_quarantines_for_project(
    project: &ProjectContext,
) -> Result<Vec<UnresolvedTransactionQuarantine>> {
    let Some((quarantine_root, record_paths)) = quarantine_record_paths(project)? else {
        return Ok(Vec::new());
    };
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
        if metadata.file_type().is_symlink() || !metadata.is_file() {
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
    let _lock = acquire_repository_lock(&project.repo_root, "transaction-recover")?;
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
    let _lock = acquire_repository_lock(&project.repo_root, "transaction-quarantine")?;
    let tx_root = journals_dir(&project.repo_root).join(transaction_id);
    ensure_regular_transaction_directory(&tx_root)?;

    let journal_path = tx_root.join("journal.json");
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
    fs::create_dir_all(&quarantine_root)
        .map_err(|error| crate::io_error(&quarantine_root, error))?;
    let quarantine_metadata = fs::symlink_metadata(&quarantine_root)
        .map_err(|error| crate::io_error(&quarantine_root, error))?;
    if quarantine_metadata.file_type().is_symlink() || !quarantine_metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction quarantine root is not a regular directory: {}",
            quarantine_root.display()
        )));
    }
    crate::sync_parent_dir(&quarantine_root)?;

    let quarantine_name = format!("{transaction_id}-{}", Uuid::new_v4().simple());
    let quarantine_path = quarantine_root.join(&quarantine_name);
    let record_path = quarantine_root.join(format!("{quarantine_name}.json"));
    write_json_atomic(
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
    if let Err(error) = fs::rename(&tx_root, &quarantine_path) {
        let _ = fs::remove_file(&record_path);
        return Err(crate::io_error(&tx_root, error));
    }
    crate::sync_parent_dir(&tx_root)?;
    crate::sync_parent_dir(&quarantine_path)?;

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

pub(crate) fn resolve_unverified_transaction_quarantines(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
) -> Result<usize> {
    let _lock = acquire_repository_lock(&project.repo_root, "transaction-quarantine-resolve")?;
    let Some((_quarantine_root, record_paths)) = quarantine_record_paths(project)? else {
        return Ok(0);
    };
    let resolved_at_utc = crate::now_utc_string();
    let mut resolved_count = 0;
    for record_path in record_paths {
        let metadata = fs::symlink_metadata(&record_path)
            .map_err(|error| crate::io_error(&record_path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        if quarantine_record_has_valid_resolution(&record_path)? {
            continue;
        }
        let bytes = fs::read(&record_path).map_err(|error| crate::io_error(&record_path, error))?;
        write_json_atomic(
            &quarantine_resolution_path(&record_path),
            &QuarantineResolutionRecord {
                schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
                resolved_at_utc: resolved_at_utc.clone(),
                resolved_checkpoint_id: checkpoint_id.clone(),
                quarantine_record_digest: blake3::hash(&bytes).to_hex().to_string(),
            },
        )?;
        resolved_count += 1;
    }
    Ok(resolved_count)
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
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    let resolution = match crate::read_json::<QuarantineResolutionRecord>(&resolution_path) {
        Ok(resolution) if resolution.schema_version == QUARANTINE_RECORD_SCHEMA_VERSION_V1 => {
            resolution
        }
        Ok(_) | Err(_) => return Ok(false),
    };
    let record_bytes =
        fs::read(record_path).map_err(|error| crate::io_error(record_path, error))?;
    Ok(resolution.quarantine_record_digest == blake3::hash(&record_bytes).to_hex().as_str())
}

fn quarantine_record_paths(project: &ProjectContext) -> Result<Option<(PathBuf, Vec<PathBuf>)>> {
    let quarantine_root = project.repo_root.join("quarantined-journals");
    let root_metadata = match fs::symlink_metadata(&quarantine_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(&quarantine_root, error)),
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
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
    let bytes = fs::read(path).map_err(|error| crate::io_error(path, error))?;
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

fn inspect_project_before_state(
    project: &ProjectContext,
    tx_root: &Path,
    journal_path: &Path,
) -> (bool, Vec<String>) {
    let result = (|| -> Result<bool> {
        let metadata = fs::symlink_metadata(journal_path)
            .map_err(|error| crate::io_error(journal_path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
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
        return recover_unreadable_journal(tx_root, &pending.journal_path);
    }
    match fs::symlink_metadata(&pending.journal_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(CheckPoError::Corruption(format!(
                "transaction journal is not a regular file: {}",
                pending.journal_path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return recover_missing_journal(tx_root, &pending.journal_path);
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

    if transaction_needs_rollback(project, &journal, &backup_paths, &staged_paths)? {
        invalidate_operation_fingerprints(project, &journal.operations)?;
        for operation in journal.operations.iter().rev() {
            recover_operation(project, &backup_root, operation)?;
        }
        ensure_before_state_restored(project, &journal.operations)?;
        if !validate_transaction_payload(&backup_root, BTreeSet::new())?.is_empty() {
            return Err(CheckPoError::Corruption(format!(
                "transaction backup still contains files after recovery: {}",
                backup_root.display()
            )));
        }
    }
    if staged_root.exists() {
        fs::remove_dir_all(&staged_root).map_err(|error| crate::io_error(&staged_root, error))?;
        crate::sync_parent_dir(&staged_root)?;
    }
    journal.state = JournalState::Recovered;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&pending.journal_path, &journal)?;
    Ok(())
}

fn ensure_regular_transaction_directory(tx_root: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(tx_root).map_err(|error| crate::io_error(tx_root, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
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
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction payload root is not a regular directory: {}",
            root.display()
        )));
    }

    let mut present = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| CheckPoError::Corruption(error.to_string()))?;
        let file_type = entry.file_type();
        if file_type.is_symlink() {
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
        if !allowed_paths.contains(&path) {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains an unexpected path: {path}"
            )));
        }
        present.insert(path);
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
        if current_hash(project, &operation.path)? != operation.before_hash {
            return Err(CheckPoError::Corruption(format!(
                "transaction recovery did not restore before state for {}",
                operation.path
            )));
        }
    }
    Ok(())
}

fn recover_missing_journal(tx_root: &Path, journal_path: &Path) -> Result<()> {
    let backup_root = tx_root.join("backup");
    if !directory_is_empty_or_missing(&backup_root)? {
        return Err(CheckPoError::Corruption(format!(
            "transaction journal is missing but backup is not empty: {}",
            journal_path.display()
        )));
    }
    let staged_root = tx_root.join("staged");
    if !directory_is_empty_or_missing(&staged_root)? {
        return Err(CheckPoError::Corruption(format!(
            "transaction journal is missing but staged files are not empty: {}",
            journal_path.display()
        )));
    }
    fs::remove_dir_all(tx_root).map_err(|error| crate::io_error(tx_root, error))
}

fn recover_unreadable_journal(tx_root: &Path, journal_path: &Path) -> Result<()> {
    let backup_root = tx_root.join("backup");
    if !directory_is_empty_or_missing(&backup_root)? {
        return Err(CheckPoError::Corruption(format!(
            "transaction journal is unreadable but backup is not empty: {}",
            journal_path.display()
        )));
    }
    let staged_root = tx_root.join("staged");
    if !directory_is_empty_or_missing(&staged_root)? {
        return Err(CheckPoError::Corruption(format!(
            "transaction journal is unreadable but staged files are not empty: {}",
            journal_path.display()
        )));
    }
    fs::remove_dir_all(tx_root).map_err(|error| crate::io_error(tx_root, error))
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

fn recover_operation(
    project: &ProjectContext,
    backup_root: &Path,
    operation: &FileOperation,
) -> Result<()> {
    let destination = operation
        .path
        .to_project_path(project.project_root.as_path());
    match operation.operation_type {
        FileOperationType::Restore => {
            ensure_project_parent_is_safe(project, &operation.path)?;
            let Some(after_hash) = operation.after_hash.as_ref() else {
                return Err(CheckPoError::Corruption(format!(
                    "restore operation missing after hash for {}",
                    operation.path
                )));
            };
            match current_hash(project, &operation.path)? {
                None => Ok(()),
                Some(current) if &current == after_hash => {
                    remove_project_file(project, &operation.path, &destination)
                }
                Some(_) => Err(CheckPoError::WorkingTreeChanged(operation.path.to_string())),
            }
        }
        FileOperationType::Delete | FileOperationType::Replace => {
            let backup_path = staged_path(backup_root, &operation.path);
            if backup_regular_file_exists(&backup_path)? {
                recover_from_backup(project, operation, &destination, &backup_path)
            } else {
                ensure_operation_not_applied(project, operation)
            }
        }
    }
}

fn recover_from_backup(
    project: &ProjectContext,
    operation: &FileOperation,
    destination: &Path,
    backup_path: &Path,
) -> Result<()> {
    ensure_project_parent_is_safe(project, &operation.path)?;
    verify_path_hash(backup_path, required_before_hash(operation)?)?;
    let current = current_hash(project, &operation.path)?;
    if current == operation.before_hash {
        fs::remove_file(backup_path).map_err(|error| crate::io_error(backup_path, error))?;
        crate::sync_parent_dir(backup_path)?;
        return Ok(());
    }
    if current.is_some() && current != operation.after_hash {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    if destination.exists() {
        remove_project_file(project, &operation.path, destination)?;
    }
    restore_backup_file_to_project(project, operation, backup_path, destination)
}

fn ensure_operation_not_applied(project: &ProjectContext, operation: &FileOperation) -> Result<()> {
    let current = current_hash(project, &operation.path)?;
    if current == operation.before_hash {
        return Ok(());
    }
    if current == operation.after_hash {
        return Err(CheckPoError::Corruption(format!(
            "backup missing for applied operation {}",
            operation.path
        )));
    }
    Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()))
}
