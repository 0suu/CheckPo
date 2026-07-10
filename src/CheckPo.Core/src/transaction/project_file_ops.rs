use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackupCopyMode {
    Copy,
    ReflinkOrCopy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CurrentFileState {
    pub(super) hash: ObjectId,
    pub(super) size_bytes: u64,
}

pub(super) fn recheck_preconditions(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    for operation in &plan.operations {
        ensure_project_parent_is_safe(project, &operation.path)?;
        let current = current_hash(project, &operation.path)?;
        if current != operation.before_hash {
            return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
        }
    }
    Ok(())
}

pub(crate) fn current_hash(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<Option<ObjectId>> {
    current_file_state(project, path).map(|state| state.map(|state| state.hash))
}

pub(super) fn current_file_state(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<Option<CurrentFileState>> {
    ensure_project_parent_is_safe(project, path)?;
    let full_path = path.to_project_path(project.project_root.as_path());
    let metadata = match fs::symlink_metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(&full_path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CheckPoError::InvalidTrackedPath(path.to_string()));
    }
    let size_bytes = metadata.len();
    hash_file(&full_path).map(|hash| Some(CurrentFileState { hash, size_bytes }))
}

pub(super) fn staged_path(root: &Path, path: &TrackedUnityFilePath) -> PathBuf {
    root.join(path.as_str())
}

pub(super) fn backup_project_file(
    project: &ProjectContext,
    operation: &FileOperation,
    source: &Path,
    backup_path: &Path,
    copy_mode: BackupCopyMode,
) -> Result<()> {
    ensure_project_parent_is_safe(project, &operation.path)?;
    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent).map_err(|error| crate::io_error(parent, error))?;
    }
    let expected_hash = required_before_hash(operation)?;
    backup_project_file_inner(
        project,
        operation,
        source,
        backup_path,
        expected_hash,
        copy_mode,
    )
    .map_err(|error| {
        map_destination_exists_to_working_tree_changed(error, operation.path.to_string())
    })
}

#[cfg(test)]
pub(super) fn backup_project_file_by_reflink_or_copy(
    project: &ProjectContext,
    operation: &FileOperation,
    source: &Path,
    backup_path: &Path,
    expected_hash: &ObjectId,
) -> Result<()> {
    backup_project_file_inner(
        project,
        operation,
        source,
        backup_path,
        expected_hash,
        BackupCopyMode::ReflinkOrCopy,
    )
}

fn backup_project_file_inner(
    project: &ProjectContext,
    operation: &FileOperation,
    source: &Path,
    backup_path: &Path,
    expected_hash: &ObjectId,
    copy_mode: BackupCopyMode,
) -> Result<()> {
    let modified = fs::metadata(source)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| crate::io_error(source, error))?;
    match copy_mode {
        BackupCopyMode::Copy => crate::storage::copy_file_no_replace(
            source,
            backup_path,
            crate::storage::CopySourceDisposition::Keep,
        )?,
        BackupCopyMode::ReflinkOrCopy => {
            crate::storage::reflink_or_copy_file_no_replace(source, backup_path)?;
        }
    }
    filetime::set_file_mtime(backup_path, FileTime::from_system_time(modified))
        .map_err(|error| crate::io_error(backup_path, error))?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(backup_path)
        .and_then(|file| file.sync_all())
        .map_err(|error| crate::io_error(backup_path, error))?;
    finish_created_backup(operation, source, backup_path, expected_hash)?;
    ensure_project_parent_is_safe(project, &operation.path)
}

#[cfg(not(windows))]
pub(super) fn backup_copy_mode(_project: &ProjectContext) -> BackupCopyMode {
    BackupCopyMode::ReflinkOrCopy
}

#[cfg(windows)]
pub(super) fn backup_copy_mode(project: &ProjectContext) -> BackupCopyMode {
    match reflink_copy::check_reflink_support(project.project_root.as_path(), &project.repo_root) {
        Ok(reflink_copy::ReflinkSupport::Supported) => BackupCopyMode::ReflinkOrCopy,
        Ok(reflink_copy::ReflinkSupport::NotSupported | reflink_copy::ReflinkSupport::Unknown)
        | Err(_) => BackupCopyMode::Copy,
    }
}

fn finish_created_backup(
    operation: &FileOperation,
    source: &Path,
    backup_path: &Path,
    expected_hash: &ObjectId,
) -> Result<()> {
    if verify_path_hash(backup_path, expected_hash).is_err() {
        let _ = fs::remove_file(backup_path);
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let quarantine_path = temporary_project_backup_path(source)?;
    fs::rename(source, &quarantine_path).map_err(|error| crate::io_error(source, error))?;
    if verify_path_hash(&quarantine_path, expected_hash).is_err() {
        let _ = fs::remove_file(backup_path);
        restore_quarantined_project_file(source, &quarantine_path)?;
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    fs::remove_file(&quarantine_path).map_err(|error| crate::io_error(&quarantine_path, error))?;
    crate::sync_parent_dir(backup_path)?;
    crate::sync_parent_dir(source)
}

fn temporary_project_backup_path(source: &Path) -> Result<PathBuf> {
    let file_name = source.file_name().ok_or_else(|| {
        CheckPoError::InvalidTrackedPath(format!("invalid path: {}", source.display()))
    })?;
    Ok(source.with_file_name(format!(
        ".checkpo-{}-{}.tmp",
        file_name.to_string_lossy(),
        Uuid::new_v4().simple()
    )))
}

fn restore_quarantined_project_file(source: &Path, quarantine_path: &Path) -> Result<()> {
    if source.exists() {
        return Err(CheckPoError::WorkingTreeChanged(
            source.display().to_string(),
        ));
    }
    fs::rename(quarantine_path, source).map_err(|error| crate::io_error(source, error))?;
    crate::sync_parent_dir(source)?;
    crate::sync_parent_dir(quarantine_path)
}
pub(super) fn restore_staged_file_to_project(
    project: &ProjectContext,
    operation: &FileOperation,
    staged: &Path,
    destination: &Path,
) -> Result<()> {
    restore_file_to_project(
        project,
        &operation.path,
        staged,
        destination,
        required_after_hash(operation)?,
    )
}

pub(super) fn restore_backup_file_to_project(
    project: &ProjectContext,
    operation: &FileOperation,
    backup_path: &Path,
    destination: &Path,
) -> Result<()> {
    let modified = fs::metadata(backup_path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| crate::io_error(backup_path, error))?;
    restore_file_to_project(
        project,
        &operation.path,
        backup_path,
        destination,
        required_before_hash(operation)?,
    )?;
    filetime::set_file_mtime(destination, FileTime::from_system_time(modified))
        .map_err(|error| crate::io_error(destination, error))
}

fn restore_file_to_project(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    source: &Path,
    destination: &Path,
    expected_hash: &ObjectId,
) -> Result<()> {
    ensure_project_parent_for_write(project, path)?;
    verify_path_hash(source, expected_hash)?;
    match move_file_no_replace(source, destination) {
        Ok(()) => {
            crate::sync_parent_dir(destination)?;
            crate::sync_parent_dir(source)
        }
        Err(error) => Err(map_destination_exists_to_working_tree_changed(error, path)),
    }
}

pub(super) fn map_destination_exists_to_working_tree_changed(
    error: CheckPoError,
    path: impl ToString,
) -> CheckPoError {
    match &error {
        CheckPoError::Io { source, .. } if source.kind() == ErrorKind::AlreadyExists => {
            CheckPoError::WorkingTreeChanged(path.to_string())
        }
        _ => error,
    }
}

pub(super) fn remove_project_file(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    destination: &Path,
) -> Result<()> {
    ensure_project_parent_is_safe(project, path)?;
    fs::remove_file(destination).map_err(|error| crate::io_error(destination, error))?;
    crate::sync_parent_dir(destination)
}

fn ensure_project_parent_for_write(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    ensure_project_parent_is_safe(project, path)?;
    let destination = path.to_project_path(project.project_root.as_path());
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| crate::io_error(parent, error))?;
    }
    ensure_project_parent_is_safe(project, path)
}

pub(super) fn ensure_project_parent_is_safe(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let mut current = project.project_root.as_path().to_path_buf();
    let segments = path.as_str().split('/').collect::<Vec<_>>();
    for segment in segments.iter().take(segments.len().saturating_sub(1)) {
        current.push(segment);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(crate::io_error(&current, error)),
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(CheckPoError::InvalidTrackedPath(format!(
                "{} contains unsafe parent component: {}",
                path,
                current.display()
            )));
        }
    }
    Ok(())
}

pub(super) fn verify_path_hash(path: &Path, expected_hash: &ObjectId) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| crate::io_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let actual = hash_file(path)?;
    if &actual != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected_hash,
            actual
        )));
    }
    Ok(())
}

pub(super) fn backup_regular_file_exists(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(crate::io_error(path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(true)
}

pub(super) fn required_before_hash(operation: &FileOperation) -> Result<&ObjectId> {
    operation.before_hash.as_ref().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "operation missing before hash for {}",
            operation.path
        ))
    })
}

pub(super) fn required_after_hash(operation: &FileOperation) -> Result<&ObjectId> {
    operation.after_hash.as_ref().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "operation missing after hash for {}",
            operation.path
        ))
    })
}

pub(super) fn restore_mtime(path: &Path, modified_at_utc: Option<&str>) -> Result<()> {
    let Some(modified_at_utc) = modified_at_utc else {
        return Ok(());
    };
    let time = chrono::DateTime::parse_from_rfc3339(modified_at_utc)
        .map_err(|error| CheckPoError::Unexpected(error.to_string()))?
        .with_timezone(&chrono::Utc);
    filetime::set_file_mtime(path, FileTime::from_system_time(time.into()))
        .map_err(|error| crate::io_error(path, error))
}
