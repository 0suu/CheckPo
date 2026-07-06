use crate::{
    list_snapshot_ids, load_project, load_project_snapshot, load_snapshot, object_path,
    read_latest_snapshot_id, report_operation_progress, snapshots_dir, validate_repository_config,
    CancellationToken, ObjectId, OperationProgress, Result, SnapshotFile, SnapshotId,
    TrackedUnityFilePath, VerificationResult,
};
use std::fs;
use std::path::Path;

pub fn verify_project(project_path: impl AsRef<Path>, full: bool) -> Result<VerificationResult> {
    verify_project_with_progress_and_cancellation(project_path, full, None, None)
}

pub fn verify_project_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<VerificationResult> {
    let project = load_project(project_path)?;
    let mut result = VerificationResult {
        is_valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    match crate::read_json::<crate::RepositoryConfig>(&project.repo_root.join("repo.json")) {
        Ok(config) => {
            if let Err(error) = validate_repository_config(&config, &project.project_id) {
                result.errors.push(error.to_string());
            }
        }
        Err(error) => result.errors.push(error.to_string()),
    }
    warn_invalid_extra_json(&project.repo_root, &mut result)?;
    let snapshot_ids = list_snapshot_ids(&project.repo_root)?;
    let snapshot_total = snapshot_ids.len();
    for (index, snapshot_id) in snapshot_ids.into_iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        match load_snapshot(&project.repo_root, &snapshot_id) {
            Ok(snapshot) => {
                if snapshot.project_id != project.project_id {
                    result.errors.push(format!(
                        "{snapshot_id}: snapshot project id does not match current project"
                    ));
                }
                verify_snapshot(
                    &project.repo_root,
                    &snapshot_id,
                    &snapshot,
                    full,
                    progress,
                    cancellation,
                    &mut result,
                )?
            }
            Err(error) => result.errors.push(error.to_string()),
        }
        report_operation_progress(
            progress,
            "verifySnapshots",
            index + 1,
            snapshot_total,
            Some(snapshot_id.to_string()),
        );
    }
    match read_latest_snapshot_id(&project.repo_root) {
        Ok(Some(latest)) => {
            let path = crate::snapshot_path(&project.repo_root, &latest);
            if !path.is_file() {
                result
                    .errors
                    .push(format!("refs/latest points to missing snapshot {latest}"));
            }
        }
        Ok(None) => {}
        Err(error) => result.errors.push(error.to_string()),
    }
    result.is_valid = result.errors.is_empty();
    Ok(result)
}

pub fn verify_checkpoint(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    full: bool,
) -> Result<VerificationResult> {
    verify_checkpoint_with_progress_and_cancellation(project_path, checkpoint_id, full, None, None)
}

pub fn verify_checkpoint_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<VerificationResult> {
    let project = load_project(project_path)?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    let mut result = VerificationResult {
        is_valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    match load_project_snapshot(&project, &snapshot_id) {
        Ok(snapshot) => verify_snapshot(
            &project.repo_root,
            &snapshot_id,
            &snapshot,
            full,
            progress,
            cancellation,
            &mut result,
        )?,
        Err(error) => result.errors.push(error.to_string()),
    }
    result.is_valid = result.errors.is_empty();
    Ok(result)
}

fn verify_snapshot(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    result: &mut VerificationResult,
) -> Result<()> {
    for (index, file) in snapshot.files.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        if let Err(error) = TrackedUnityFilePath::parse(file.path.as_str()) {
            result.errors.push(error.to_string());
        }
        if let Err(error) = ObjectId::parse(file.content_hash().as_str()) {
            result.errors.push(error.to_string());
        }
        let object = object_path(repo_root, file.content_hash());
        if !object.is_file() {
            result.errors.push(format!(
                "{snapshot_id}: missing object {} for {}",
                file.content_hash(),
                file.path
            ));
            continue;
        }
        if full {
            if let Err(error) =
                crate::verify_file_hash_and_size(&object, file.content_hash(), file.size_bytes)
            {
                result.errors.push(error.to_string());
            }
        } else {
            match fs::metadata(&object) {
                Ok(metadata) if metadata.len() == file.size_bytes => {}
                Ok(metadata) => result.errors.push(format!(
                    "{} size expected {}, got {}",
                    object.display(),
                    file.size_bytes,
                    metadata.len()
                )),
                Err(error) => result.errors.push(error.to_string()),
            }
        }
        report_operation_progress(
            progress,
            "verifyObjects",
            index + 1,
            snapshot.files.len(),
            Some(file.path.to_string()),
        );
    }
    Ok(())
}

fn warn_invalid_extra_json(repo_root: &Path, result: &mut VerificationResult) -> Result<()> {
    let dir = snapshots_dir(repo_root);
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir).map_err(|error| crate::io_error(&dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(&dir, error))?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if SnapshotId::parse(stem).is_err() {
            result.warnings.push(format!(
                "ignored invalid snapshot json filename: {}",
                path.display()
            ));
        }
    }
    Ok(())
}
