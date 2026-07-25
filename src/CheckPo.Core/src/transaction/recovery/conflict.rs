use super::*;

pub(super) fn analyze_transaction_recovery_conflicts_locked(
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
