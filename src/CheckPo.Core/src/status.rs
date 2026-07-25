use crate::{
    CheckPoError, CheckpointIndexState, CheckpointIndexStatus, ProjectStatus, ProjectStatusOptions,
    ProjectStorageSummary, Result, StorageSummaryDetail,
};
use std::path::Path;

pub fn project_status(
    project_path: impl AsRef<Path>,
    options: ProjectStatusOptions,
) -> Result<ProjectStatus> {
    let project_path = project_path.as_ref();
    let mut project = crate::load_project(project_path)?;
    if project.location_status != crate::ProjectLocationStatus::CopiedSuspected {
        {
            // Acquiring the exclusive project/repository lock recovers both
            // checkpoint creation and deletion journals before it returns.
            let _recovery_lock =
                crate::acquire_project_repository_lock(&project, "project-status-recovery")?;
        }
        project = crate::load_project(project_path)?;
    }

    let _status_lock = crate::acquire_project_repository_shared_lock(&project, "project-status")?;
    project_status_for_project_unlocked(&project, options)
}

fn project_status_for_project_unlocked(
    project: &crate::ProjectContext,
    options: ProjectStatusOptions,
) -> Result<ProjectStatus> {
    let project_view = crate::project_view(project)?;
    let project_path = project_view.project_root_path.clone();
    let pending_transactions = crate::pending_transactions_for_project(project)?;
    let unresolved_quarantines = crate::unresolved_transaction_quarantines_for_project(project)?;
    let mut checkpoint_index = crate::checkpoint_index_status_unlocked(project)?;
    let mut warnings = Vec::new();

    let checkpoints = if checkpoint_index.state == CheckpointIndexState::Current {
        match crate::list_checkpoints_with_warnings_for_project_unlocked(project) {
            Ok(result) => {
                warnings.extend(result.warnings);
                Some(result.checkpoints)
            }
            Err(CheckPoError::IndexUnavailable(detail)) => {
                checkpoint_index = corrupt_index(detail);
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };

    let storage = if checkpoint_index.state == CheckpointIndexState::Current {
        match options.storage_detail {
            StorageSummaryDetail::IndexOnly => {
                match crate::storage_index_summary_from_index_unlocked(project) {
                    Ok(summary) => Some(ProjectStorageSummary {
                        detail: StorageSummaryDetail::IndexOnly,
                        checkpoint_count: summary.checkpoint_count,
                        unique_blob_count: summary.unique_blob_count,
                        logical_size_bytes: summary.logical_size_bytes,
                        stored_size_bytes: None,
                        recovery_rescue_file_count: None,
                        recovery_rescue_bytes: None,
                    }),
                    Err(CheckPoError::IndexUnavailable(detail)) => {
                        checkpoint_index = corrupt_index(detail);
                        None
                    }
                    Err(error) => return Err(error),
                }
            }
            StorageSummaryDetail::Exact => {
                match crate::storage_summary_from_index_unlocked(project) {
                    Ok(summary) => Some(ProjectStorageSummary {
                        detail: StorageSummaryDetail::Exact,
                        checkpoint_count: summary.checkpoint_count,
                        unique_blob_count: summary.unique_blob_count,
                        logical_size_bytes: summary.logical_size_bytes,
                        stored_size_bytes: Some(summary.stored_size_bytes),
                        recovery_rescue_file_count: Some(summary.recovery_rescue_file_count),
                        recovery_rescue_bytes: Some(summary.recovery_rescue_bytes),
                    }),
                    Err(CheckPoError::IndexUnavailable(detail)) => {
                        checkpoint_index = corrupt_index(detail);
                        None
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    } else {
        None
    };

    warnings.sort();
    warnings.dedup();
    Ok(ProjectStatus {
        project: project_view,
        project_path,
        checkpoint_index,
        checkpoints,
        storage,
        pending_transactions,
        unresolved_quarantines,
        warnings,
    })
}

fn corrupt_index(detail: String) -> CheckpointIndexStatus {
    CheckpointIndexStatus {
        state: CheckpointIndexState::Corrupt,
        rebuildable: true,
        detail: Some(detail),
    }
}
