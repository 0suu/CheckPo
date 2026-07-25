use super::*;

pub(super) fn pending_transaction_by_id(
    project: &ProjectContext,
    transaction_id: &str,
) -> Result<PendingTransaction> {
    pending_transactions_for_project(project)?
        .into_iter()
        .find(|pending| pending.transaction_id == transaction_id)
        .ok_or_else(|| crate::user_error("the interrupted transaction is no longer pending."))
}

pub(super) fn read_valid_recovery_journal(
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

pub(super) fn validate_transaction_id(transaction_id: &str) -> Result<()> {
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

pub(super) fn validate_recovery_conflict_plan_id(plan_id: &str) -> Result<()> {
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

pub(super) fn recovery_conflict_plan_id(
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
