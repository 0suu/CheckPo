use super::*;

pub(super) use crate::project::ensure_project_parent_is_safe;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackupCopyMode {
    ReflinkOrCopy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CurrentFileState {
    pub(super) hash: ObjectId,
    pub(super) size_bytes: u64,
    pub(super) modified_at_utc: String,
}

pub(super) struct DeferredBackupSource {
    path: TrackedUnityFilePath,
    source: crate::storage::AnchoredFile,
    version: crate::storage::AnchoredFileVersion,
}

pub(super) fn recheck_preconditions(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    for operation in &plan.operations {
        let full_path = operation
            .path
            .to_project_path(project.project_root.as_path());
        let current = match fs::symlink_metadata(&full_path) {
            Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
                return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()))
            }
            Ok(metadata) if metadata.is_file() => Some(current_file_state_from_anchor(
                &anchored_project,
                &operation.path,
            )?),
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
        if current.as_ref().map(|state| &state.hash) != operation.before_hash.as_ref()
            || current.as_ref().map(|state| state.size_bytes) != operation.before_size_bytes
            || current.as_ref().map(|state| state.modified_at_utc.as_str())
                != operation.before_modified_at_utc.as_deref()
        {
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
    anchored_project.verify_root_binding()
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
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let state = current_file_state_from_anchor(&anchored_project, path)?;
    anchored_project.verify_root_binding()?;
    Ok(Some(state))
}

pub(super) fn current_file_state_from_anchor(
    anchored_project: &crate::storage::AnchoredRoot,
    path: &TrackedUnityFilePath,
) -> Result<CurrentFileState> {
    let relative = Path::new(path.as_str());
    let mut file = anchored_project.open_file(relative)?;
    let hashed = file.hash()?;
    anchored_project.verify_binding(relative, &file)?;
    let modified = hashed
        .metadata
        .modified()
        .map_err(|error| crate::io_error(path.to_string(), error))?;
    Ok(CurrentFileState {
        hash: hashed.object_id,
        size_bytes: hashed.metadata.len(),
        modified_at_utc: crate::canonical_utc(modified),
    })
}

pub(super) fn staged_path(root: &Path, path: &TrackedUnityFilePath) -> PathBuf {
    root.join(path.as_str())
}

pub(super) fn prepare_stage_destination_parent(
    project: &ProjectContext,
    anchored_repo: &crate::storage::AnchoredRoot,
    destination_parent: &Path,
    sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
) -> Result<()> {
    let parent_relative = destination_parent
        .strip_prefix(&project.repo_root)
        .map_err(|_| {
            CheckPoError::Corruption(format!(
                "transaction staged parent is outside repository {}: {}",
                project.repo_root.display(),
                destination_parent.display()
            ))
        })?;
    let synthetic_file = parent_relative.join(".checkpo-staging-parent-anchor");
    let _ = anchored_repo.open_parent_batched(&synthetic_file, true, sync_batch)?;
    Ok(())
}

// The held repository, destination, durability batch, and cancellation token
// are separate safety boundaries and are intentionally explicit here.
#[allow(clippy::too_many_arguments)]
pub(super) fn stage_object_for_transaction_prepared(
    project: &ProjectContext,
    anchored_repo: &crate::storage::AnchoredRoot,
    object_id: &ObjectId,
    destination: &Path,
    size_bytes: u64,
    modified_at_utc: Option<&str>,
    sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    let source = crate::storage::object_path_no_follow(&project.repo_root, object_id)?;
    let source_relative = source.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "object path is outside repository {}: {}",
            project.repo_root.display(),
            source.display()
        ))
    })?;
    let destination_relative = destination.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "transaction staged path is outside repository {}: {}",
            project.repo_root.display(),
            destination.display()
        ))
    })?;
    let mut input = anchored_repo
        .open_file(source_relative)
        .map_err(|error| match error {
            CheckPoError::Io { source, .. } if source.kind() == ErrorKind::NotFound => {
                CheckPoError::ObjectMissing(object_id.to_string())
            }
            CheckPoError::Corruption(message) => CheckPoError::ObjectHashMismatch(message),
            error => error,
        })?;
    let (destination_parent, destination_leaf) =
        anchored_repo.open_parent_batched(destination_relative, false, sync_batch)?;
    let mut output = destination_parent.create_new_file(&destination_leaf)?;
    let result = (|| -> Result<()> {
        let copied = input.copy_and_hash_to_profiled_with_cancellation(
            &mut output,
            destination,
            None,
            cancellation,
        )?;
        if copied.metadata.len() != size_bytes || &copied.object_id != object_id {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "{} expected {} bytes with hash {}, got {} bytes with hash {}",
                source.display(),
                size_bytes,
                object_id,
                copied.metadata.len(),
                copied.object_id
            )));
        }
        if let Some(modified) = parse_mtime(modified_at_utc)? {
            output.set_mtime(modified)?;
        }
        output.sync_all()?;
        let staged_metadata = output.metadata()?;
        if staged_metadata.len() != size_bytes {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "{} size expected {}, got {}",
                destination.display(),
                size_bytes,
                staged_metadata.len()
            )));
        }
        verify_file_mtime(&staged_metadata, destination, modified_at_utc)?;
        anchored_repo.verify_binding(source_relative, &input)?;
        destination_parent.verify_file_binding(&destination_leaf, &output)?;
        anchored_repo.verify_root_binding()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = destination_parent.unlink_file_if_bound(&destination_leaf, output);
        return result;
    }
    sync_batch.record(destination_parent)
}

/// On Windows, moves the verified source to the transaction backup through its
/// held handle. On Unix and portable fallbacks, creates and verifies a durable
/// held-source clone/copy, then returns the source and its hash-time version for
/// identity-bound cleanup after the backup parent barrier.
///
/// Returns the held source and its hash-time version when the source remains and
/// must be removed after the backup parent barrier.
pub(super) fn backup_project_file_deferred(
    project: &ProjectContext,
    anchored_project: &crate::storage::AnchoredRoot,
    anchored_repo: &crate::storage::AnchoredRoot,
    operation: &FileOperation,
    backup_path: &Path,
    backup_sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
    _project_sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
) -> Result<Option<DeferredBackupSource>> {
    let expected_hash = required_before_hash(operation)?;
    let expected_size = operation.before_size_bytes.ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "backup operation missing before size for {}",
            operation.path
        ))
    })?;
    let project_relative = Path::new(operation.path.as_str());
    let (project_parent, project_leaf) =
        anchored_project.open_parent_for_mutation(project_relative, false)?;
    let mut source = project_parent.open_file(&project_leaf)?;
    let hashed = source.hash()?;
    if &hashed.object_id != expected_hash || hashed.metadata.len() != expected_size {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    verify_file_mtime(
        &hashed.metadata,
        &operation
            .path
            .to_project_path(project.project_root.as_path()),
        operation.before_modified_at_utc.as_deref(),
    )?;
    project_parent.verify_file_binding(&project_leaf, &source)?;
    #[cfg(windows)]
    {
        // A FileId alone does not protect against in-place writes. Reopen the
        // verified source without FILE_SHARE_WRITE and keep that guard through
        // backup publication and the later identity-bound removal.
        source = project_parent.open_file_without_write_sharing(&project_leaf, &source)?;
        source.verify_version(&hashed.version)?;
    }

    let backup_relative = backup_path.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "transaction backup is outside repository {}: {}",
            project.repo_root.display(),
            backup_path.display()
        ))
    })?;
    let (backup_parent, backup_leaf) =
        anchored_repo.open_parent_batched(backup_relative, true, backup_sync_batch)?;
    let project_parent_relative = project_relative.parent().unwrap_or_else(|| Path::new(""));
    let backup_parent_relative = backup_relative.parent().unwrap_or_else(|| Path::new(""));
    anchored_project.verify_parent_binding(project_parent_relative, &project_parent)?;
    anchored_repo.verify_parent_binding(backup_parent_relative, &backup_parent)?;

    // Publish a private, fully verified backup before removing the source.
    // Windows deliberately uses this path too: its no-write-sharing source
    // guard closes the same-FileId write race, while the durable copy supports
    // read-only project files without weakening identity checks.
    let mut output = source
        .clone_or_copy_to_new(&backup_parent, &backup_leaf, backup_path)
        .map_err(|error| map_destination_exists_to_working_tree_changed(error, &operation.path))?;
    let copy_result = (|| -> Result<()> {
        let modified = hashed
            .metadata
            .modified()
            .map_err(|error| crate::io_error(backup_path, error))?;
        output.set_mtime(modified)?;
        output.sync_all()?;
        let readback = output.hash()?;
        if readback.object_id != *expected_hash || readback.metadata.len() != expected_size {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "backup readback mismatch for {}",
                operation.path
            )));
        }
        verify_file_mtime(
            &readback.metadata,
            backup_path,
            operation.before_modified_at_utc.as_deref(),
        )?;
        backup_parent.verify_file_binding(&backup_leaf, &output)?;
        project_parent.verify_file_binding(&project_leaf, &source)?;
        source.verify_version(&hashed.version)?;
        anchored_project.verify_parent_binding(project_parent_relative, &project_parent)?;
        anchored_repo.verify_parent_binding(backup_parent_relative, &backup_parent)?;
        anchored_project.verify_root_binding()?;
        anchored_repo.verify_root_binding()
    })();
    match copy_result {
        Ok(()) => {
            backup_sync_batch.record(backup_parent)?;
            Ok(Some(DeferredBackupSource {
                path: operation.path.clone(),
                source,
                version: hashed.version,
            }))
        }
        Err(error) => {
            let _ = backup_parent.unlink_file_if_bound(&backup_leaf, output);
            Err(error)
        }
    }
}

pub(super) fn remove_deferred_backup_source(
    anchored_project: &crate::storage::AnchoredRoot,
    deferred: DeferredBackupSource,
    source_sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
) -> Result<()> {
    let relative = Path::new(deferred.path.as_str());
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let (parent, leaf) = anchored_project.open_parent_for_mutation(relative, false)?;
    anchored_project.verify_parent_binding(parent_relative, &parent)?;
    parent.unlink_file_if_bound_versioned(&leaf, deferred.source, deferred.version)?;
    source_sync_batch.record(parent)
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

#[cfg(test)]
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
    finish_created_backup(project, operation, source, backup_path, expected_hash)?;
    ensure_project_parent_is_safe(project, &operation.path)
}

#[cfg(test)]
fn finish_created_backup(
    project: &ProjectContext,
    operation: &FileOperation,
    source: &Path,
    backup_path: &Path,
    expected_hash: &ObjectId,
) -> Result<()> {
    if let Err(error) = verify_path_hash(&project.repo_root, backup_path, expected_hash) {
        let _ = fs::remove_file(backup_path);
        return Err(CheckPoError::WorkingTreeChanged(format!(
            "{} (backup verification failed: {error})",
            operation.path,
        )));
    }
    let quarantine_path = temporary_project_backup_path(source)?;
    crate::storage::move_file_to_tombstone(source, &quarantine_path)?;
    let canonical_quarantine_path = operation
        .path
        .to_project_path(project.project_root.as_path())
        .with_file_name(quarantine_path.file_name().ok_or_else(|| {
            CheckPoError::InvalidTrackedPath(quarantine_path.display().to_string())
        })?);
    if let Err(error) = verify_path_hash(
        project.project_root.as_path(),
        &canonical_quarantine_path,
        expected_hash,
    ) {
        let _ = fs::remove_file(backup_path);
        restore_quarantined_project_file(source, &quarantine_path)?;
        return Err(CheckPoError::WorkingTreeChanged(format!(
            "{} (quarantine verification failed: {error})",
            operation.path,
        )));
    }
    crate::storage::remove_file_durable(&quarantine_path)
}

#[cfg(test)]
fn temporary_project_backup_path(source: &Path) -> Result<PathBuf> {
    source.file_name().ok_or_else(|| {
        CheckPoError::InvalidTrackedPath(format!("invalid path: {}", source.display()))
    })?;
    Ok(source.with_file_name(format!(".checkpo-{}.tmp", Uuid::new_v4().simple())))
}

#[cfg(test)]
fn restore_quarantined_project_file(source: &Path, quarantine_path: &Path) -> Result<()> {
    if source.exists() {
        return Err(CheckPoError::WorkingTreeChanged(
            source.display().to_string(),
        ));
    }
    crate::storage::move_file_to_tombstone(quarantine_path, source)
}

/// Publishes a Restore operation (a path with no before-state) while deferring
/// parent-directory barriers. On a shared filesystem the already-synced staged
/// inode is moved directly with an exclusive rename. Across filesystems, a
/// verified adjacent temporary copy is synced once and then published.
///
/// Returns true when the staged source remains and must be removed after the
/// project-directory barrier has completed.
pub(super) fn restore_new_staged_file_to_project_deferred(
    project: &ProjectContext,
    operation: &FileOperation,
    staged: &Path,
    destination: &Path,
    transaction_id: &str,
    staged_sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
    project_sync_batch: &mut crate::storage::AnchoredParentSyncBatch,
) -> Result<bool> {
    if !matches!(
        operation.operation_type,
        FileOperationType::Restore | FileOperationType::Replace
    ) {
        return Err(CheckPoError::Corruption(format!(
            "deferred staged publish received an unsupported operation for {}",
            operation.path
        )));
    }
    let expected_hash = required_after_hash(operation)?;
    let expected_size = operation.after_size_bytes.ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "restore operation missing after size for {}",
            operation.path
        ))
    })?;
    let staged_relative = staged.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "staged restore source is outside repository {}: {}",
            project.repo_root.display(),
            staged.display()
        ))
    })?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let (staged_parent, staged_leaf) =
        anchored_repo.open_parent_for_mutation(staged_relative, false)?;
    let mut staged_file = staged_parent
        .open_file(&staged_leaf)
        .map_err(|error| match error {
            CheckPoError::Corruption(message) => CheckPoError::ObjectHashMismatch(message),
            error => error,
        })?;
    let staged_hash = staged_file.hash()?;
    if staged_hash.metadata.len() != expected_size || &staged_hash.object_id != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {} bytes with hash {}, got {} bytes with hash {}",
            staged.display(),
            expected_size,
            expected_hash,
            staged_hash.metadata.len(),
            staged_hash.object_id
        )));
    }
    verify_file_mtime(
        &staged_hash.metadata,
        staged,
        operation.after_modified_at_utc.as_deref(),
    )?;
    staged_parent.verify_file_binding(&staged_leaf, &staged_file)?;
    anchored_repo.verify_root_binding()?;
    let project_relative = Path::new(operation.path.as_str());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (destination_parent, destination_leaf) =
        anchored_project.open_parent_batched(project_relative, true, project_sync_batch)?;
    let project_parent_relative = project_relative.parent().unwrap_or_else(|| Path::new(""));
    anchored_project.verify_parent_binding(project_parent_relative, &destination_parent)?;
    match staged_parent.rename_no_replace_to(
        &staged_leaf,
        &staged_file,
        &destination_parent,
        &destination_leaf,
    ) {
        Ok(()) => {
            if let Err(error) =
                anchored_project.verify_parent_binding(project_parent_relative, &destination_parent)
            {
                let _ = destination_parent.rename_no_replace_to(
                    &destination_leaf,
                    &staged_file,
                    &staged_parent,
                    &staged_leaf,
                );
                return Err(error);
            }
            project_sync_batch.record(destination_parent)?;
            staged_sync_batch.record(staged_parent)?;
            anchored_project.verify_root_binding()?;
            anchored_repo.verify_root_binding()?;
            return Ok(false);
        }
        Err(error) if is_cross_device_error(&error) => {}
        Err(error) => {
            return Err(map_destination_exists_to_working_tree_changed(
                error,
                &operation.path,
            ))
        }
    }

    let temporary_path =
        transaction_materialization_temp_path(destination, &operation.path, transaction_id)?;
    let temporary_leaf = temporary_path
        .file_name()
        .ok_or_else(|| CheckPoError::InvalidTrackedPath(temporary_path.display().to_string()))?;
    let mut output = destination_parent.create_new_file(temporary_leaf)?;
    let copy_result = (|| -> Result<()> {
        let copied = staged_file.copy_and_hash_to(&mut output, &temporary_path)?;
        if copied.metadata.len() != expected_size || &copied.object_id != expected_hash {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "{} expected {} bytes with hash {}, got {} bytes with hash {}",
                staged.display(),
                expected_size,
                expected_hash,
                copied.metadata.len(),
                copied.object_id
            )));
        }
        if let Some(modified) = parse_mtime(operation.after_modified_at_utc.as_deref())? {
            output.set_mtime(modified)?;
        }
        output.sync_all()?;
        let readback = output.hash()?;
        if readback.metadata.len() != expected_size || &readback.object_id != expected_hash {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "{} readback expected {} bytes with hash {}, got {} bytes with hash {}",
                temporary_path.display(),
                expected_size,
                expected_hash,
                readback.metadata.len(),
                readback.object_id
            )));
        }
        verify_file_mtime(
            &readback.metadata,
            &temporary_path,
            operation.after_modified_at_utc.as_deref(),
        )?;
        destination_parent.verify_file_binding(temporary_leaf, &output)?;
        staged_parent.verify_file_binding(&staged_leaf, &staged_file)?;
        anchored_project.verify_parent_binding(project_parent_relative, &destination_parent)?;
        destination_parent
            .rename_no_replace_to(
                temporary_leaf,
                &output,
                &destination_parent,
                &destination_leaf,
            )
            .map_err(|error| {
                map_destination_exists_to_working_tree_changed(error, &operation.path)
            })?;
        anchored_project.verify_parent_binding(project_parent_relative, &destination_parent)?;
        anchored_project.verify_root_binding()?;
        staged_parent.verify_file_binding(&staged_leaf, &staged_file)?;
        anchored_repo.verify_root_binding()
    })();
    match copy_result {
        Ok(()) => {
            project_sync_batch.record(destination_parent)?;
            Ok(true)
        }
        Err(error) => {
            let cleanup_leaf = if destination_parent
                .verify_file_binding(&destination_leaf, &output)
                .is_ok()
            {
                destination_leaf.as_os_str()
            } else {
                temporary_leaf
            };
            let _ = destination_parent.unlink_file_if_bound(cleanup_leaf, output);
            Err(error)
        }
    }
}

pub(super) fn copy_backup_file_to_project(
    project: &ProjectContext,
    operation: &FileOperation,
    backup_path: &Path,
    destination: &Path,
    transaction_id: &str,
) -> Result<()> {
    let expected = required_before_hash(operation)?;
    let expected_size = operation.before_size_bytes.ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "recovery operation missing before size for {}",
            operation.path
        ))
    })?;
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let backup_relative = backup_path.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "backup path is outside repository: {}",
            backup_path.display()
        ))
    })?;
    let (backup_parent, backup_leaf) = repo.open_parent(backup_relative, false)?;
    let mut source = backup_parent.open_file(&backup_leaf)?;
    let source_hash = source.hash()?;
    if &source_hash.object_id != expected || source_hash.metadata.len() != expected_size {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "backup mismatch for {}",
            operation.path
        )));
    }
    verify_file_mtime(
        &source_hash.metadata,
        backup_path,
        operation.before_modified_at_utc.as_deref(),
    )?;

    let project_root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(operation.path.as_str());
    let (destination_parent, destination_leaf) =
        project_root.open_parent_for_mutation(relative, false)?;
    let temporary_path =
        transaction_materialization_temp_path(destination, &operation.path, transaction_id)?;
    let temporary_leaf = temporary_path
        .file_name()
        .ok_or_else(|| CheckPoError::InvalidTrackedPath(temporary_path.display().to_string()))?;
    let mut output = destination_parent.create_new_file(temporary_leaf)?;
    let result = (|| -> Result<()> {
        let copied = source.copy_and_hash_to(&mut output, &temporary_path)?;
        if copied.object_id != *expected || copied.metadata.len() != expected_size {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "backup copy mismatch for {}",
                operation.path
            )));
        }
        if let Some(modified) = parse_mtime(operation.before_modified_at_utc.as_deref())? {
            output.set_mtime(modified)?;
        }
        output.sync_all()?;
        let readback = output.hash()?;
        if readback.object_id != *expected || readback.metadata.len() != expected_size {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "backup recovery readback mismatch for {}",
                operation.path
            )));
        }
        verify_file_mtime(
            &readback.metadata,
            &temporary_path,
            operation.before_modified_at_utc.as_deref(),
        )?;
        backup_parent.verify_file_binding(&backup_leaf, &source)?;
        destination_parent.rename_no_replace_to(
            temporary_leaf,
            &output,
            &destination_parent,
            &destination_leaf,
        )?;
        destination_parent.sync_all()?;
        project_root.verify_root_binding()?;
        repo.verify_root_binding()
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let cleanup_leaf = if destination_parent
                .verify_file_binding(&destination_leaf, &output)
                .is_ok()
            {
                destination_leaf.as_os_str()
            } else {
                temporary_leaf
            };
            let _ = destination_parent.unlink_file_if_bound(cleanup_leaf, output);
            Err(error)
        }
    }
}

pub(super) fn transaction_materialization_temp_path(
    destination: &Path,
    path: &TrackedUnityFilePath,
    transaction_id: &str,
) -> Result<PathBuf> {
    destination.file_name().ok_or_else(|| {
        CheckPoError::InvalidTrackedPath(format!("invalid path: {}", destination.display()))
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(transaction_id.as_bytes());
    hasher.update(&[0]);
    hasher.update(path.as_str().as_bytes());
    let digest = hasher.finalize().to_hex().to_string();
    Ok(destination.with_file_name(format!(".checkpo-{}.tmp", &digest[..32])))
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

fn is_cross_device_error(error: &CheckPoError) -> bool {
    let CheckPoError::Io { source, .. } = error else {
        return false;
    };
    #[cfg(unix)]
    if source.raw_os_error() == Some(libc::EXDEV) {
        return true;
    }
    #[cfg(windows)]
    if source.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE as i32) {
        return true;
    }
    false
}

pub(super) fn remove_project_directory(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(path.as_str());
    let (parent, leaf) = anchored_project.open_parent_for_mutation(relative, false)?;
    parent.unlink_dir(&leaf).map_err(|error| match &error {
        CheckPoError::Io { source, .. } if source.kind() == ErrorKind::DirectoryNotEmpty => {
            CheckPoError::WorkingTreeChanged(path.to_string())
        }
        _ => error,
    })?;
    parent.sync_all()?;
    anchored_project.verify_root_binding()
}

pub(super) fn create_project_directory(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(path.as_str());
    let (parent, leaf) = anchored_project.open_parent_for_mutation(relative, false)?;
    parent
        .create_directory(&leaf)
        .map_err(|error| match &error {
            CheckPoError::Io { source, .. } if source.kind() == ErrorKind::AlreadyExists => {
                CheckPoError::WorkingTreeChanged(path.to_string())
            }
            _ => error,
        })?;
    parent.sync_all()?;
    anchored_project.verify_root_binding()
}

pub(super) fn ensure_project_directory_exists_for_recovery(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    match anchored_project.open_directory(Path::new(path.as_str()), false) {
        Ok(_) => anchored_project.verify_root_binding(),
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            create_project_directory(project, path)
        }
        Err(CheckPoError::Corruption(_)) => Err(CheckPoError::WorkingTreeChanged(path.to_string())),
        Err(error) => Err(error),
    }
}

pub(super) fn verify_path_hash(root: &Path, path: &Path, expected_hash: &ObjectId) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| crate::io_error(path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let relative = path.strip_prefix(root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "verified path is outside anchored root {}: {}",
            root.display(),
            path.display()
        ))
    })?;
    let anchored_root = crate::storage::AnchoredRoot::open(root)?;
    let mut file = anchored_root.open_file(relative)?;
    let actual = file.hash()?.object_id;
    if &actual != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected_hash,
            actual
        )));
    }
    anchored_root.verify_binding(relative, &file)?;
    anchored_root.verify_root_binding()?;
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

fn parse_mtime(value: Option<&str>) -> Result<Option<std::time::SystemTime>> {
    value
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&chrono::Utc).into())
                .map_err(|error| CheckPoError::Corruption(error.to_string()))
        })
        .transpose()
}

pub(super) fn verify_file_mtime(
    metadata: &fs::Metadata,
    path: &Path,
    expected_modified_at_utc: Option<&str>,
) -> Result<()> {
    let Some(expected_modified_at_utc) = expected_modified_at_utc else {
        return Ok(());
    };
    let actual = metadata
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(path, error))?;
    if actual != expected_modified_at_utc {
        return Err(CheckPoError::Corruption(format!(
            "mtime readback mismatch for {}: expected {}, got {}",
            path.display(),
            expected_modified_at_utc,
            actual
        )));
    }
    Ok(())
}

pub(super) fn set_project_file_mtime(
    project: &ProjectContext,
    operation: &FileOperation,
    target_modified_at_utc: &str,
) -> Result<()> {
    let path = operation
        .path
        .to_project_path(project.project_root.as_path());
    let relative = Path::new(operation.path.as_str());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let mut file = anchored_project.open_file_read_write(relative)?;
    let hashed = file.hash()?;
    let current_modified_at_utc = hashed
        .metadata
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(&path, error))?;
    if operation.before_hash.as_ref() != Some(&hashed.object_id)
        || operation.before_size_bytes != Some(hashed.metadata.len())
        || operation.before_modified_at_utc.as_deref() != Some(current_modified_at_utc.as_str())
    {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let time = chrono::DateTime::parse_from_rfc3339(target_modified_at_utc)
        .map_err(|error| CheckPoError::Corruption(error.to_string()))?
        .with_timezone(&chrono::Utc);
    file.set_mtime(time.into())?;
    file.sync_all()?;
    let readback = file
        .metadata()?
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(&path, error))?;
    if readback != target_modified_at_utc {
        return Err(CheckPoError::Corruption(format!(
            "metadata readback mismatch for {}: expected {}, got {}",
            operation.path, target_modified_at_utc, readback
        )));
    }
    anchored_project.verify_binding(relative, &file)?;
    anchored_project.verify_root_binding()
}

pub(super) fn recover_project_file_mtime(
    project: &ProjectContext,
    operation: &FileOperation,
) -> Result<()> {
    let path = operation
        .path
        .to_project_path(project.project_root.as_path());
    let relative = Path::new(operation.path.as_str());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let mut file = anchored_project.open_file_read_write(relative)?;
    let hashed = file.hash()?;
    if operation.before_hash.as_ref() != Some(&hashed.object_id)
        || operation.before_size_bytes != Some(hashed.metadata.len())
    {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let current_modified_at_utc = hashed
        .metadata
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(&path, error))?;
    let before = operation.before_modified_at_utc.as_deref().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "metadata operation missing before mtime for {}",
            operation.path
        ))
    })?;
    let after = operation.after_modified_at_utc.as_deref().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "metadata operation missing after mtime for {}",
            operation.path
        ))
    })?;
    if current_modified_at_utc == before {
        anchored_project.verify_binding(relative, &file)?;
        anchored_project.verify_root_binding()?;
        return Ok(());
    }
    if current_modified_at_utc != after {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let time = chrono::DateTime::parse_from_rfc3339(before)
        .map_err(|error| CheckPoError::Corruption(error.to_string()))?
        .with_timezone(&chrono::Utc);
    file.set_mtime(time.into())?;
    file.sync_all()?;
    let readback = file
        .metadata()?
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(&path, error))?;
    if readback != before {
        return Err(CheckPoError::Corruption(format!(
            "metadata recovery readback mismatch for {}: expected {}, got {}",
            operation.path, before, readback
        )));
    }
    anchored_project.verify_binding(relative, &file)?;
    anchored_project.verify_root_binding()
}

pub(super) fn restore_before_mtime_for_recovery(
    project: &ProjectContext,
    operation: &FileOperation,
) -> Result<()> {
    let expected_hash = required_before_hash(operation)?;
    let expected_size = operation.before_size_bytes.ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "recovery operation missing before size for {}",
            operation.path
        ))
    })?;
    let before = operation.before_modified_at_utc.as_deref().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "recovery operation missing before mtime for {}",
            operation.path
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(operation.path.as_str());
    let mut file = root.open_file_read_write(relative)?;
    let hashed = file.hash()?;
    if &hashed.object_id != expected_hash || hashed.metadata.len() != expected_size {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    if let Some(modified) = parse_mtime(Some(before))? {
        file.set_mtime(modified)?;
    }
    file.sync_all()?;
    verify_file_mtime(
        &file.metadata()?,
        &operation
            .path
            .to_project_path(project.project_root.as_path()),
        Some(before),
    )?;
    root.verify_binding(relative, &file)?;
    root.verify_root_binding()
}
