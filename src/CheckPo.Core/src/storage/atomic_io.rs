// The held-parent object publisher supersedes the legacy path-based move
// pipeline in production. Keep its cross-filesystem primitives for the
// existing focused tests and transaction test helpers while that code is
// retired separately.
#![cfg_attr(not(test), allow(dead_code))]

use super::*;

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).map_err(|error| io_error(path, error))?;
    serde_json::from_reader(file).map_err(|error| json_error(path, error))
}

#[cfg(test)]
#[allow(dead_code)]
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| json_error(path, error))?;
    write_bytes_atomic(path, &bytes)
}

#[cfg(test)]
#[allow(dead_code)]
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_bytes_atomic_profiled(path, bytes, None)
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn write_bytes_atomic_profiled(
    path: &Path,
    bytes: &[u8],
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temp_path = short_temporary_path(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| io_error(&temp_path, error))?;
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Write,
            || file.write_all(bytes),
        )
        .map_err(|error| io_error(&temp_path, error))?;
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::FileFsync,
            || file.sync_all(),
        )
        .map_err(|error| io_error(&temp_path, error))?;
        if let Some(recorder) = recorder {
            recorder.file_fsync();
        }
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Publish,
            || replace_file(&temp_path, path),
        )?;
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
            || sync_parent_dir(path),
        )?;
        if let Some(recorder) = recorder {
            recorder.directory_fsync();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
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

#[cfg(not(windows))]
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        let file = File::open(parent).map_err(|error| io_error(parent, error))?;
        file.sync_all().map_err(|error| io_error(parent, error))?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        super::windows_durability::sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn sync_parent_chain(path: &Path, stop_at: &Path) -> Result<()> {
    if !path.starts_with(stop_at) {
        return Err(CheckPoError::Unexpected(format!(
            "cannot sync parent chain outside {}: {}",
            stop_at.display(),
            path.display()
        )));
    }
    let mut current = path.parent();
    while let Some(directory) = current {
        let handle = File::open(directory).map_err(|error| io_error(directory, error))?;
        handle
            .sync_all()
            .map_err(|error| io_error(directory, error))?;
        if directory == stop_at {
            return Ok(());
        }
        current = directory.parent();
    }
    Err(CheckPoError::Unexpected(format!(
        "parent chain did not reach {} from {}",
        stop_at.display(),
        path.display()
    )))
}

#[cfg(windows)]
pub(crate) fn sync_parent_chain(path: &Path, stop_at: &Path) -> Result<()> {
    if !path.starts_with(stop_at) {
        return Err(CheckPoError::Unexpected(format!(
            "cannot sync parent chain outside {}: {}",
            stop_at.display(),
            path.display()
        )));
    }
    let mut current = path.parent();
    while let Some(directory) = current {
        super::windows_durability::sync_directory(directory)?;
        if directory == stop_at {
            return Ok(());
        }
        current = directory.parent();
    }
    Err(CheckPoError::Unexpected(format!(
        "parent chain did not reach {} from {}",
        stop_at.display(),
        path.display()
    )))
}

#[cfg(not(windows))]
pub(crate) fn replace_file(temp_path: &Path, destination: &Path) -> Result<()> {
    fs::rename(temp_path, destination).map_err(|error| io_error(destination, error))
}

#[cfg(windows)]
pub(crate) fn replace_file(temp_path: &Path, destination: &Path) -> Result<()> {
    super::windows_durability::rename_replace(temp_path, destination)
}

#[derive(Debug)]
pub(crate) struct MoveFileNoReplaceFailure {
    pub(crate) error: CheckPoError,
    pub(crate) destination_published: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredMoveOutcome {
    Moved,
    CopyRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicCopyFaultPoint {
    CopyInProgress,
    TempMaterialized,
    TempSynced,
    DestinationPublished,
    DestinationParentSynced,
    SourceRemoved,
    SourceParentSynced,
}

type AtomicCopyFaultHook<'a> = Option<&'a dyn Fn(AtomicCopyFaultPoint)>;

fn inject_atomic_copy_fault(hook: AtomicCopyFaultHook<'_>, point: AtomicCopyFaultPoint) {
    if let Some(hook) = hook {
        hook(point);
    }
}

#[cfg(test)]
pub(crate) fn move_file_no_replace(source: &Path, destination: &Path) -> Result<()> {
    move_file_no_replace_with_status_profiled(source, destination, None)
        .map_err(|failure| failure.error)
}

pub(crate) fn move_file_no_replace_with_status_profiled(
    source: &Path,
    destination: &Path,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> std::result::Result<(), MoveFileNoReplaceFailure> {
    move_file_no_replace_with_status_and_temp(
        source,
        destination,
        short_temporary_path(destination),
        None,
        recorder,
    )
}

fn move_file_no_replace_with_status_and_temp(
    source: &Path,
    destination: &Path,
    temporary_path: PathBuf,
    fault_hook: AtomicCopyFaultHook<'_>,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> std::result::Result<(), MoveFileNoReplaceFailure> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| move_failure(io_error(parent, error), false))?;
    }
    #[cfg(windows)]
    {
        match measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Publish,
            || rename_file_no_replace(source, destination),
        ) {
            Ok(()) => {
                inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::DestinationPublished);
                inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::DestinationParentSynced);
                inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::SourceRemoved);
                inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::SourceParentSynced);
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(move_failure(io_error(destination, error), false))
            }
            Err(error) if error.raw_os_error() == Some(17) => materialize_file_no_replace(
                source,
                destination,
                &temporary_path,
                CopySourceDisposition::Remove,
                MaterializeMode::Copy,
                fault_hook,
                recorder,
            ),
            Err(error) => Err(move_failure(io_error(destination, error), false)),
        }
    }
    #[cfg(not(windows))]
    match measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::Publish,
        || fs::hard_link(source, destination),
    ) {
        Ok(()) => finish_published_move(source, destination, fault_hook, recorder),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(move_failure(io_error(destination, error), false))
        }
        Err(error) if hard_link_allows_copy_fallback(&error) => materialize_file_no_replace(
            source,
            destination,
            &temporary_path,
            CopySourceDisposition::Remove,
            MaterializeMode::Copy,
            fault_hook,
            recorder,
        ),
        Err(error) => Err(move_failure(io_error(destination, error), false)),
    }
}

#[cfg(not(windows))]
fn finish_published_move(
    source: &Path,
    destination: &Path,
    fault_hook: AtomicCopyFaultHook<'_>,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> std::result::Result<(), MoveFileNoReplaceFailure> {
    inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::DestinationPublished);
    measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
        || sync_parent_dir(destination),
    )
    .map_err(|error| move_failure(error, true))?;
    if let Some(recorder) = recorder {
        recorder.directory_fsync();
    }
    inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::DestinationParentSynced);
    fs::remove_file(source).map_err(|error| move_failure(io_error(source, error), true))?;
    inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::SourceRemoved);
    measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
        || sync_parent_dir(source),
    )
    .map_err(|error| move_failure(error, true))?;
    if let Some(recorder) = recorder {
        recorder.directory_fsync();
    }
    inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::SourceParentSynced);
    Ok(())
}

fn move_failure(error: CheckPoError, destination_published: bool) -> MoveFileNoReplaceFailure {
    MoveFileNoReplaceFailure {
        error,
        destination_published,
    }
}

#[cfg(not(windows))]
fn hard_link_allows_copy_fallback(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::CrossesDevices | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        if matches!(
            error.raw_os_error(),
            Some(libc::EXDEV) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS)
        ) {
            return true;
        }
        #[cfg(target_os = "linux")]
        if error.raw_os_error() == Some(libc::EPERM) {
            // Linux reports EPERM when the filesystem itself does not support hard links.
            return true;
        }
        false
    }
    #[cfg(not(unix))]
    false
}

/// Moves an already-synced file between two trusted roots without flushing
/// either directory immediately. A cross-filesystem or unsupported exclusive
/// rename is reported as `CopyRequired` without changing either path, allowing
/// the caller to use an adjacent-temp copy fallback.
///
/// Both parent-directory barriers are reserved before the rename. The caller
/// must flush both batches before committing the transaction that makes the
/// destination authoritative.
#[cfg(test)]
pub(crate) fn move_synced_file_no_replace_deferred_dirs(
    source: &Path,
    destination: &Path,
    source_sync_batch: &mut crate::storage::DirectorySyncBatch,
    destination_sync_batch: &mut crate::storage::DirectorySyncBatch,
) -> std::result::Result<DeferredMoveOutcome, MoveFileNoReplaceFailure> {
    source_sync_batch
        .record_parent(source)
        .map_err(|error| move_failure(error, false))?;
    destination_sync_batch
        .record_parent(destination)
        .map_err(|error| move_failure(error, false))?;
    if let Some(parent) = destination.parent() {
        ensure_regular_directory_no_follow(parent).map_err(|error| move_failure(error, false))?;
    }

    match rename_file_no_replace(source, destination) {
        Ok(()) => Ok(DeferredMoveOutcome::Moved),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(move_failure(io_error(destination, error), false))
        }
        Err(error) if rename_requires_copy_fallback(&error) => {
            Ok(DeferredMoveOutcome::CopyRequired)
        }
        Err(error) => Err(move_failure(io_error(source, error), false)),
    }
}

#[cfg(test)]
fn rename_requires_copy_fallback(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::CrossesDevices | std::io::ErrorKind::Unsupported
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EXDEV) {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(17) {
        return true;
    }
    exclusive_rename_is_unavailable(error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopySourceDisposition {
    #[cfg(test)]
    Keep,
    Remove,
}

#[cfg(all(test, windows))]
pub(crate) fn copy_file_no_replace(
    source: &Path,
    destination: &Path,
    source_disposition: CopySourceDisposition,
) -> Result<()> {
    materialize_file_no_replace(
        source,
        destination,
        &short_temporary_path(destination),
        source_disposition,
        MaterializeMode::Copy,
        None,
        None,
    )
    .map_err(|failure| failure.error)
}

#[cfg(test)]
pub(crate) fn reflink_or_copy_file_no_replace(source: &Path, destination: &Path) -> Result<()> {
    materialize_file_no_replace(
        source,
        destination,
        &short_temporary_path(destination),
        CopySourceDisposition::Keep,
        MaterializeMode::ReflinkOrCopy,
        None,
        None,
    )
    .map_err(|failure| failure.error)
}

#[derive(Debug, Clone, Copy)]
enum MaterializeMode {
    Copy,
    #[cfg(test)]
    ReflinkOrCopy,
}

fn materialize_source_to_new_file(
    source: &Path,
    destination: &Path,
    mode: MaterializeMode,
    fault_hook: AtomicCopyFaultHook<'_>,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> std::io::Result<()> {
    match mode {
        MaterializeMode::Copy => copy_to_new_file(source, destination, fault_hook, recorder),
        #[cfg(test)]
        MaterializeMode::ReflinkOrCopy => match reflink_copy::reflink(source, destination) {
            Ok(()) => Ok(()),
            Err(_) => {
                match fs::symlink_metadata(destination) {
                    Ok(metadata)
                        if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) =>
                    {
                        fs::remove_file(destination)?;
                    }
                    Ok(_) => {
                        return Err(std::io::Error::other(
                            "reflink left a non-regular destination",
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }

                copy_to_new_file(source, destination, fault_hook, recorder)
            }
        },
    }
}

fn copy_to_new_file(
    source: &Path,
    destination: &Path,
    fault_hook: AtomicCopyFaultHook<'_>,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> std::io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut reported_progress = false;
    loop {
        let read = measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::SourceRead,
            || input.read(&mut buffer),
        )?;
        if read == 0 {
            break;
        }
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Write,
            || output.write_all(&buffer[..read]),
        )?;
        if !reported_progress {
            inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::CopyInProgress);
            reported_progress = true;
        }
    }
    Ok(())
}

fn materialize_file_no_replace(
    source: &Path,
    destination: &Path,
    temp_path: &Path,
    source_disposition: CopySourceDisposition,
    mode: MaterializeMode,
    fault_hook: AtomicCopyFaultHook<'_>,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> std::result::Result<(), MoveFileNoReplaceFailure> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| move_failure(io_error(parent, error), false))?;
    }
    if temp_path.parent() != destination.parent() || temp_path == destination {
        return Err(move_failure(
            CheckPoError::Unexpected(format!(
                "atomic materialization temp must be adjacent to destination: {}",
                temp_path.display()
            )),
            false,
        ));
    }
    let mut destination_published = false;
    let result = (|| -> Result<()> {
        if measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::ExistenceCheck,
            || destination.exists(),
        ) {
            return Err(io_error(
                destination,
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "destination already exists",
                ),
            ));
        }
        materialize_source_to_new_file(source, temp_path, mode, fault_hook, recorder)
            .map_err(|error| io_error(temp_path, error))?;
        inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::TempMaterialized);
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp_path)
            .map_err(|error| io_error(temp_path, error))?;
        let permissions = fs::metadata(source)
            .map_err(|error| io_error(source, error))?
            .permissions();
        fs::set_permissions(temp_path, permissions).map_err(|error| io_error(temp_path, error))?;
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::FileFsync,
            || output.sync_all(),
        )
        .map_err(|error| io_error(temp_path, error))?;
        if let Some(recorder) = recorder {
            recorder.file_fsync();
        }
        drop(output);
        inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::TempSynced);
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::Publish,
            || publish_temp_file_no_replace(temp_path, destination, &mut destination_published),
        )?;
        inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::DestinationPublished);
        measure_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
            || sync_parent_dir(destination),
        )?;
        if let Some(recorder) = recorder {
            recorder.directory_fsync();
        }
        inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::DestinationParentSynced);
        if source_disposition == CopySourceDisposition::Remove {
            fs::remove_file(source).map_err(|error| io_error(source, error))?;
            inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::SourceRemoved);
            measure_io(
                recorder,
                crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
                || sync_parent_dir(source),
            )?;
            if let Some(recorder) = recorder {
                recorder.directory_fsync();
            }
            inject_atomic_copy_fault(fault_hook, AtomicCopyFaultPoint::SourceParentSynced);
        }
        Ok(())
    })();
    if result.is_err() && !destination_published {
        let _ = fs::remove_file(temp_path);
    }
    result.map_err(|error| move_failure(error, destination_published))
}

fn publish_temp_file_no_replace(
    temp_path: &Path,
    destination: &Path,
    destination_published: &mut bool,
) -> Result<()> {
    match rename_file_no_replace(temp_path, destination) {
        Ok(()) => {
            *destination_published = true;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(io_error(destination, error))
        }
        Err(error) if exclusive_rename_is_unavailable(&error) => {
            match fs::hard_link(temp_path, destination) {
                Ok(()) => {
                    *destination_published = true;
                    fs::remove_file(temp_path).map_err(|error| io_error(temp_path, error))
                }
                Err(link_error) if link_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(io_error(destination, link_error))
                }
                Err(link_error) => Err(CheckPoError::Unexpected(format!(
                    "destination filesystem cannot atomically publish a new file at {}: exclusive rename is unavailable ({error}); hard link failed ({link_error})",
                    destination.display()
                ))),
            }
        }
        Err(error) => Err(io_error(destination, error)),
    }
}

#[cfg(target_os = "macos")]
fn rename_file_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in source"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in destination"))?;
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_file_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in source"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in destination"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_file_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    super::windows_durability::rename_no_replace(source, destination)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_file_no_replace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive rename is unavailable on this platform",
    ))
}

fn exclusive_rename_is_unavailable(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        // Windows durable publication must use the handle-bound rename and
        // FileId readback path. Falling back to path-based hard-link/remove
        // would silently weaken the crash and TOCTOU guarantees.
        let _ = error;
        return false;
    }
    #[cfg(not(windows))]
    if error.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        return matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
        );
    }
    #[allow(unreachable_code)]
    false
}

#[cfg(test)]
pub(crate) fn remove_file_durable(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        super::windows_durability::remove_file(path)
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(path).map_err(|error| io_error(path, error))?;
        sync_parent_dir(path)
    }
}

/// Atomically moves a file to an adjacent, transaction-owned tombstone name.
/// The tombstone must not exist. Deleting the tombstone is deliberately a
/// separate step: after this function succeeds, recovery can distinguish an
/// unapplied source from an already-detached source without relying on a
/// non-atomic remove operation.
#[cfg(test)]
pub(crate) fn move_file_to_tombstone(source: &Path, tombstone: &Path) -> Result<()> {
    rename_file_no_replace(source, tombstone).map_err(|error| {
        let path = if error.kind() == std::io::ErrorKind::AlreadyExists {
            tombstone
        } else {
            source
        };
        io_error(path, error)
    })?;
    sync_parent_dir(tombstone)?;
    if source.parent() != tombstone.parent() {
        sync_parent_dir(source)?;
    }
    Ok(())
}

/// Publishes an already-synced temporary file without synchronizing its source
/// and destination directories immediately. The caller must flush `sync_batch`
/// before publishing any root/reference that makes the destination reachable.
#[allow(dead_code)]
pub(crate) fn move_file_no_replace_deferred_dirs_profiled(
    source: &Path,
    destination: &Path,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: &mut crate::storage::DirectorySyncBatch,
) -> std::result::Result<(), MoveFileNoReplaceFailure> {
    // Validate both paths and reserve their directory barriers before mutating
    // the filesystem, so a successful publication cannot be followed by a
    // fallible batch-registration step.
    sync_batch
        .record_parent(destination)
        .map_err(|error| move_failure(error, false))?;
    sync_batch
        .record_parent(source)
        .map_err(|error| move_failure(error, false))?;
    if let Some(parent) = destination.parent() {
        ensure_regular_directory_no_follow(parent).map_err(|error| move_failure(error, false))?;
    }

    match measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::Publish,
        || rename_file_no_replace(source, destination),
    ) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(move_failure(io_error(destination, error), false))
        }
        Err(error) if exclusive_rename_is_unavailable(&error) => {
            // Uncommon filesystems without an exclusive rename primitive retain
            // the established immediate-durability fallback.
            move_file_no_replace_with_status_profiled(source, destination, recorder)
        }
        Err(error) => Err(move_failure(io_error(destination, error), false)),
    }
}

#[cfg(not(windows))]
pub(crate) fn sync_directory(directory: &Path) -> Result<()> {
    let file = File::open(directory).map_err(|error| io_error(directory, error))?;
    file.sync_all().map_err(|error| io_error(directory, error))
}

#[cfg(windows)]
pub(crate) fn sync_directory(directory: &Path) -> Result<()> {
    super::windows_durability::sync_directory(directory)
}

fn short_temporary_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(".checkpo-{}.tmp", Uuid::new_v4().simple()))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn atomic_materialization_uses_short_temp_name_for_long_destination_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let long_name = format!("{}.asset", "a".repeat(220));
        let atomic_destination = temp.path().join(&long_name);

        write_bytes_atomic(&atomic_destination, b"atomic").unwrap();
        assert_eq!(fs::read(&atomic_destination).unwrap(), b"atomic");

        let source = temp.path().join("source");
        let copied_destination = temp.path().join(format!("copy-{long_name}"));
        fs::write(&source, "copied").unwrap();
        copy_file_no_replace(&source, &copied_destination, CopySourceDisposition::Keep).unwrap();
        assert_eq!(fs::read(&copied_destination).unwrap(), b"copied");
    }
}

#[cfg(test)]
mod deferred_move_tests {
    use super::*;

    #[test]
    fn synced_file_move_defers_both_directory_barriers() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("journal");
        let destination_root = temp.path().join("project");
        fs::create_dir_all(source_root.join("staged")).unwrap();
        fs::create_dir_all(destination_root.join("Assets")).unwrap();
        let source = source_root.join("staged/file.asset");
        let destination = destination_root.join("Assets/file.asset");
        let mut source_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&source)
            .unwrap();
        source_file.write_all(b"payload").unwrap();
        source_file.sync_all().unwrap();
        drop(source_file);
        let mut source_batch = crate::storage::DirectorySyncBatch::new(&source_root).unwrap();
        let mut destination_batch =
            crate::storage::DirectorySyncBatch::new(&destination_root).unwrap();

        let outcome = move_synced_file_no_replace_deferred_dirs(
            &source,
            &destination,
            &mut source_batch,
            &mut destination_batch,
        )
        .unwrap();

        assert_eq!(outcome, DeferredMoveOutcome::Moved);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "payload");
        assert_eq!(source_batch.pending_count(), 1);
        assert_eq!(destination_batch.pending_count(), 1);
        source_batch.flush(None).unwrap();
        destination_batch.flush(None).unwrap();
    }

    #[test]
    fn synced_file_move_does_not_replace_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("journal");
        let destination_root = temp.path().join("project");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_root.join("file.asset");
        let destination = destination_root.join("file.asset");
        fs::write(&source, "after").unwrap();
        fs::write(&destination, "concurrent").unwrap();
        let mut source_batch = crate::storage::DirectorySyncBatch::new(&source_root).unwrap();
        let mut destination_batch =
            crate::storage::DirectorySyncBatch::new(&destination_root).unwrap();

        let failure = move_synced_file_no_replace_deferred_dirs(
            &source,
            &destination,
            &mut source_batch,
            &mut destination_batch,
        )
        .unwrap_err();

        assert!(!failure.destination_published);
        assert!(matches!(
            failure.error,
            CheckPoError::Io { ref source, .. }
                if source.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(fs::read_to_string(&source).unwrap(), "after");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "concurrent");
    }

    #[cfg(unix)]
    #[test]
    fn cross_device_rename_error_requests_copy_fallback() {
        assert!(rename_requires_copy_fallback(
            &std::io::Error::from_raw_os_error(libc::EXDEV)
        ));
    }
}

#[cfg(test)]
mod fault_tests {
    use super::*;
    use std::process::Command;

    const CHILD_ENV: &str = "CHECKPO_ATOMIC_COPY_FAULT_CHILD";
    const SOURCE_ENV: &str = "CHECKPO_ATOMIC_COPY_FAULT_SOURCE";
    const DESTINATION_ENV: &str = "CHECKPO_ATOMIC_COPY_FAULT_DESTINATION";
    const TEMP_ENV: &str = "CHECKPO_ATOMIC_COPY_FAULT_TEMP";
    const MODE_ENV: &str = "CHECKPO_ATOMIC_COPY_FAULT_MODE";
    const CHILD_EXIT_CODE: i32 = 86;
    const CHILD_TEST_NAME: &str = "storage::atomic_io::fault_tests::atomic_copy_fault_child";

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_hard_link_unsupported_eperm_uses_atomic_copy_fallback() {
        let error = std::io::Error::from_raw_os_error(libc::EPERM);
        assert!(hard_link_allows_copy_fallback(&error));
    }

    #[test]
    fn atomic_copy_fault_child() {
        let Ok(point_name) = std::env::var(CHILD_ENV) else {
            return;
        };
        let point = parse_fault_point(&point_name);
        let source = PathBuf::from(std::env::var_os(SOURCE_ENV).unwrap());
        let destination = PathBuf::from(std::env::var_os(DESTINATION_ENV).unwrap());
        let temporary = PathBuf::from(std::env::var_os(TEMP_ENV).unwrap());
        let hook = |current| {
            if current == point {
                std::process::exit(CHILD_EXIT_CODE);
            }
        };
        let result = match std::env::var(MODE_ENV).as_deref() {
            Ok("move") => move_file_no_replace_with_status_and_temp(
                &source,
                &destination,
                temporary,
                Some(&hook),
                None,
            ),
            Ok("materialize") => materialize_file_no_replace(
                &source,
                &destination,
                &temporary,
                CopySourceDisposition::Remove,
                MaterializeMode::Copy,
                Some(&hook),
                None,
            ),
            Ok(mode) => panic!("unknown fault mode: {mode}"),
            Err(_) => panic!("missing fault mode"),
        };
        panic!("fault point {point_name} was not reached: {result:?}");
    }

    #[test]
    fn atomic_copy_fault_matrix_never_exposes_a_partial_destination() {
        // Child-process exit validates visibility and ordering. It is not a power-loss durability
        // test; filesystem/VM fault injection is still required to validate persistence barriers.
        let temp = tempfile::tempdir().unwrap();
        let points = [
            AtomicCopyFaultPoint::CopyInProgress,
            AtomicCopyFaultPoint::TempMaterialized,
            AtomicCopyFaultPoint::TempSynced,
            AtomicCopyFaultPoint::DestinationPublished,
            AtomicCopyFaultPoint::DestinationParentSynced,
            AtomicCopyFaultPoint::SourceRemoved,
            AtomicCopyFaultPoint::SourceParentSynced,
        ];
        let expected = vec![0x5a_u8; 3 * 1024 * 1024];

        for point in points {
            let case = temp.path().join(fault_point_name(point));
            fs::create_dir_all(&case).unwrap();
            let source = case.join("source");
            let destination = case.join("destination");
            let temporary = case.join(".checkpo-0123456789abcdef0123456789abcdef.tmp");
            fs::write(&source, &expected).unwrap();

            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(CHILD_TEST_NAME)
                .arg("--nocapture")
                .env(CHILD_ENV, fault_point_name(point))
                .env(SOURCE_ENV, &source)
                .env(DESTINATION_ENV, &destination)
                .env(TEMP_ENV, &temporary)
                .env(MODE_ENV, "materialize")
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(CHILD_EXIT_CODE), "point {point:?}");

            if destination.exists() {
                assert_eq!(fs::read(&destination).unwrap(), expected, "point {point:?}");
                assert!(!temporary.exists(), "point {point:?}");
            } else {
                assert!(source.exists(), "point {point:?}");
                assert!(temporary.exists(), "point {point:?}");
                let temporary_len = fs::metadata(&temporary).unwrap().len();
                assert!(temporary_len <= expected.len() as u64, "point {point:?}");
                if point == AtomicCopyFaultPoint::CopyInProgress {
                    assert!(temporary_len < expected.len() as u64, "point {point:?}");
                }
            }

            match point {
                AtomicCopyFaultPoint::CopyInProgress
                | AtomicCopyFaultPoint::TempMaterialized
                | AtomicCopyFaultPoint::TempSynced => {
                    assert!(source.exists(), "point {point:?}");
                    assert!(!destination.exists(), "point {point:?}");
                }
                AtomicCopyFaultPoint::DestinationPublished
                | AtomicCopyFaultPoint::DestinationParentSynced => {
                    assert!(source.exists(), "point {point:?}");
                    assert!(destination.exists(), "point {point:?}");
                }
                AtomicCopyFaultPoint::SourceRemoved | AtomicCopyFaultPoint::SourceParentSynced => {
                    assert!(!source.exists(), "point {point:?}");
                    assert!(destination.exists(), "point {point:?}");
                }
            }
        }
    }

    #[test]
    fn atomic_fast_move_fault_matrix_preserves_a_complete_destination() {
        let temp = tempfile::tempdir().unwrap();
        let points = [
            AtomicCopyFaultPoint::DestinationPublished,
            AtomicCopyFaultPoint::DestinationParentSynced,
            AtomicCopyFaultPoint::SourceRemoved,
            AtomicCopyFaultPoint::SourceParentSynced,
        ];
        let expected = vec![0xa5_u8; 128 * 1024];

        for point in points {
            let case = temp
                .path()
                .join(format!("fast-{}", fault_point_name(point)));
            fs::create_dir_all(&case).unwrap();
            let source = case.join("source");
            let destination = case.join("destination");
            let temporary = case.join(".checkpo-0123456789abcdef0123456789abcdef.tmp");
            fs::write(&source, &expected).unwrap();

            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(CHILD_TEST_NAME)
                .arg("--nocapture")
                .env(CHILD_ENV, fault_point_name(point))
                .env(SOURCE_ENV, &source)
                .env(DESTINATION_ENV, &destination)
                .env(TEMP_ENV, &temporary)
                .env(MODE_ENV, "move")
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(CHILD_EXIT_CODE), "point {point:?}");
            assert_eq!(fs::read(&destination).unwrap(), expected, "point {point:?}");
            assert!(!temporary.exists(), "point {point:?}");

            #[cfg(not(windows))]
            match point {
                AtomicCopyFaultPoint::DestinationPublished
                | AtomicCopyFaultPoint::DestinationParentSynced => {
                    assert!(source.exists(), "point {point:?}");
                }
                AtomicCopyFaultPoint::SourceRemoved | AtomicCopyFaultPoint::SourceParentSynced => {
                    assert!(!source.exists(), "point {point:?}");
                }
                _ => unreachable!(),
            }
            #[cfg(windows)]
            assert!(!source.exists(), "point {point:?}");
        }
    }

    fn fault_point_name(point: AtomicCopyFaultPoint) -> &'static str {
        match point {
            AtomicCopyFaultPoint::CopyInProgress => "copy-in-progress",
            AtomicCopyFaultPoint::TempMaterialized => "temp-materialized",
            AtomicCopyFaultPoint::TempSynced => "temp-synced",
            AtomicCopyFaultPoint::DestinationPublished => "destination-published",
            AtomicCopyFaultPoint::DestinationParentSynced => "destination-parent-synced",
            AtomicCopyFaultPoint::SourceRemoved => "source-removed",
            AtomicCopyFaultPoint::SourceParentSynced => "source-parent-synced",
        }
    }

    fn parse_fault_point(value: &str) -> AtomicCopyFaultPoint {
        [
            AtomicCopyFaultPoint::CopyInProgress,
            AtomicCopyFaultPoint::TempMaterialized,
            AtomicCopyFaultPoint::TempSynced,
            AtomicCopyFaultPoint::DestinationPublished,
            AtomicCopyFaultPoint::DestinationParentSynced,
            AtomicCopyFaultPoint::SourceRemoved,
            AtomicCopyFaultPoint::SourceParentSynced,
        ]
        .into_iter()
        .find(|point| fault_point_name(*point) == value)
        .unwrap_or_else(|| panic!("unknown fault point: {value}"))
    }
}
