use super::anchored_fs::AnchoredFile;
use super::*;

#[cfg(test)]
pub(crate) fn put_object_from_file_with_known_hash(
    repo_root: &Path,
    source: &Path,
    object_id: &ObjectId,
    size_bytes: u64,
) -> Result<bool> {
    put_object_from_file_with_known_hash_profiled(repo_root, source, object_id, size_bytes, None)
}

#[cfg(test)]
pub(crate) fn put_object_from_file_with_known_hash_profiled(
    repo_root: &Path,
    source: &Path,
    object_id: &ObjectId,
    size_bytes: u64,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<bool> {
    let mut sync_batch = AnchoredParentSyncBatch::new();
    let result = put_object_from_file_with_known_hash_impl(
        repo_root,
        source,
        None,
        object_id,
        size_bytes,
        recorder,
        &mut sync_batch,
        None,
        || {},
    );
    match result {
        Ok(created) => {
            sync_batch.flush_with_progress(recorder, |_, _| Ok(()))?;
            if created {
                verify_file_hash_and_size_anchored(
                    repo_root,
                    &object_path(repo_root, object_id),
                    object_id,
                    size_bytes,
                )?;
            }
            Ok(created)
        }
        Err(error) => {
            let _ = sync_batch.flush_with_progress(recorder, |_, _| Ok(()));
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn put_object_from_anchored_file_with_known_hash_profiled_batched(
    repo_root: &Path,
    source_root: &AnchoredRoot,
    source_relative: &Path,
    source_display: &Path,
    object_id: &ObjectId,
    size_bytes: u64,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: &mut AnchoredParentSyncBatch,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<bool> {
    put_object_from_file_with_known_hash_impl(
        repo_root,
        source_display,
        Some((source_root, source_relative)),
        object_id,
        size_bytes,
        recorder,
        sync_batch,
        cancellation,
        || {},
    )
}

#[allow(clippy::too_many_arguments)]
fn put_object_from_file_with_known_hash_impl<F>(
    repo_root: &Path,
    source: &Path,
    anchored_source: Option<(&AnchoredRoot, &Path)>,
    object_id: &ObjectId,
    size_bytes: u64,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: &mut AnchoredParentSyncBatch,
    cancellation: Option<&crate::CancellationToken>,
    after_destination_parent_opened: F,
) -> Result<bool>
where
    F: FnOnce(),
{
    if let Some(recorder) = recorder {
        recorder.checked(size_bytes);
    }
    let destination = object_path(repo_root, object_id);
    let relative = destination.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "object path is outside repository {}: {}",
            repo_root.display(),
            destination.display()
        ))
    })?;
    let parent_relative = relative.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!("invalid object path: {}", destination.display()))
    })?;
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let (destination_parent, destination_leaf) = measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::DirectoryPrepare,
        || anchored_repo.open_parent_batched_profiled(relative, true, sync_batch, recorder),
    )
    .map_err(|error| match &error {
        CheckPoError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotADirectory => {
            CheckPoError::Corruption(format!(
                "object shard is not a no-follow directory: {}",
                destination.display()
            ))
        }
        _ => error,
    })?;
    after_destination_parent_opened();

    let mut repair_destination = None;
    match measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::ExistenceCheck,
        || destination_parent.open_file(&destination_leaf),
    ) {
        Ok(mut existing) => {
            let verification = verify_anchored_object(
                &mut existing,
                &destination,
                object_id,
                size_bytes,
                recorder,
                crate::checkpoint_metrics::IoTimingKind::ExistingValidationRead,
            );
            match verification {
                Ok(()) => {
                    destination_parent.verify_file_binding(&destination_leaf, &existing)?;
                    anchored_repo.verify_parent_binding(parent_relative, &destination_parent)?;
                    existing.sync_all()?;
                    anchored_repo.defer_directory_chain(parent_relative, sync_batch)?;
                    if let Some(recorder) = recorder {
                        recorder.existing();
                    }
                    return Ok(false);
                }
                Err(CheckPoError::ObjectHashMismatch(_)) => {
                    destination_parent.verify_file_binding(&destination_leaf, &existing)?;
                    repair_destination = Some(existing);
                }
                Err(error) => return Err(error),
            }
        }
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut repaired_existing = repair_destination.is_some();
    let (temporary_leaf, mut temporary) =
        destination_parent.create_unique_temporary_file("object")?;
    let copy_result = (|| {
        let (copied_size_bytes, actual) = match anchored_source {
            Some((source_root, source_relative)) => {
                let mut input = source_root
                    .open_file(source_relative)
                    .map_err(|_| CheckPoError::WorkingTreeChanged(source.display().to_string()))?;
                let copied = input
                    .copy_and_hash_to_profiled_with_cancellation(
                        &mut temporary,
                        &destination,
                        recorder,
                        cancellation,
                    )
                    .map_err(|error| match &error {
                        CheckPoError::Io { path, .. } if path == source => {
                            CheckPoError::WorkingTreeChanged(source.display().to_string())
                        }
                        _ => error,
                    })?;
                source_root
                    .verify_binding(source_relative, &input)
                    .map_err(|_| CheckPoError::WorkingTreeChanged(source.display().to_string()))?;
                (copied.metadata.len(), copied.object_id)
            }
            None => copy_and_hash_file_to_anchored(source, &mut temporary, &destination, recorder)?,
        };
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::FileFsync,
            || temporary.sync_all(),
        )?;
        if let Some(recorder) = recorder {
            recorder.file_fsync();
        }
        if copied_size_bytes != size_bytes {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "{} size expected {}, got {}",
                source.display(),
                size_bytes,
                copied_size_bytes
            )));
        }
        if &actual != object_id {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "{} expected {}, got {}",
                source.display(),
                object_id,
                actual
            )));
        }
        Ok(())
    })();
    if let Err(error) = copy_result {
        cleanup_private_temporary(&destination_parent, &temporary_leaf, temporary);
        return Err(error);
    }

    let publication = measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::Publish,
        || match repair_destination.as_ref() {
            Some(existing) => destination_parent.replace_from_temporary_batched(
                &temporary_leaf,
                &temporary,
                &destination_leaf,
                existing,
                sync_batch,
            ),
            None => destination_parent
                .rename_no_replace_to(
                    &temporary_leaf,
                    &temporary,
                    &destination_parent,
                    &destination_leaf,
                )
                .and_then(|()| destination_parent.defer_sync(sync_batch)),
        },
    );

    if let Err(error) = publication {
        let already_exists = matches!(
            &error,
            CheckPoError::Io { source, .. }
                if source.kind() == std::io::ErrorKind::AlreadyExists
        );
        if !already_exists || repair_destination.is_some() {
            cleanup_private_temporary(&destination_parent, &temporary_leaf, temporary);
            return Err(error);
        }

        let mut winner = match destination_parent.open_file(&destination_leaf) {
            Ok(winner) => winner,
            Err(winner_error) => {
                cleanup_private_temporary(&destination_parent, &temporary_leaf, temporary);
                return Err(winner_error);
            }
        };
        match verify_anchored_object(
            &mut winner,
            &destination,
            object_id,
            size_bytes,
            recorder,
            crate::checkpoint_metrics::IoTimingKind::ExistingValidationRead,
        ) {
            Ok(()) => {
                destination_parent.verify_file_binding(&destination_leaf, &winner)?;
                winner.sync_all()?;
                anchored_repo.defer_directory_chain(parent_relative, sync_batch)?;
                cleanup_private_temporary(&destination_parent, &temporary_leaf, temporary);
                anchored_repo.verify_parent_binding(parent_relative, &destination_parent)?;
                if let Some(recorder) = recorder {
                    recorder.existing();
                }
                return Ok(false);
            }
            Err(CheckPoError::ObjectHashMismatch(_)) => {
                destination_parent.verify_file_binding(&destination_leaf, &winner)?;
                repaired_existing = true;
                measure_io(
                    recorder,
                    crate::checkpoint_metrics::IoTimingKind::Publish,
                    || {
                        destination_parent.replace_from_temporary_batched(
                            &temporary_leaf,
                            &temporary,
                            &destination_leaf,
                            &winner,
                            sync_batch,
                        )
                    },
                )?;
            }
            Err(winner_error) => {
                cleanup_private_temporary(&destination_parent, &temporary_leaf, temporary);
                return Err(winner_error);
            }
        }
    }

    let published = destination_parent.open_file(&destination_leaf)?;
    destination_parent.verify_file_binding(&destination_leaf, &temporary)?;
    if published.metadata()?.len() != size_bytes {
        return Err(CheckPoError::WorkingTreeChanged(
            destination.display().to_string(),
        ));
    }
    destination_parent.verify_file_binding(&destination_leaf, &published)?;
    anchored_repo.verify_parent_binding(parent_relative, &destination_parent)?;
    if repaired_existing {
        if let Some(recorder) = recorder {
            recorder.repaired();
        }
    }
    if let Some(recorder) = recorder {
        recorder.written(size_bytes);
    }
    Ok(true)
}

fn copy_and_hash_file_to_anchored(
    source: &Path,
    output: &mut AnchoredFile,
    output_path: &Path,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<(u64, ObjectId)> {
    let mut input = File::open(source).map_err(|error| io_error(source, error))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied_size_bytes = 0_u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::SourceRead,
            || input.read(&mut buffer),
        )
        .map_err(|error| io_error(source, error))?;
        if read == 0 {
            break;
        }
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Hash,
            || hasher.update(&buffer[..read]),
        );
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Write,
            || output.write_all(&buffer[..read]),
        )
        .map_err(|error| io_error(output_path, error))?;
        copied_size_bytes = accumulate_streamed_bytes(copied_size_bytes, read)?;
    }
    Ok((
        copied_size_bytes,
        ObjectId::parse(hasher.finalize().to_hex().as_ref())?,
    ))
}

fn accumulate_streamed_bytes(total: u64, read: usize) -> Result<u64> {
    total.checked_add(read as u64).ok_or_else(|| {
        CheckPoError::Unexpected("streamed object size exceeds u64::MAX".to_string())
    })
}

fn verify_anchored_object(
    file: &mut AnchoredFile,
    path: &Path,
    expected: &ObjectId,
    size_bytes: u64,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    timing: crate::checkpoint_metrics::IoTimingKind,
) -> Result<()> {
    let hashed = measure_io(recorder, timing, || file.hash())?;
    if hashed.metadata.len() != size_bytes {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            path.display(),
            size_bytes,
            hashed.metadata.len()
        )));
    }
    if &hashed.object_id != expected {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected,
            hashed.object_id
        )));
    }
    Ok(())
}

fn cleanup_private_temporary(
    parent: &AnchoredParent,
    temporary_leaf: &std::ffi::OsStr,
    temporary: AnchoredFile,
) {
    let _ = parent.unlink_file_if_bound(temporary_leaf, temporary);
    let _ = parent.sync_all();
}

fn measure_io<T>(
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    kind: crate::checkpoint_metrics::IoTimingKind,
    operation: impl FnOnce() -> T,
) -> T {
    match recorder {
        Some(recorder) => recorder.measure(kind, operation),
        None => operation(),
    }
}

#[cfg(test)]
pub(crate) fn copy_object_to_file(
    repo_root: &Path,
    object_id: &ObjectId,
    destination: &Path,
    size_bytes: u64,
) -> Result<()> {
    if let Err(error) = copy_object_to_file_verified(repo_root, object_id, destination, size_bytes)
    {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
fn copy_object_to_file_verified(
    repo_root: &Path,
    object_id: &ObjectId,
    destination: &Path,
    size_bytes: u64,
) -> Result<()> {
    let source = object_path_with_safe_parent(repo_root, object_id, false)?;
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CheckPoError::ObjectMissing(object_id.to_string()));
        }
        Err(error) => return Err(io_error(&source, error)),
    };
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular object file",
            source.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let relative = source.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "object path is outside anchored repository {}: {}",
            repo_root.display(),
            source.display()
        ))
    })?;
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let mut input = anchored_repo.open_file(relative)?;
    let mut output = File::create(destination).map_err(|error| io_error(destination, error))?;
    let copied = input.copy_and_hash_to(&mut output, destination)?;
    output
        .sync_all()
        .map_err(|error| io_error(destination, error))?;
    drop(output);
    if copied.metadata.len() != size_bytes {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            source.display(),
            size_bytes,
            copied.metadata.len()
        )));
    }
    if &copied.object_id != object_id {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            source.display(),
            object_id,
            copied.object_id
        )));
    }
    anchored_repo.verify_binding(relative, &input)?;
    anchored_repo.verify_root_binding()?;
    Ok(())
}

pub(crate) fn verify_stored_object_profiled(
    anchored_repo: &AnchoredRoot,
    repo_root: &Path,
    object_id: &ObjectId,
    size_bytes: u64,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<Option<String>> {
    let path = object_path(repo_root, object_id);
    measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::PostWriteReadback,
        || {
            verify_file_hash_and_size_with_anchor(
                anchored_repo,
                repo_root,
                &path,
                object_id,
                size_bytes,
            )
        },
    )
}

#[cfg(test)]
fn verify_file_hash_and_size_anchored(
    repo_root: &Path,
    path: &Path,
    expected: &ObjectId,
    size_bytes: u64,
) -> Result<()> {
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    verify_file_hash_and_size_with_anchor(&anchored_repo, repo_root, path, expected, size_bytes)
        .map(|_| ())
}

fn verify_file_hash_and_size_with_anchor(
    anchored_repo: &AnchoredRoot,
    repo_root: &Path,
    path: &Path,
    expected: &ObjectId,
    size_bytes: u64,
) -> Result<Option<String>> {
    let relative = path.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "object path is outside anchored repository {}: {}",
            repo_root.display(),
            path.display()
        ))
    })?;
    let mut file = anchored_repo.open_file(relative)?;
    let hashed = file.hash()?;
    if hashed.metadata.len() != size_bytes {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            path.display(),
            size_bytes,
            hashed.metadata.len()
        )));
    }
    if &hashed.object_id != expected {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected,
            hashed.object_id
        )));
    }
    let fingerprint = file.fingerprint()?;
    anchored_repo.verify_binding(relative, &file)?;
    Ok(fingerprint)
}

pub(crate) fn object_path_no_follow(repo_root: &Path, object_id: &ObjectId) -> Result<PathBuf> {
    object_path_with_safe_parent_profiled(repo_root, object_id, false, None)
}

#[cfg(test)]
fn object_path_with_safe_parent(
    repo_root: &Path,
    object_id: &ObjectId,
    create: bool,
) -> Result<PathBuf> {
    object_path_with_safe_parent_profiled(repo_root, object_id, create, None)
}

fn object_path_with_safe_parent_profiled(
    repo_root: &Path,
    object_id: &ObjectId,
    create: bool,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<PathBuf> {
    let loose_root = repo_root.join("objects").join("loose");
    measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::DirectoryPrepare,
        || ensure_regular_directory_no_follow(&loose_root),
    )?;
    let destination = object_path(repo_root, object_id);
    let parent = destination.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!("invalid object path: {}", destination.display()))
    })?;
    if create {
        create_dir_all_no_follow_profiled(&loose_root, parent, recorder)?;
        return Ok(destination);
    }
    let relative = parent.strip_prefix(&loose_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "object shard is outside loose object root: {}",
            parent.display()
        ))
    })?;
    let mut current = loose_root;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(CheckPoError::Corruption(format!(
                "unsafe object shard path: {}",
                parent.display()
            )));
        };
        current.push(component);
        match measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::DirectoryPrepare,
            || fs::symlink_metadata(&current),
        ) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe object shard directory: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(io_error(&current, error)),
        }
    }
    Ok(destination)
}

#[cfg(test)]
pub(crate) fn hash_file(path: &Path) -> Result<ObjectId> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let hash = hasher.finalize();
    ObjectId::parse(hash.to_hex().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_streaming_cas_repairs_corrupt_existing_object() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("objects/loose")).unwrap();
        let source = temp.path().join("source");
        let expected = b"verified streaming payload";
        fs::write(&source, expected).unwrap();
        let object_id = hash_file(&source).unwrap();
        let destination = object_path(&repo, &object_id);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"corrupt").unwrap();

        let repaired =
            put_object_from_file_with_known_hash(&repo, &source, &object_id, expected.len() as u64)
                .unwrap();
        let reused =
            put_object_from_file_with_known_hash(&repo, &source, &object_id, expected.len() as u64)
                .unwrap();

        assert!(repaired);
        assert!(!reused);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".checkpo-object-")));
    }

    #[test]
    fn held_streaming_cas_validates_existing_before_opening_source() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("objects/loose")).unwrap();
        let expected = b"already stored";
        let object_id = crate::hash_bytes(expected);
        let destination = object_path(&repo, &object_id);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, expected).unwrap();

        let created = put_object_from_file_with_known_hash(
            &repo,
            &temp.path().join("source-does-not-exist"),
            &object_id,
            expected.len() as u64,
        )
        .unwrap();

        assert!(!created);
        assert_eq!(fs::read(destination).unwrap(), expected);
    }

    #[test]
    fn held_streaming_cas_defers_directory_barrier_to_held_batch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let source = temp.path().join("source");
        let expected = b"batched payload";
        fs::write(&source, expected).unwrap();
        let object_id = hash_file(&source).unwrap();
        let mut sync_batch = AnchoredParentSyncBatch::new();

        let created = put_object_from_file_with_known_hash_impl(
            &repo,
            &source,
            None,
            &object_id,
            expected.len() as u64,
            None,
            &mut sync_batch,
            None,
            || {},
        )
        .unwrap();

        assert!(created);
        assert!(sync_batch.pending_count() > 0);
        assert_eq!(fs::read(object_path(&repo, &object_id)).unwrap(), expected);
        sync_batch.flush().unwrap();
        assert_eq!(sync_batch.pending_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn held_streaming_cas_cannot_follow_swapped_object_shard() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("objects/loose")).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let source = temp.path().join("source");
        let expected = b"confined payload";
        fs::write(&source, expected).unwrap();
        let object_id = hash_file(&source).unwrap();
        let destination = object_path(&repo, &object_id);
        let shard = destination.parent().unwrap().to_path_buf();
        let held_shard = shard.with_file_name(format!(
            "{}-held",
            shard.file_name().unwrap().to_string_lossy()
        ));
        let mut sync_batch = AnchoredParentSyncBatch::new();

        let error = put_object_from_file_with_known_hash_impl(
            &repo,
            &source,
            None,
            &object_id,
            expected.len() as u64,
            None,
            &mut sync_batch,
            None,
            || {
                fs::rename(&shard, &held_shard).unwrap();
                symlink(&outside, &shard).unwrap();
            },
        )
        .unwrap_err();
        sync_batch.flush().unwrap();

        assert!(matches!(
            error,
            CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_)
        ));
        assert_eq!(
            fs::read(held_shard.join(object_id.as_str())).unwrap(),
            expected
        );
        assert!(!outside.join(object_id.as_str()).exists());
    }

    #[test]
    fn streaming_size_counter_supports_more_than_two_gib_without_large_fixture() {
        let two_gib = 2_u64 * 1024 * 1024 * 1024;

        assert_eq!(
            accumulate_streamed_bytes(two_gib - 1, 64 * 1024).unwrap(),
            two_gib + 65_535
        );
        assert!(accumulate_streamed_bytes(u64::MAX, 1).is_err());
    }
}
