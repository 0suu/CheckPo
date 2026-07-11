use super::*;

pub(super) use crate::project::ensure_project_parent_is_safe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackupCopyMode {
    #[cfg(windows)]
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
        let full_path = operation
            .path
            .to_project_path(project.project_root.as_path());
        let current = match fs::symlink_metadata(&full_path) {
            Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
                return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()))
            }
            Ok(metadata) if metadata.is_file() => Some(hash_file(&full_path)?),
            Ok(metadata)
                if metadata.is_dir()
                    && operation.before_hash.is_none()
                    && plan.directories_to_remove.contains(&operation.path) =>
            {
                None
            }
            Ok(_) => return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string())),
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    || (error.kind() == ErrorKind::NotADirectory
                        && has_tracked_ancestor(
                            &operation.path,
                            plan.operations.iter().filter_map(|candidate| {
                                candidate.before_hash.as_ref().map(|_| &candidate.path)
                            }),
                        )) =>
            {
                None
            }
            Err(error) => return Err(crate::io_error(&full_path, error)),
        };
        if current != operation.before_hash {
            return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
        }
    }
    for directory in &plan.directories_to_remove {
        let path = directory.to_project_path(project.project_root.as_path());
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| crate::io_error(&path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(CheckPoError::WorkingTreeChanged(directory.to_string()));
        }
    }
    let before_file_paths = plan
        .operations
        .iter()
        .filter(|operation| operation.before_hash.is_some())
        .map(|operation| operation.path.clone())
        .collect::<BTreeSet<_>>();
    for directory in &plan.directories_to_create {
        let path = directory.to_project_path(project.project_root.as_path());
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file()
                    && !crate::metadata_is_link_or_reparse(&metadata)
                    && before_file_paths.contains(directory) => {}
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    || (error.kind() == ErrorKind::NotADirectory
                        && has_tracked_ancestor(directory, before_file_paths.iter())) => {}
            Ok(_) => return Err(CheckPoError::WorkingTreeChanged(directory.to_string())),
            Err(error) => return Err(crate::io_error(&path, error)),
        }
    }
    Ok(())
}

fn has_tracked_ancestor<'a>(
    path: &TrackedUnityFilePath,
    mut candidates: impl Iterator<Item = &'a TrackedUnityFilePath>,
) -> bool {
    candidates.any(|candidate| {
        path.as_str()
            .strip_prefix(candidate.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
    })
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
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
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
        create_dir_all_durable(parent, &project.repo_root)?;
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
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| crate::io_error(source, error))?;
    if crate::metadata_is_link_or_reparse(&source_metadata) || !source_metadata.is_file() {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let modified = fs::metadata(source)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| crate::io_error(source, error))?;
    match copy_mode {
        #[cfg(windows)]
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
    crate::sync_parent_dir(backup_path)?;
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
    crate::sync_parent_dir(source)
}

fn temporary_project_backup_path(source: &Path) -> Result<PathBuf> {
    source.file_name().ok_or_else(|| {
        CheckPoError::InvalidTrackedPath(format!("invalid path: {}", source.display()))
    })?;
    Ok(source.with_file_name(format!(".checkpo-{}.tmp", Uuid::new_v4().simple())))
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

pub(super) fn copy_backup_file_to_project(
    project: &ProjectContext,
    operation: &FileOperation,
    backup_path: &Path,
    destination: &Path,
) -> Result<()> {
    let modified = fs::metadata(backup_path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| crate::io_error(backup_path, error))?;
    ensure_project_parent_for_write(project, &operation.path)?;
    let expected = required_before_hash(operation)?;
    verify_path_hash(backup_path, expected)?;
    crate::storage::copy_file_no_replace(
        backup_path,
        destination,
        crate::storage::CopySourceDisposition::Keep,
    )
    .map_err(|error| map_destination_exists_to_working_tree_changed(error, &operation.path))?;
    if let Err(error) = verify_path_hash(destination, expected) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    filetime::set_file_mtime(destination, FileTime::from_system_time(modified))
        .map_err(|error| crate::io_error(destination, error))?;
    sync_project_file(destination)?;
    crate::sync_parent_dir(destination)
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
    move_file_no_replace(source, destination)
        .map_err(|error| map_destination_exists_to_working_tree_changed(error, path))
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

pub(super) fn remove_project_directory(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    ensure_project_parent_is_safe(project, path)?;
    let destination = path.to_project_path(project.project_root.as_path());
    let metadata =
        fs::symlink_metadata(&destination).map_err(|error| crate::io_error(&destination, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::WorkingTreeChanged(path.to_string()));
    }
    fs::remove_dir(&destination).map_err(|error| {
        if error.kind() == ErrorKind::DirectoryNotEmpty {
            CheckPoError::WorkingTreeChanged(path.to_string())
        } else {
            crate::io_error(&destination, error)
        }
    })?;
    crate::sync_parent_dir(&destination)
}

pub(super) fn create_project_directory(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    ensure_project_parent_is_safe(project, path)?;
    let destination = path.to_project_path(project.project_root.as_path());
    match fs::create_dir(&destination) {
        Ok(()) => crate::sync_parent_dir(&destination),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            Err(CheckPoError::WorkingTreeChanged(path.to_string()))
        }
        Err(error) => Err(crate::io_error(&destination, error)),
    }
}

pub(super) fn ensure_project_directory_exists_for_recovery(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let destination = path.to_project_path(project.project_root.as_path());
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
            Ok(())
        }
        Ok(_) => Err(CheckPoError::WorkingTreeChanged(path.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            create_project_directory(project, path)
        }
        Err(error) => Err(crate::io_error(&destination, error)),
    }
}

fn ensure_project_parent_for_write(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    ensure_project_parent_is_safe(project, path)?;
    let destination = path.to_project_path(project.project_root.as_path());
    if let Some(parent) = destination.parent() {
        create_dir_all_durable(parent, project.project_root.as_path())?;
    }
    ensure_project_parent_is_safe(project, path)
}

fn create_dir_all_durable(directory: &Path, stop_at: &Path) -> Result<()> {
    let relative = directory.strip_prefix(stop_at).map_err(|_| {
        CheckPoError::Unexpected(format!(
            "cannot create durable directory outside {}: {}",
            stop_at.display(),
            directory.display()
        ))
    })?;
    let mut current = stop_at.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
                continue
            }
            Ok(_) => {
                return Err(CheckPoError::InvalidTrackedPath(format!(
                    "unsafe directory component: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => match fs::create_dir(&current) {
                Ok(()) => crate::sync_parent_dir(&current)?,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&current)
                        .map_err(|error| crate::io_error(&current, error))?;
                    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                        return Err(CheckPoError::InvalidTrackedPath(format!(
                            "unsafe directory component: {}",
                            current.display()
                        )));
                    }
                }
                Err(error) => return Err(crate::io_error(&current, error)),
            },
            Err(error) => return Err(crate::io_error(&current, error)),
        }
    }
    Ok(())
}

pub(super) fn verify_path_hash(path: &Path, expected_hash: &ObjectId) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| crate::io_error(path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
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
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
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

pub(super) fn sync_project_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| crate::io_error(path, error))
}
