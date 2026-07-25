use super::*;

mod common;
mod conflict;
mod export;
mod quarantine;
mod rescue;
mod rollback;
mod state;

use common::*;
use conflict::*;
use export::*;
use quarantine::quarantine_unknown_transaction_locked;
use rescue::*;
use rollback::{
    current_file_state_for_recovery, ensure_regular_transaction_directory, journal_before_paths,
    preserve_after_file_for_recovery, recover_one, remove_anchored_project_file,
    validate_transaction_payload,
};
use state::*;

pub(super) use quarantine::resolve_unverified_transaction_quarantines_unlocked;
pub(crate) use quarantine::unresolved_transaction_quarantines_for_project;
pub use quarantine::{
    ensure_no_unresolved_transaction_quarantines, quarantine_transaction,
    unresolved_transaction_quarantines,
};
#[cfg(test)]
pub(super) use rescue::{
    prepare_recovery_conflict_rescue_and_remove_first_for_test,
    prepare_recovery_conflict_rescue_for_test,
};
pub(super) use rollback::invalidate_operation_fingerprints;

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
                let recovery_conflict_count = analyze_transaction_recovery_conflicts_locked(
                    &project,
                    &pending.transaction_id,
                )
                .map(|plan| plan.conflicts.len())
                .unwrap_or(0);
                result.failed_transaction_count += 1;
                result.failed_transactions.push(TransactionRecoveryFailure {
                    transaction_id: pending.transaction_id,
                    error: error.to_string(),
                    recovery_conflict_count,
                });
            }
        }
    }
    Ok(result)
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
