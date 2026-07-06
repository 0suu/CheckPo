use crate::{
    build_plan_with_progress_and_cancellation, load_project, parse_tracked_paths,
    read_latest_snapshot_id, ApplyOptions, ApplyResult, CancellationToken, CheckPoError,
    OperationPlan, OperationPlanKind, OperationProgress, Result, SnapshotId, TrackedUnityFilePath,
};
use std::collections::BTreeSet;
use std::path::Path;

pub fn preview_discard(
    project_path: impl AsRef<Path>,
    checkpoint_id: Option<&str>,
    paths: &[TrackedUnityFilePath],
) -> Result<OperationPlan> {
    preview_discard_with_progress_and_cancellation(project_path, checkpoint_id, paths, None, None)
}

pub fn preview_discard_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: Option<&str>,
    paths: &[TrackedUnityFilePath],
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<OperationPlan> {
    let project = load_project(project_path)?;
    let snapshot_id = match checkpoint_id {
        Some(id) => SnapshotId::parse(id)?,
        None => read_latest_snapshot_id(&project.repo_root)?
            .ok_or_else(|| crate::CheckPoError::SnapshotNotFound("latest".into()))?,
    };
    build_plan_with_progress_and_cancellation(
        &project,
        snapshot_id,
        OperationPlanKind::Discard,
        Some(paths),
        progress,
        cancellation,
    )
}

pub fn preview_discard_files(
    project_path: impl AsRef<Path>,
    paths: &[String],
    checkpoint_id: Option<&str>,
) -> Result<OperationPlan> {
    let tracked = parse_tracked_paths(paths)?;
    preview_discard(project_path, checkpoint_id, &tracked)
}

pub fn apply_discard_plan(
    project_path: impl AsRef<Path>,
    checkpoint_id: Option<&str>,
    paths: &[TrackedUnityFilePath],
    plan: OperationPlan,
    options: ApplyOptions,
) -> Result<ApplyResult> {
    apply_discard_plan_with_progress_and_cancellation(
        project_path,
        checkpoint_id,
        paths,
        plan,
        options,
        None,
        None,
    )
}

pub fn apply_discard_plan_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: Option<&str>,
    paths: &[TrackedUnityFilePath],
    plan: OperationPlan,
    options: ApplyOptions,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<ApplyResult> {
    if !options.yes {
        return Err(crate::user_error("discard apply requires --yes."));
    }
    let project = load_project(project_path)?;
    let snapshot_id = match checkpoint_id {
        Some(id) => SnapshotId::parse(id)?,
        None => read_latest_snapshot_id(&project.repo_root)?
            .ok_or_else(|| crate::CheckPoError::SnapshotNotFound("latest".into()))?,
    };
    if plan.kind != OperationPlanKind::Discard {
        return Err(CheckPoError::Corruption(
            "expected discard operation plan".to_string(),
        ));
    }
    if plan.checkpoint_id != snapshot_id {
        return Err(CheckPoError::WorkingTreeChanged(
            "discard checkpoint changed after preview".to_string(),
        ));
    }
    let selected = paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if plan.selected_paths.as_deref() != Some(selected.as_slice()) {
        return Err(CheckPoError::WorkingTreeChanged(
            "discard path set changed after preview".to_string(),
        ));
    }
    crate::apply_plan(&project, plan, options, progress, cancellation)
}

pub fn apply_discard_files_plan(
    project_path: impl AsRef<Path>,
    paths: &[String],
    checkpoint_id: Option<&str>,
    plan: OperationPlan,
    options: ApplyOptions,
) -> Result<ApplyResult> {
    let tracked = parse_tracked_paths(paths)?;
    apply_discard_plan(project_path, checkpoint_id, &tracked, plan, options)
}
