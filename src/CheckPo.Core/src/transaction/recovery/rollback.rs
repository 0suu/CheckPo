use super::*;

pub(super) fn recover_one(project: &ProjectContext, pending: &PendingTransaction) -> Result<()> {
    let tx_root = pending
        .journal_path
        .parent()
        .ok_or_else(|| CheckPoError::Corruption("invalid journal path".into()))?;
    ensure_regular_transaction_directory(tx_root)?;
    if pending.state == JOURNAL_STATE_UNREADABLE {
        let quarantine_path = quarantine_unknown_transaction_locked(
            project,
            tx_root,
            &pending.journal_path,
            &pending.transaction_id,
            "transaction journal is unreadable",
        )?;
        return Err(CheckPoError::Corruption(format!(
            "unreadable transaction was quarantined at {}",
            quarantine_path.display()
        )));
    }
    match fs::symlink_metadata(&pending.journal_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            let quarantine_path = quarantine_unknown_transaction_locked(
                project,
                tx_root,
                &pending.journal_path,
                &pending.transaction_id,
                "transaction journal is not a regular file",
            )?;
            return Err(CheckPoError::Corruption(format!(
                "transaction with a non-regular journal was quarantined at {}",
                quarantine_path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let quarantine_path = quarantine_unknown_transaction_locked(
                project,
                tx_root,
                &pending.journal_path,
                &pending.transaction_id,
                "transaction journal is missing",
            )?;
            return Err(CheckPoError::Corruption(format!(
                "transaction with a missing journal was quarantined at {}",
                quarantine_path.display()
            )));
        }
        Err(error) => return Err(crate::io_error(&pending.journal_path, error)),
    }
    let mut journal = read_transaction_journal(&pending.journal_path)?;
    validate_transaction_journal_identity(tx_root, &journal)?;
    if journal.operations.is_empty() {
        return Err(CheckPoError::Corruption(
            "transaction journal contains no operations".to_string(),
        ));
    }
    validate_journal_operations(project, &journal.checkpoint_id, &journal.operations)?;
    validate_journal_directory_topology(
        &journal.operations,
        &journal.directories_to_remove,
        &journal.directories_to_create,
    )?;
    let backup_root = tx_root.join("backup");
    let staged_root = tx_root.join("staged");
    let backup_paths = validate_transaction_payload(
        &backup_root,
        journal
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Delete | FileOperationType::Replace
                )
            })
            .map(|operation| operation.path.clone())
            .collect(),
    )?;
    let staged_paths = validate_transaction_payload(
        &staged_root,
        journal
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Restore | FileOperationType::Replace
                )
            })
            .map(|operation| operation.path.clone())
            .collect(),
    )?;

    cleanup_transaction_materialization_temps(project, &journal)?;

    if transaction_needs_rollback(project, &journal, &backup_paths, &staged_paths)? {
        invalidate_operation_fingerprints(project, &journal.operations)?;
        recover_topology_transaction(project, &backup_root, &journal)?;
    }
    remove_repository_tree_if_exists(&project.repo_root, &staged_root)?;
    journal.state = JournalState::Recovered;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&pending.journal_path, &journal)?;
    Ok(())
}

pub(in crate::transaction) fn remove_repository_tree_if_exists(
    repo_root: &Path,
    directory: &Path,
) -> Result<()> {
    let relative = directory.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "transaction cleanup is outside repository: {}",
            directory.display()
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(repo_root)?;
    let (parent, leaf) = match root.open_parent_for_mutation(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    let tree = match parent.open_directory_for_mutation(&leaf) {
        Ok(tree) => tree,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    tree.remove_tree_contents()?;
    drop(tree);
    parent.unlink_dir(&leaf)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

fn cleanup_transaction_materialization_temps(
    project: &ProjectContext,
    journal: &TransactionJournal,
) -> Result<()> {
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    for operation in journal.operations.iter().filter(|operation| {
        matches!(
            operation.operation_type,
            FileOperationType::Restore | FileOperationType::Replace
        )
    }) {
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        let temporary = transaction_materialization_temp_path(
            &destination,
            &operation.path,
            &journal.transaction_id,
        )?;
        let relative = temporary
            .strip_prefix(project.project_root.as_path())
            .map_err(|_| {
                CheckPoError::Corruption(format!(
                    "transaction materialization temp is outside project: {}",
                    temporary.display()
                ))
            })?;
        let (parent, leaf) = match anchored_project.open_parent_for_mutation(relative, false) {
            Ok(value) => value,
            Err(CheckPoError::Io { source, .. })
                if matches!(
                    source.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        match parent.open_file(&leaf) {
            Ok(file) => {
                parent.unlink_file_if_bound(&leaf, file)?;
                parent.sync_all()?;
            }
            Err(CheckPoError::Corruption(_)) => {
                return Err(CheckPoError::Corruption(format!(
                    "transaction materialization temp is not a regular file: {}",
                    temporary.display()
                )))
            }
            Err(CheckPoError::Io { source, .. })
                if matches!(
                    source.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error),
        }
    }
    anchored_project.verify_root_binding()
}

pub(super) fn ensure_regular_transaction_directory(tx_root: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(tx_root).map_err(|error| crate::io_error(tx_root, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction root is not a regular directory: {}",
            tx_root.display()
        )));
    }
    Ok(())
}

pub(super) fn validate_transaction_payload(
    root: &Path,
    allowed_paths: BTreeSet<TrackedUnityFilePath>,
) -> Result<BTreeSet<TrackedUnityFilePath>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(crate::io_error(root, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction payload root is not a regular directory: {}",
            root.display()
        )));
    }

    let mut present = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| CheckPoError::Corruption(error.to_string()))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        let file_type = metadata.file_type();
        if crate::metadata_is_link_or_reparse(&metadata) {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a symlink: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            continue;
        }
        if !file_type.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a non-regular file: {}",
                entry.path().display()
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| CheckPoError::Corruption(error.to_string()))?;
        let relative = relative.to_str().ok_or_else(|| {
            CheckPoError::Corruption(format!(
                "transaction payload path is not valid UTF-8: {}",
                entry.path().display()
            ))
        })?;
        let path = TrackedUnityFilePath::parse(&relative.replace('\\', "/"))?;
        if allowed_paths.contains(&path) {
            present.insert(path);
            continue;
        }
        if crate::is_checkpo_atomic_materialization_temporary_file(entry.path()) {
            continue;
        }
        return Err(CheckPoError::Corruption(format!(
            "transaction payload contains an unexpected path: {path}"
        )));
    }
    Ok(present)
}

fn transaction_needs_rollback(
    project: &ProjectContext,
    journal: &TransactionJournal,
    backup_paths: &BTreeSet<TrackedUnityFilePath>,
    staged_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<bool> {
    if journal.state == JournalState::Applying || !backup_paths.is_empty() {
        return Ok(true);
    }
    if journal.state != JournalState::Staged {
        return Ok(false);
    }
    for operation in &journal.operations {
        let Some(after_hash) = operation.after_hash.as_ref() else {
            continue;
        };
        if !staged_paths.contains(&operation.path)
            && current_hash(project, &operation.path)?.as_ref() == Some(after_hash)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_before_state_restored(
    project: &ProjectContext,
    operations: &[FileOperation],
) -> Result<()> {
    for operation in operations {
        let current = current_file_state_for_recovery(
            project,
            &operation.path,
            &journal_before_paths(operations),
        )?;
        if current.as_ref().map(|state| &state.hash) != operation.before_hash.as_ref()
            || current.as_ref().map(|state| state.size_bytes) != operation.before_size_bytes
            || current.as_ref().map(|state| state.modified_at_utc.as_str())
                != operation.before_modified_at_utc.as_deref()
        {
            return Err(CheckPoError::Corruption(format!(
                "transaction recovery did not restore before state for {}",
                operation.path
            )));
        }
    }
    Ok(())
}

pub(super) fn journal_before_paths(operations: &[FileOperation]) -> BTreeSet<TrackedUnityFilePath> {
    operations
        .iter()
        .filter(|operation| operation.before_hash.is_some())
        .map(|operation| operation.path.clone())
        .collect()
}

fn recover_topology_transaction(
    project: &ProjectContext,
    backup_root: &Path,
    journal: &TransactionJournal,
) -> Result<()> {
    let before_paths = journal_before_paths(&journal.operations);
    let transaction_root = backup_root.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!("invalid backup root: {}", backup_root.display()))
    })?;
    let recovery_after_root = transaction_root.join("recovery-after");
    for operation in journal.operations.iter().filter(|operation| {
        matches!(
            operation.operation_type,
            FileOperationType::Restore | FileOperationType::Replace
        )
    }) {
        if journal.kind == OperationPlanKind::Discard {
            super::plan::ensure_discard_folder_meta_operation_is_safe(project, &operation.path)?;
        }
        let Some(after_hash) = operation.after_hash.as_ref() else {
            continue;
        };
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        remove_existing_held_after_file(
            project,
            operation,
            &destination,
            &journal.transaction_id,
            after_hash,
            &recovery_after_root,
        )?;
        match current_hash_for_recovery(project, &operation.path, &before_paths)? {
            Some(current) if &current == after_hash => {
                remove_after_file_for_recovery(
                    project,
                    operation,
                    &destination,
                    &journal.transaction_id,
                    after_hash,
                    &recovery_after_root,
                )?;
            }
            Some(current) if operation.before_hash.as_ref() == Some(&current) => {}
            None => {}
            Some(_) => return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string())),
        }
        if journal.kind == OperationPlanKind::Discard {
            super::plan::ensure_discard_folder_meta_operation_is_safe(project, &operation.path)?;
        }
    }

    let mut created_directories = journal.directories_to_create.clone();
    created_directories.sort_by(|left, right| {
        right
            .as_str()
            .matches('/')
            .count()
            .cmp(&left.as_str().matches('/').count())
            .then_with(|| left.cmp(right))
    });
    for directory in &created_directories {
        let path = directory.to_project_path(project.project_root.as_path());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
                remove_project_directory(project, directory)?;
            }
            Ok(metadata)
                if metadata.is_file()
                    && !crate::metadata_is_link_or_reparse(&metadata)
                    && before_paths.contains(directory) => {}
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Ok(_) => return Err(CheckPoError::WorkingTreeChanged(directory.to_string())),
            Err(error) => return Err(crate::io_error(&path, error)),
        }
    }

    let mut removed_directories = journal.directories_to_remove.clone();
    removed_directories.sort();
    for directory in &removed_directories {
        ensure_project_directory_exists_for_recovery(project, directory)?;
    }

    let mut backed_up = journal
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.operation_type,
                FileOperationType::Delete | FileOperationType::Replace
            )
        })
        .collect::<Vec<_>>();
    backed_up.sort_by(|left, right| left.path.cmp(&right.path));
    for operation in backed_up {
        if journal.kind == OperationPlanKind::Discard {
            super::plan::ensure_discard_folder_meta_operation_is_safe(project, &operation.path)?;
        }
        recover_before_file(
            project,
            backup_root,
            operation,
            &before_paths,
            &journal.transaction_id,
        )?;
        if journal.kind == OperationPlanKind::Discard {
            super::plan::ensure_discard_folder_meta_operation_is_safe(project, &operation.path)?;
        }
    }
    for operation in journal
        .operations
        .iter()
        .filter(|operation| operation.operation_type == FileOperationType::SetMetadata)
    {
        recover_project_file_mtime(project, operation)?;
    }
    ensure_before_state_restored(project, &journal.operations)?;
    if journal.kind == OperationPlanKind::Discard {
        for operation in &journal.operations {
            super::plan::ensure_discard_folder_meta_operation_is_safe(project, &operation.path)?;
        }
    }
    Ok(())
}

fn recovery_after_path(
    destination: &Path,
    path: &TrackedUnityFilePath,
    transaction_id: &str,
) -> Result<PathBuf> {
    destination.file_name().ok_or_else(|| {
        CheckPoError::InvalidTrackedPath(format!("invalid path: {}", destination.display()))
    })?;
    let digest = blake3::hash(path.as_str().as_bytes()).to_hex();
    let held_name = format!(".checkpo-r-{}-{transaction_id}.tmp", &digest[..16]);
    Ok(destination.with_file_name(held_name))
}

fn existing_held_after_file(
    project: &ProjectContext,
    destination: &Path,
    path: &TrackedUnityFilePath,
    transaction_id: &str,
    expected_hash: &ObjectId,
) -> Result<Option<PathBuf>> {
    let held = recovery_after_path(destination, path, transaction_id)?;
    let relative = held
        .strip_prefix(project.project_root.as_path())
        .map_err(|_| {
            CheckPoError::Corruption(format!("invalid recovery quarantine: {}", held.display()))
        })?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (parent, leaf) = match root.open_parent(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. })
            if matches!(
                source.kind(),
                ErrorKind::NotFound | ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let mut file = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(CheckPoError::Corruption(_)) => {
            return Err(CheckPoError::Corruption(format!(
                "recovery quarantine is not a regular file: {}",
                held.display()
            )))
        }
        Err(error) => return Err(error),
    };
    let actual = file.hash()?.object_id;
    if &actual != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            held.display(),
            expected_hash,
            actual
        )));
    }
    parent.verify_file_binding(&leaf, &file)?;
    root.verify_root_binding()?;
    Ok(Some(held))
}

pub(super) fn preserve_after_file_for_recovery(
    project: &ProjectContext,
    source: &Path,
    expected_hash: &ObjectId,
    recovery_after_root: &Path,
) -> Result<()> {
    let preserved = recovery_after_root.join(expected_hash.as_str());
    let preserved_relative = preserved.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "recovery copy is outside repository: {}",
            preserved.display()
        ))
    })?;
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let (preserved_parent, preserved_leaf) =
        repo.open_parent_for_mutation(preserved_relative, true)?;
    match preserved_parent.open_file(&preserved_leaf) {
        Ok(mut file) => {
            let actual = file.hash()?.object_id;
            if &actual != expected_hash {
                return Err(CheckPoError::ObjectHashMismatch(format!(
                    "{} expected {}, got {}",
                    preserved.display(),
                    expected_hash,
                    actual
                )));
            }
            preserved_parent.verify_file_binding(&preserved_leaf, &file)?;
            return repo.verify_root_binding();
        }
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {}
        Err(CheckPoError::Corruption(_)) => {
            return Err(CheckPoError::Corruption(format!(
                "recovery copy is not a regular file: {}",
                preserved.display()
            )))
        }
        Err(error) => return Err(error),
    }

    let temporary_leaf = std::ffi::OsString::from(format!(
        ".checkpo-preserve-{}.tmp",
        &expected_hash.as_str()[..16]
    ));
    if let Ok(file) = preserved_parent.open_file(&temporary_leaf) {
        preserved_parent.unlink_file_if_bound(&temporary_leaf, file)?;
        preserved_parent.sync_all()?;
    }
    let source_relative = source
        .strip_prefix(project.project_root.as_path())
        .map_err(|_| {
            CheckPoError::Corruption(format!(
                "recovery source is outside project: {}",
                source.display()
            ))
        })?;
    let project_root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (source_parent, source_leaf) = project_root.open_parent(source_relative, false)?;
    let mut source_file = source_parent.open_file(&source_leaf)?;
    let source_hash = source_file.hash()?;
    if &source_hash.object_id != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            source.display(),
            expected_hash,
            source_hash.object_id
        )));
    }
    let mut output = preserved_parent.create_new_file(&temporary_leaf)?;
    let result = (|| -> Result<()> {
        let copied = source_file.copy_and_hash_to(&mut output, &preserved)?;
        if &copied.object_id != expected_hash {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery preservation copy mismatch: {}",
                source.display()
            )));
        }
        output.sync_all()?;
        let readback = output.hash()?;
        if &readback.object_id != expected_hash {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery preservation readback mismatch: {}",
                preserved.display()
            )));
        }
        source_parent.verify_file_binding(&source_leaf, &source_file)?;
        preserved_parent.rename_no_replace_to(
            &temporary_leaf,
            &output,
            &preserved_parent,
            &preserved_leaf,
        )?;
        preserved_parent.sync_all()?;
        project_root.verify_root_binding()?;
        repo.verify_root_binding()
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let cleanup_leaf = if preserved_parent
                .verify_file_binding(&preserved_leaf, &output)
                .is_ok()
            {
                preserved_leaf.as_os_str()
            } else {
                temporary_leaf.as_os_str()
            };
            let _ = preserved_parent.unlink_file_if_bound(cleanup_leaf, output);
            Err(error)
        }
    }
}

fn remove_existing_held_after_file(
    project: &ProjectContext,
    operation: &FileOperation,
    destination: &Path,
    transaction_id: &str,
    expected_hash: &ObjectId,
    recovery_after_root: &Path,
) -> Result<()> {
    let Some(held) = existing_held_after_file(
        project,
        destination,
        &operation.path,
        transaction_id,
        expected_hash,
    )?
    else {
        return Ok(());
    };
    preserve_after_file_for_recovery(project, &held, expected_hash, recovery_after_root)?;
    remove_anchored_project_file(project, &held, expected_hash)
}

pub(super) fn remove_anchored_project_file(
    project: &ProjectContext,
    path: &Path,
    expected_hash: &ObjectId,
) -> Result<()> {
    let relative = path
        .strip_prefix(project.project_root.as_path())
        .map_err(|_| {
            CheckPoError::Corruption(format!(
                "recovery removal is outside project: {}",
                path.display()
            ))
        })?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let (parent, leaf) = root.open_parent_for_mutation(relative, false)?;
    let mut file = parent.open_file(&leaf)?;
    let hashed = file.hash()?;
    if &hashed.object_id != expected_hash {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected_hash,
            hashed.object_id
        )));
    }
    parent.verify_file_binding(&leaf, &file)?;
    #[cfg(windows)]
    {
        file = parent.open_file_without_write_sharing(&leaf, &file)?;
        file.verify_version(&hashed.version)?;
    }
    root.verify_root_binding()?;
    parent.unlink_file_if_bound_versioned(&leaf, file, hashed.version)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

fn remove_after_file_for_recovery(
    project: &ProjectContext,
    operation: &FileOperation,
    destination: &Path,
    transaction_id: &str,
    expected_hash: &ObjectId,
    recovery_after_root: &Path,
) -> Result<()> {
    if existing_held_after_file(
        project,
        destination,
        &operation.path,
        transaction_id,
        expected_hash,
    )?
    .is_some()
    {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    preserve_after_file_for_recovery(project, destination, expected_hash, recovery_after_root)?;
    let held = recovery_after_path(destination, &operation.path, transaction_id)?;
    let root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(operation.path.as_str());
    let (parent, destination_leaf) = root.open_parent_for_mutation(relative, false)?;
    let held_leaf = held
        .file_name()
        .ok_or_else(|| CheckPoError::InvalidTrackedPath(held.display().to_string()))?;
    let mut file = parent.open_file(&destination_leaf)?;
    let actual = file.hash()?.object_id;
    if &actual != expected_hash {
        return Err(CheckPoError::WorkingTreeChanged(operation.path.to_string()));
    }
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    root.verify_parent_binding(parent_relative, &parent)?;
    parent.rename_no_replace_to(&destination_leaf, &file, &parent, held_leaf)?;
    if let Err(error) = root.verify_parent_binding(parent_relative, &parent) {
        let _ = parent.rename_no_replace_to(held_leaf, &file, &parent, &destination_leaf);
        return Err(error);
    }
    parent.sync_all()?;
    parent.unlink_file_if_bound(held_leaf, file)?;
    parent.sync_all()?;
    root.verify_root_binding()
}

fn recover_before_file(
    project: &ProjectContext,
    backup_root: &Path,
    operation: &FileOperation,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
    transaction_id: &str,
) -> Result<()> {
    let destination = operation
        .path
        .to_project_path(project.project_root.as_path());
    let backup_path = staged_path(backup_root, &operation.path);
    let expected = required_before_hash(operation)?;
    match current_hash_for_recovery(project, &operation.path, before_paths)? {
        Some(current) if &current == expected => {
            restore_before_mtime_for_recovery(project, operation)
        }
        None if backup_regular_file_exists(&backup_path)? => {
            verify_path_hash(&project.repo_root, &backup_path, expected)?;
            copy_backup_file_to_project(
                project,
                operation,
                &backup_path,
                &destination,
                transaction_id,
            )
        }
        Some(_) => Err(CheckPoError::WorkingTreeChanged(operation.path.to_string())),
        None => Err(CheckPoError::Corruption(format!(
            "backup missing for applied operation {}",
            operation.path
        ))),
    }
}

fn current_hash_for_recovery(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<Option<ObjectId>> {
    let full_path = path.to_project_path(project.project_root.as_path());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
            Err(CheckPoError::WorkingTreeChanged(path.to_string()))
        }
        Ok(metadata) if metadata.is_file() => {
            current_file_state_from_anchor(&anchored_project, path).map(|state| Some(state.hash))
        }
        Ok(metadata) if metadata.is_dir() => Ok(None),
        Ok(_) => Err(CheckPoError::WorkingTreeChanged(path.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error)
            if error.kind() == ErrorKind::NotADirectory
                && before_paths.iter().any(|candidate| {
                    path.as_str()
                        .strip_prefix(candidate.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/'))
                }) =>
        {
            Ok(None)
        }
        Err(error) => Err(crate::io_error(&full_path, error)),
    }
}

pub(super) fn current_file_state_for_recovery(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<Option<CurrentFileState>> {
    let full_path = path.to_project_path(project.project_root.as_path());
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
            Err(CheckPoError::WorkingTreeChanged(path.to_string()))
        }
        Ok(metadata) if metadata.is_file() => {
            current_file_state_from_anchor(&anchored_project, path).map(Some)
        }
        Ok(metadata) if metadata.is_dir() => Ok(None),
        Ok(_) => Err(CheckPoError::WorkingTreeChanged(path.to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error)
            if error.kind() == ErrorKind::NotADirectory
                && before_paths.iter().any(|candidate| {
                    path.as_str()
                        .strip_prefix(candidate.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/'))
                }) =>
        {
            Ok(None)
        }
        Err(error) => Err(crate::io_error(&full_path, error)),
    }
}

pub(in crate::transaction) fn invalidate_operation_fingerprints(
    project: &ProjectContext,
    operations: &[FileOperation],
) -> Result<()> {
    let paths = operations
        .iter()
        .map(|operation| operation.path.clone())
        .collect::<Vec<_>>();
    crate::invalidate_file_fingerprints(project, &paths)
}
