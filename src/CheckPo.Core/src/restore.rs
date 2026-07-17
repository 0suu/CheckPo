use crate::{
    build_plan_with_progress_and_cancellation, load_project, ApplyOptions, ApplyResult,
    CancellationToken, CheckPoError, OperationPlan, OperationPlanKind, OperationProgress, Result,
    SnapshotId,
};
use std::path::Path;

pub fn preview_restore(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
) -> Result<OperationPlan> {
    preview_restore_with_progress_and_cancellation(project_path, checkpoint_id, None, None)
}

pub fn preview_restore_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<OperationPlan> {
    let project = load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "restore-preview")?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    build_plan_with_progress_and_cancellation(
        &project,
        snapshot_id,
        OperationPlanKind::Restore,
        None,
        progress,
        cancellation,
    )
}

pub fn apply_restore_plan(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    plan: OperationPlan,
    options: ApplyOptions,
) -> Result<ApplyResult> {
    apply_restore_plan_with_progress_and_cancellation(
        project_path,
        checkpoint_id,
        plan,
        options,
        None,
        None,
    )
}

pub fn apply_restore_plan_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    plan: OperationPlan,
    options: ApplyOptions,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<ApplyResult> {
    if !options.yes {
        return Err(crate::user_error("restore apply requires --yes."));
    }
    let project = load_project(project_path)?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    if plan.kind != OperationPlanKind::Restore {
        return Err(CheckPoError::Corruption(
            "expected restore operation plan".to_string(),
        ));
    }
    if plan.checkpoint_id != snapshot_id {
        return Err(CheckPoError::WorkingTreeChanged(
            "restore checkpoint changed after preview".to_string(),
        ));
    }
    crate::transaction::apply_restore_plan_and_resolve_quarantines(
        &project,
        plan,
        options,
        progress,
        cancellation,
        &snapshot_id,
    )
}
