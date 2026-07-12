use crate::{
    list_snapshot_ids, load_project, object_path, read_latest_snapshot_id,
    report_operation_progress, snapshots_dir, validate_repository_config, CancellationToken,
    ObjectId, OperationProgress, Result, SnapshotFile, SnapshotId, TrackedUnityFilePath,
    VerificationResult,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;

const VERIFY_HASH_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_VERIFICATION_ERRORS: usize = 1024;

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
                push_verification_error(&mut result, error.to_string());
            }
        }
        Err(error) => push_verification_error(&mut result, error.to_string()),
    }
    warn_invalid_extra_json(&project.repo_root, &mut result)?;
    let snapshot_ids = list_snapshot_ids(&project.repo_root)?;
    let snapshot_total = snapshot_ids.len();
    let mut verified_object_shards = HashSet::new();
    let mut verified_objects = HashMap::new();
    for (index, snapshot_id) in snapshot_ids.into_iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        match crate::storage::load_snapshot_with_warnings(&project.repo_root, &snapshot_id) {
            Ok((snapshot, warnings)) => {
                result.warnings.extend(warnings);
                if snapshot.project_id != project.project_id {
                    push_verification_error(
                        &mut result,
                        format!(
                            "{snapshot_id}: snapshot project id does not match current project"
                        ),
                    );
                }
                let mut state = VerifyObjectState {
                    result: &mut result,
                    verified_shards: &mut verified_object_shards,
                    verified_objects: &mut verified_objects,
                };
                verify_snapshot(
                    &project.repo_root,
                    &snapshot_id,
                    &snapshot,
                    full,
                    progress,
                    cancellation,
                    &mut state,
                )?
            }
            Err(error) => push_verification_error(&mut result, error.to_string()),
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
                push_verification_error(
                    &mut result,
                    format!("refs/latest points to missing snapshot {latest}"),
                );
            }
        }
        Ok(None) => {}
        Err(error) => push_verification_error(&mut result, error.to_string()),
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
    let mut verified_object_shards = HashSet::new();
    let mut verified_objects = HashMap::new();
    match crate::storage::load_project_snapshot_with_warnings(&project, &snapshot_id) {
        Ok((snapshot, warnings)) => {
            result.warnings.extend(warnings);
            let mut state = VerifyObjectState {
                result: &mut result,
                verified_shards: &mut verified_object_shards,
                verified_objects: &mut verified_objects,
            };
            verify_snapshot(
                &project.repo_root,
                &snapshot_id,
                &snapshot,
                full,
                progress,
                cancellation,
                &mut state,
            )?
        }
        Err(error) => push_verification_error(&mut result, error.to_string()),
    }
    result.is_valid = result.errors.is_empty();
    Ok(result)
}

struct VerifyObjectState<'a> {
    result: &'a mut VerificationResult,
    verified_shards: &'a mut HashSet<std::path::PathBuf>,
    verified_objects: &'a mut HashMap<ObjectId, VerifiedObject>,
}

struct VerifiedObject {
    expected_size_bytes: u64,
    first_snapshot_id: SnapshotId,
}

fn verify_snapshot(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    state: &mut VerifyObjectState<'_>,
) -> Result<()> {
    for (index, file) in snapshot.files.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        if let Err(error) = TrackedUnityFilePath::parse(file.path.as_str()) {
            push_verification_error(state.result, error.to_string());
        }
        if let Err(error) = ObjectId::parse(file.content_hash().as_str()) {
            push_verification_error(state.result, error.to_string());
        }
        if let Some(verified) = state.verified_objects.get(file.content_hash()) {
            if verified.expected_size_bytes != file.size_bytes {
                push_verification_error(
                    state.result,
                    format!(
                        "{snapshot_id}: object {} has conflicting expected sizes: {} in snapshot {}, {} for {}",
                        file.content_hash(),
                        verified.expected_size_bytes,
                        verified.first_snapshot_id,
                        file.size_bytes,
                        file.path
                    ),
                );
            }
            report_operation_progress(
                progress,
                "verifyObjects",
                index + 1,
                snapshot.files.len(),
                Some(file.path.to_string()),
            );
            continue;
        }
        state.verified_objects.insert(
            file.content_hash().clone(),
            VerifiedObject {
                expected_size_bytes: file.size_bytes,
                first_snapshot_id: snapshot_id.clone(),
            },
        );
        let object = object_path(repo_root, file.content_hash());
        let shard = object.parent().ok_or_else(|| {
            crate::CheckPoError::Corruption(format!("invalid object path: {}", object.display()))
        })?;
        if !state.verified_shards.contains(shard) {
            match crate::storage::object_path_no_follow(repo_root, file.content_hash()) {
                Ok(_) => {
                    state.verified_shards.insert(shard.to_path_buf());
                }
                Err(error) => {
                    push_verification_error(state.result, error.to_string());
                    continue;
                }
            }
        }
        let metadata = match fs::symlink_metadata(&object) {
            Ok(metadata)
                if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) =>
            {
                metadata
            }
            Ok(_) => {
                push_verification_error(
                    state.result,
                    format!(
                        "{snapshot_id}: object {} for {} is not a no-follow regular file",
                        file.content_hash(),
                        file.path
                    ),
                );
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                push_verification_error(
                    state.result,
                    format!(
                        "{snapshot_id}: missing object {} for {}",
                        file.content_hash(),
                        file.path
                    ),
                );
                continue;
            }
            Err(error) => {
                push_verification_error(state.result, error.to_string());
                continue;
            }
        };
        if full {
            if let Err(error) = verify_file_hash_and_size_with_cancellation(
                &object,
                file.content_hash(),
                file.size_bytes,
                cancellation,
            ) {
                push_verification_error(state.result, error.to_string());
            }
        } else {
            match metadata.len() {
                size if size == file.size_bytes => {}
                size => push_verification_error(
                    state.result,
                    format!(
                        "{} size expected {}, got {}",
                        object.display(),
                        file.size_bytes,
                        size
                    ),
                ),
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

fn verify_file_hash_and_size_with_cancellation(
    path: &Path,
    expected: &ObjectId,
    size_bytes: u64,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    crate::ensure_not_cancelled(cancellation)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| crate::io_error(path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(crate::CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() != size_bytes {
        return Err(crate::CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            path.display(),
            size_bytes,
            metadata.len()
        )));
    }

    let mut input = fs::File::open(path).map_err(|error| crate::io_error(path, error))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; VERIFY_HASH_BUFFER_SIZE];
    loop {
        crate::ensure_not_cancelled(cancellation)?;
        let read = input
            .read(&mut buffer)
            .map_err(|error| crate::io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = ObjectId::parse(hasher.finalize().to_hex().as_ref())?;
    if &actual != expected {
        return Err(crate::CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected,
            actual
        )));
    }
    Ok(())
}

fn push_verification_error(result: &mut VerificationResult, error: String) {
    if result.errors.len() < MAX_VERIFICATION_ERRORS {
        result.errors.push(error);
    } else if result.errors.len() == MAX_VERIFICATION_ERRORS {
        result.errors.push(format!(
            "verification stopped collecting detailed errors after {MAX_VERIFICATION_ERRORS} entries"
        ));
    }
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
