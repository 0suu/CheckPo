use super::*;

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

fn recover_one(project: &ProjectContext, pending: &PendingTransaction) -> Result<()> {
    let tx_root = pending
        .journal_path
        .parent()
        .ok_or_else(|| CheckPoError::Corruption("invalid journal path".into()))?;
    if pending.state == JOURNAL_STATE_UNREADABLE {
        return recover_unreadable_journal(tx_root, &pending.journal_path);
    }
    if !pending.journal_path.is_file() {
        return recover_missing_journal(tx_root, &pending.journal_path);
    }
    let mut journal: TransactionJournal = crate::read_json(&pending.journal_path)?;
    let backup_root = tx_root.join("backup");

    if journal.state == JournalState::Applying {
        invalidate_operation_fingerprints(project, &journal.operations)?;
        for operation in journal.operations.iter().rev() {
            recover_operation(project, &backup_root, operation)?;
        }
    }
    let staged = tx_root.join("staged");
    if staged.exists() {
        fs::remove_dir_all(&staged).map_err(|error| crate::io_error(&staged, error))?;
    }
    journal.state = JournalState::Recovered;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&pending.journal_path, &journal)?;
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
