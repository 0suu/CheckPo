use crate::{io_error, json_error, CheckPoError, ObjectId, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// A directory that remains bound to the inode opened at construction time.
///
/// On Unix every descendant is opened relative to this handle with
/// `O_NOFOLLOW`. Renaming or replacing the path used to construct this value
/// therefore cannot redirect later reads outside the directory that was
/// originally approved.
pub(crate) struct AnchoredRoot {
    display_path: PathBuf,
    identity: FileIdentity,
    #[cfg(any(unix, windows))]
    directory: File,
}

pub(crate) struct AnchoredFile {
    display_path: PathBuf,
    file: File,
    identity: FileIdentity,
}

pub(crate) struct AnchoredParent {
    display_path: PathBuf,
    directory: File,
    identity: FileIdentity,
}

/// Held parent-directory descriptors whose entry updates form one durability
/// barrier. The bounded pending set avoids exhausting the process descriptor
/// limit on projects with many distinct directories; an early partial flush is
/// safe because it only makes already-created entries durable sooner.
pub(crate) struct AnchoredParentSyncBatch {
    parents: Vec<AnchoredParent>,
    max_pending: usize,
    completed_count: usize,
    unreported_sync_duration: std::time::Duration,
    unreported_sync_count: usize,
}

/// The subset of file metadata needed by the scanner's cache fast path.
///
/// On Unix this is produced by one `fstatat(AT_SYMLINK_NOFOLLOW)` against a
/// held parent-directory descriptor.  In particular, collecting it does not
/// open every unchanged source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnchoredFileMetadata {
    pub(crate) size_bytes: u64,
    pub(crate) modified: std::time::SystemTime,
    pub(crate) fingerprint: Option<String>,
    pub(crate) is_regular: bool,
    pub(crate) is_link: bool,
}

pub(crate) struct AnchoredHash {
    pub(crate) object_id: ObjectId,
    pub(crate) metadata: fs::Metadata,
    /// Opaque proof of the exact handle version observed after hashing.
    /// Callers must not recapture this after `hash`: doing so would admit a
    /// write that raced between the hash and the second metadata read.
    pub(crate) version: AnchoredFileVersion,
}

#[derive(Clone, Copy)]
pub(crate) struct AnchoredFileVersion {
    full: FileVersion,
    #[cfg(unix)]
    stable_content: StableFileVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
    #[cfg(not(any(unix, windows)))]
    length: u64,
    #[cfg(not(any(unix, windows)))]
    modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    identity: FileIdentity,
    length: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
    #[cfg(windows)]
    changed: i64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplaceProtocolPhase {
    RecoveryRecordDurable,
    DestinationDetached,
    ReplacementPublished,
}

#[cfg(windows)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowsReplaceRecoveryRecord {
    version: u32,
    destination_leaf_utf16: Vec<u16>,
    temporary_leaf_utf16: Vec<u16>,
    tombstone_leaf_utf16: Vec<u16>,
    old_volume_serial_number: u64,
    old_file_id: [u8; 16],
    new_volume_serial_number: u64,
    new_file_id: [u8; 16],
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StableFileVersion {
    identity: FileIdentity,
    length: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl AnchoredRoot {
    const MAX_ANCHORED_JSON_BYTES: u64 = 512 * 1024 * 1024;
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(CheckPoError::Unexpected(format!(
                "anchored root must be absolute: {}",
                path.display()
            )));
        }

        #[cfg(unix)]
        {
            let directory = open_unix_path(
                libc::AT_FDCWD,
                path,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )?;
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(path, error))?;
            if !metadata.is_dir() {
                return Err(CheckPoError::Corruption(format!(
                    "anchored root is not a directory: {}",
                    path.display()
                )));
            }
            Ok(Self {
                display_path: path.to_path_buf(),
                identity: FileIdentity::from_metadata(&metadata)?,
                directory,
            })
        }

        #[cfg(windows)]
        {
            let directory = open_windows_directory_no_follow(path)?;
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(path, error))?;
            if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(CheckPoError::Corruption(format!(
                    "anchored root is not a regular directory: {}",
                    path.display()
                )));
            }
            Ok(Self {
                display_path: path.to_path_buf(),
                identity: FileIdentity::from_file(path, &directory)?,
                directory,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
            if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(CheckPoError::Corruption(format!(
                    "anchored root is not a regular directory: {}",
                    path.display()
                )));
            }
            Ok(Self {
                display_path: path.to_path_buf(),
                identity: FileIdentity::from_metadata(&metadata)?,
            })
        }
    }

    pub(crate) fn verify_root_binding(&self) -> Result<()> {
        #[cfg(unix)]
        let current = {
            let directory = open_unix_path(
                libc::AT_FDCWD,
                &self.display_path,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )?;
            directory
                .metadata()
                .map_err(|error| io_error(&self.display_path, error))?
        };

        #[cfg(windows)]
        let current = {
            if FileIdentity::from_file(&self.display_path, &self.directory)? != self.identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.display().to_string(),
                ));
            }
            let directory = open_windows_directory_no_follow(&self.display_path)?;
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(&self.display_path, error))?;
            if FileIdentity::from_file(&self.display_path, &directory)? != self.identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.display().to_string(),
                ));
            }
            metadata
        };

        #[cfg(not(any(unix, windows)))]
        let current = fs::symlink_metadata(&self.display_path)
            .map_err(|error| io_error(&self.display_path, error))?;

        if !current.is_dir() || crate::metadata_is_link_or_reparse(&current) || {
            #[cfg(windows)]
            {
                false
            }
            #[cfg(not(windows))]
            {
                FileIdentity::from_metadata(&current)? != self.identity
            }
        } {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn open_file(&self, relative: &Path) -> Result<AnchoredFile> {
        self.open_file_impl(relative, |_, _| {})
    }

    pub(crate) fn open_file_read_write(&self, relative: &Path) -> Result<AnchoredFile> {
        let (parent, leaf) = self.open_parent_for_mutation(relative, false)?;
        parent.open_file_read_write(&leaf)
    }

    /// Durably publishes `bytes` below this held repository root.
    ///
    /// The temporary file and final rename both use the held parent handle, so
    /// replacing the root or an intermediate pathname cannot redirect the
    /// write. The root/parent pathname bindings are checked after publication;
    /// a swap is reported even though the data remains confined to the
    /// originally approved directory.
    pub(crate) fn write_bytes_atomic(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        self.write_bytes_atomic_impl(relative, bytes, false)
    }

    pub(crate) fn write_bytes_atomic_new(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        self.write_bytes_atomic_impl(relative, bytes, true)
    }

    pub(crate) fn write_json_atomic<T: serde::Serialize>(
        &self,
        relative: &Path,
        value: &T,
    ) -> Result<()> {
        let display_path = self.display_path.join(relative);
        let bytes = serde_json::to_vec(value).map_err(|error| json_error(display_path, error))?;
        self.write_bytes_atomic(relative, &bytes)
    }

    pub(crate) fn write_json_atomic_new<T: serde::Serialize>(
        &self,
        relative: &Path,
        value: &T,
    ) -> Result<()> {
        let display_path = self.display_path.join(relative);
        let bytes = serde_json::to_vec(value).map_err(|error| json_error(display_path, error))?;
        self.write_bytes_atomic_new(relative, &bytes)
    }

    pub(crate) fn write_json_atomic_path<T: serde::Serialize>(
        &self,
        path: &Path,
        value: &T,
    ) -> Result<()> {
        let relative = self.relative_path(path, "JSON destination")?;
        self.write_json_atomic(relative, value)
    }

    pub(crate) fn read_json_path<T: serde::de::DeserializeOwned>(&self, path: &Path) -> Result<T> {
        let relative = self.relative_path(path, "JSON source")?;
        let bytes = self.read_bytes_bounded(relative, Self::MAX_ANCHORED_JSON_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|error| json_error(path, error))
    }

    pub(crate) fn read_bytes_bounded_path(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
        let relative = self.relative_path(path, "read source")?;
        self.read_bytes_bounded(relative, max_bytes)
    }

    /// Publishes immutable content-addressed bytes through held parent handles.
    /// Existing bytes are returned unchanged when they match. A conflicting
    /// entry is repaired only from these already-validated expected bytes,
    /// using the held-parent atomic exchange primitive and final readback.
    pub(crate) fn store_content_addressed_bytes_profiled(
        &self,
        path: &Path,
        bytes: &[u8],
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
        mut sync_batch: Option<&mut AnchoredParentSyncBatch>,
        existing_is_known_durable: bool,
    ) -> Result<()> {
        let relative = self.relative_path(path, "content-addressed destination")?;
        if let Some(recorder) = recorder {
            recorder.checked(bytes.len() as u64);
        }
        let (parent, leaf) = match sync_batch.as_deref_mut() {
            Some(batch) => self.open_parent_batched(relative, true, batch)?,
            None => self.open_parent_for_mutation(relative, true)?,
        };

        let existing = measure_anchored_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::ExistenceCheck,
            || parent.open_file(&leaf),
        );
        let mut repair_destination = None;
        match existing {
            Ok(mut existing) => {
                match validate_content_addressed_bytes(&mut existing, path, bytes, recorder) {
                    Ok(()) => {
                        parent.verify_file_binding(&leaf, &existing)?;
                        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
                        self.verify_parent_binding(parent_relative, &parent)?;
                        if !existing_is_known_durable {
                            measure_anchored_io(
                                recorder,
                                crate::checkpoint_metrics::IoTimingKind::FileFsync,
                                || existing.sync_all(),
                            )?;
                            if let Some(recorder) = recorder {
                                recorder.file_fsync();
                            }
                            self.defer_or_sync_directory_chain(
                                parent_relative,
                                sync_batch.as_deref_mut(),
                                recorder,
                            )?;
                        }
                        if let Some(recorder) = recorder {
                            recorder.existing();
                        }
                        return Ok(());
                    }
                    Err(CheckPoError::Corruption(_)) => {
                        parent.verify_file_binding(&leaf, &existing)?;
                        if let Some(recorder) = recorder {
                            recorder.repaired();
                        }
                        repair_destination = Some(existing);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(CheckPoError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let (temporary_leaf, mut temporary) = parent.create_unique_temporary_file("cas")?;
        let publication = (|| {
            measure_anchored_io(
                recorder,
                crate::checkpoint_metrics::IoTimingKind::Write,
                || temporary.write_all(bytes),
            )
            .map_err(|error| io_error(&temporary.display_path, error))?;
            measure_anchored_io(
                recorder,
                crate::checkpoint_metrics::IoTimingKind::FileFsync,
                || temporary.sync_all(),
            )?;
            if let Some(recorder) = recorder {
                recorder.file_fsync();
            }
            measure_anchored_io(
                recorder,
                crate::checkpoint_metrics::IoTimingKind::Publish,
                || match repair_destination.as_ref() {
                    Some(destination) => parent.replace_from_temporary(
                        &temporary_leaf,
                        &temporary,
                        &leaf,
                        destination,
                    ),
                    None => {
                        parent.rename_no_replace_to(&temporary_leaf, &temporary, &parent, &leaf)
                    }
                },
            )
        })();

        if let Err(error) = publication {
            let _ = parent.unlink_file_if_bound_ref(&temporary_leaf, &temporary);
            let _ = parent.sync_all();
            if matches!(
                &error,
                CheckPoError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::AlreadyExists
            ) {
                let mut winner = parent.open_file(&leaf)?;
                validate_content_addressed_bytes(&mut winner, path, bytes, recorder)?;
                parent.verify_file_binding(&leaf, &winner)?;
                let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
                self.verify_parent_binding(parent_relative, &parent)?;
                measure_anchored_io(
                    recorder,
                    crate::checkpoint_metrics::IoTimingKind::FileFsync,
                    || winner.sync_all(),
                )?;
                if let Some(recorder) = recorder {
                    recorder.file_fsync();
                }
                self.defer_or_sync_directory_chain(
                    parent_relative,
                    sync_batch.as_deref_mut(),
                    recorder,
                )?;
                if let Some(recorder) = recorder {
                    recorder.existing();
                }
                return Ok(());
            }
            return Err(error);
        }

        parent.verify_file_binding(&leaf, &temporary)?;
        let stored = measure_anchored_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::PostWriteReadback,
            || temporary.read_bounded(bytes.len() as u64),
        )?;
        if stored != bytes {
            return Err(CheckPoError::Corruption(format!(
                "content-addressed write verification failed: {}",
                path.display()
            )));
        }
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        self.verify_parent_binding(parent_relative, &parent)?;
        match sync_batch {
            Some(batch) => batch.record(parent)?,
            None => {
                measure_anchored_io(
                    recorder,
                    crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
                    || parent.sync_all(),
                )?;
                if let Some(recorder) = recorder {
                    recorder.directory_fsync();
                }
            }
        }
        if let Some(recorder) = recorder {
            recorder.written(bytes.len() as u64);
        }
        Ok(())
    }

    pub(crate) fn defer_directory_chain(
        &self,
        relative_directory: &Path,
        sync_batch: &mut AnchoredParentSyncBatch,
    ) -> Result<()> {
        let components = validated_relative_components(relative_directory)?;
        sync_batch.record_directory_handle(&self.display_path, &self.directory)?;
        let mut prefix = PathBuf::new();
        for component in components {
            prefix.push(component);
            sync_batch.record(self.open_directory(&prefix, false)?)?;
        }
        Ok(())
    }

    fn defer_or_sync_directory_chain(
        &self,
        relative_directory: &Path,
        sync_batch: Option<&mut AnchoredParentSyncBatch>,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    ) -> Result<()> {
        match sync_batch {
            Some(batch) => self.defer_directory_chain(relative_directory, batch),
            None => {
                let mut batch = AnchoredParentSyncBatch::new();
                self.defer_directory_chain(relative_directory, &mut batch)?;
                batch.flush_with_progress(recorder, |_, _| Ok(()))
            }
        }
    }

    fn read_bytes_bounded(&self, relative: &Path, max_bytes: u64) -> Result<Vec<u8>> {
        let mut file = self.open_file(relative)?;
        let bytes = file.read_bounded(max_bytes)?;
        self.verify_binding(relative, &file)?;
        self.verify_root_binding()?;
        Ok(bytes)
    }

    fn relative_path<'a>(&self, path: &'a Path, description: &str) -> Result<&'a Path> {
        path.strip_prefix(&self.display_path).map_err(|_| {
            CheckPoError::Corruption(format!(
                "anchored {description} is outside held root {}: {}",
                self.display_path.display(),
                path.display()
            ))
        })
    }

    fn write_bytes_atomic_impl(
        &self,
        relative: &Path,
        bytes: &[u8],
        create_new: bool,
    ) -> Result<()> {
        let (parent, leaf) = self.open_parent_for_mutation(relative, true)?;
        parent.write_bytes_atomic(&leaf, bytes, create_new)?;
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        self.verify_parent_binding(parent_relative, &parent)?;
        self.verify_root_binding()
    }

    pub(crate) fn open_parent(
        &self,
        relative: &Path,
        create_missing: bool,
    ) -> Result<(AnchoredParent, std::ffi::OsString)> {
        self.open_parent_impl(relative, create_missing, create_missing, None, None)
    }

    pub(crate) fn open_parent_for_mutation(
        &self,
        relative: &Path,
        create_missing: bool,
    ) -> Result<(AnchoredParent, std::ffi::OsString)> {
        self.open_parent_impl(relative, create_missing, true, None, None)
    }

    pub(crate) fn open_parent_batched(
        &self,
        relative: &Path,
        create_missing: bool,
        sync_batch: &mut AnchoredParentSyncBatch,
    ) -> Result<(AnchoredParent, std::ffi::OsString)> {
        self.open_parent_impl(relative, create_missing, true, Some(sync_batch), None)
    }

    pub(crate) fn open_parent_batched_profiled(
        &self,
        relative: &Path,
        create_missing: bool,
        sync_batch: &mut AnchoredParentSyncBatch,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    ) -> Result<(AnchoredParent, std::ffi::OsString)> {
        self.open_parent_impl(relative, create_missing, true, Some(sync_batch), recorder)
    }

    fn open_parent_impl(
        &self,
        relative: &Path,
        create_missing: bool,
        _writable_parent: bool,
        mut sync_batch: Option<&mut AnchoredParentSyncBatch>,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    ) -> Result<(AnchoredParent, std::ffi::OsString)> {
        let components = validated_relative_components(relative)?;
        let leaf = components
            .last()
            .expect("validated relative path has a component")
            .to_os_string();
        let parent_components = &components[..components.len() - 1];
        let mut display_path = self.display_path.clone();

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let mut directory = self
                .directory
                .try_clone()
                .map_err(|error| io_error(&self.display_path, error))?;
            for component in parent_components {
                display_path.push(component);
                let opened = match open_unix_component(
                    directory.as_raw_fd(),
                    component,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    &display_path,
                ) {
                    Ok(opened) => opened,
                    Err(CheckPoError::Io { source, .. })
                        if create_missing && source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        let created = create_unix_directory_component(
                            directory.as_raw_fd(),
                            component,
                            &display_path,
                        )?;
                        if created {
                            if let Some(recorder) = recorder {
                                recorder.directory_created();
                            }
                        }
                        let parent_display_path = display_path
                            .parent()
                            .expect("created component has an anchored parent");
                        match sync_batch.as_deref_mut() {
                            Some(sync_batch) => sync_batch
                                .record_directory_handle(parent_display_path, &directory)?,
                            None => directory
                                .sync_all()
                                .map_err(|error| io_error(parent_display_path, error))?,
                        }
                        open_unix_component(
                            directory.as_raw_fd(),
                            component,
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                            &display_path,
                        )?
                    }
                    Err(error) => return Err(error),
                };
                let metadata = opened
                    .metadata()
                    .map_err(|error| io_error(&display_path, error))?;
                if !metadata.is_dir() {
                    return Err(CheckPoError::Corruption(format!(
                        "anchored parent component is not a directory: {}",
                        display_path.display()
                    )));
                }
                directory = opened;
            }
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(&display_path, error))?;
            Ok((
                AnchoredParent {
                    display_path,
                    identity: FileIdentity::from_metadata(&metadata)?,
                    directory,
                },
                leaf,
            ))
        }

        #[cfg(windows)]
        {
            let mut directory = if _writable_parent {
                reopen_windows_directory_for_mutation(&self.directory, &self.display_path)?
            } else {
                self.directory
                    .try_clone()
                    .map_err(|error| io_error(&self.display_path, error))?
            };
            for component in parent_components {
                display_path.push(component);
                let opened = match open_windows_relative_directory(
                    &directory,
                    component,
                    false,
                    _writable_parent,
                ) {
                    Ok(opened) => opened,
                    Err(CheckPoError::Io { source, .. })
                        if create_missing && source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        let parent_display_path = display_path
                            .parent()
                            .expect("created component has an anchored parent");
                        let (opened, created) = match open_windows_relative_directory(
                            &directory, component, true, true,
                        ) {
                            Ok(opened) => (opened, true),
                            Err(CheckPoError::Io { source, .. })
                                if source.kind() == std::io::ErrorKind::AlreadyExists =>
                            {
                                (
                                    open_windows_relative_directory(
                                        &directory,
                                        component,
                                        false,
                                        _writable_parent,
                                    )?,
                                    false,
                                )
                            }
                            Err(error) => return Err(error),
                        };
                        if created {
                            if let Some(recorder) = recorder {
                                recorder.directory_created();
                            }
                        }
                        match sync_batch.as_deref_mut() {
                            Some(sync_batch) => sync_batch
                                .record_directory_handle(parent_display_path, &directory)?,
                            None => directory
                                .sync_all()
                                .map_err(|error| io_error(parent_display_path, error))?,
                        }
                        opened
                    }
                    Err(error) => return Err(error),
                };
                let metadata = opened
                    .metadata()
                    .map_err(|error| io_error(&display_path, error))?;
                if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(CheckPoError::Corruption(format!(
                        "unsafe anchored parent component: {}",
                        display_path.display()
                    )));
                }
                directory = opened;
            }
            let identity = FileIdentity::from_file(&display_path, &directory)?;
            Ok((
                AnchoredParent {
                    display_path,
                    directory,
                    identity,
                },
                leaf,
            ))
        }

        #[cfg(not(any(unix, windows)))]
        {
            let mut current = self.display_path.clone();
            for component in parent_components {
                current.push(component);
                match fs::symlink_metadata(&current) {
                    Ok(metadata)
                        if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {}
                    Err(error)
                        if create_missing && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        fs::create_dir(&current).map_err(|error| io_error(&current, error))?;
                        if let Some(recorder) = recorder {
                            recorder.directory_created();
                        }
                        crate::sync_parent_dir(&current)?;
                    }
                    Ok(_) => {
                        return Err(CheckPoError::Corruption(format!(
                            "unsafe anchored parent: {}",
                            current.display()
                        )))
                    }
                    Err(error) => return Err(io_error(&current, error)),
                }
            }
            let directory = open_portable_directory_no_follow(&current)?;
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(&current, error))?;
            let identity = FileIdentity::from_metadata(&metadata)?;
            Ok((
                AnchoredParent {
                    display_path: current,
                    directory,
                    identity,
                },
                leaf,
            ))
        }
    }

    pub(crate) fn open_directory(
        &self,
        relative: &Path,
        create_missing: bool,
    ) -> Result<AnchoredParent> {
        if relative.as_os_str().is_empty() {
            let directory = self
                .directory
                .try_clone()
                .map_err(|error| io_error(&self.display_path, error))?;
            return Ok(AnchoredParent {
                display_path: self.display_path.clone(),
                identity: self.identity,
                directory,
            });
        }
        let synthetic = relative.join(".checkpo-anchor-leaf");
        self.open_parent_impl(&synthetic, create_missing, create_missing, None, None)
            .map(|(parent, _)| parent)
    }

    pub(crate) fn open_directory_for_mutation(
        &self,
        relative: &Path,
        create_missing: bool,
    ) -> Result<AnchoredParent> {
        if relative.as_os_str().is_empty() {
            #[cfg(windows)]
            let directory =
                reopen_windows_directory_for_mutation(&self.directory, &self.display_path)?;
            #[cfg(not(windows))]
            let directory = self
                .directory
                .try_clone()
                .map_err(|error| io_error(&self.display_path, error))?;
            return Ok(AnchoredParent {
                display_path: self.display_path.clone(),
                identity: self.identity,
                directory,
            });
        }
        let synthetic = relative.join(".checkpo-anchor-leaf");
        self.open_parent_impl(&synthetic, create_missing, true, None, None)
            .map(|(parent, _)| parent)
    }

    pub(crate) fn verify_parent_binding(
        &self,
        relative: &Path,
        parent: &AnchoredParent,
    ) -> Result<()> {
        let current = self.open_directory(relative, false).map_err(|_| {
            CheckPoError::WorkingTreeChanged(self.display_path.join(relative).display().to_string())
        })?;
        if !current.same_directory(parent) {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.join(relative).display().to_string(),
            ));
        }
        self.verify_root_binding()
    }

    fn open_file_impl(
        &self,
        relative: &Path,
        mut component_opened: impl FnMut(usize, &Path),
    ) -> Result<AnchoredFile> {
        let components = validated_relative_components(relative)?;
        let display_path = self.display_path.join(relative);

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let mut current_directory: Option<File> = None;
            let mut walked = PathBuf::new();
            for (index, component) in components.iter().enumerate() {
                walked.push(component);
                let parent_fd = current_directory
                    .as_ref()
                    .map_or_else(|| self.directory.as_raw_fd(), AsRawFd::as_raw_fd);
                let last = index + 1 == components.len();
                let flags = if last {
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
                } else {
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
                };
                let opened = open_unix_component(parent_fd, component, flags, &display_path)?;
                component_opened(index, &walked);
                let metadata = opened
                    .metadata()
                    .map_err(|error| io_error(&display_path, error))?;
                if last {
                    if !metadata.is_file() {
                        return Err(CheckPoError::Corruption(format!(
                            "anchored path is not a regular file: {}",
                            display_path.display()
                        )));
                    }
                    let identity = FileIdentity::from_metadata(&metadata)?;
                    return Ok(AnchoredFile {
                        display_path,
                        file: opened,
                        identity,
                    });
                }
                if !metadata.is_dir() {
                    return Err(CheckPoError::Corruption(format!(
                        "anchored path component is not a directory: {}",
                        self.display_path.join(&walked).display()
                    )));
                }
                current_directory = Some(opened);
            }
            unreachable!("validated relative path has at least one component");
        }

        #[cfg(windows)]
        {
            let _ = (&components, &mut component_opened, &display_path);
            let (parent, leaf) = self.open_parent(relative, false)?;
            parent.open_file(&leaf)
        }

        #[cfg(not(any(unix, windows)))]
        {
            let mut current = self.display_path.clone();
            for (index, component) in components.iter().enumerate() {
                current.push(component);
                component_opened(index, &current);
                let metadata =
                    fs::symlink_metadata(&current).map_err(|error| io_error(&current, error))?;
                let last = index + 1 == components.len();
                if crate::metadata_is_link_or_reparse(&metadata)
                    || (last && !metadata.is_file())
                    || (!last && !metadata.is_dir())
                {
                    return Err(CheckPoError::Corruption(format!(
                        "unsafe anchored path: {}",
                        current.display()
                    )));
                }
            }
            let file = open_portable_file_no_follow(&display_path)?;
            let metadata = file
                .metadata()
                .map_err(|error| io_error(&display_path, error))?;
            if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(CheckPoError::Corruption(format!(
                    "anchored path changed while opening: {}",
                    display_path.display()
                )));
            }
            let identity = FileIdentity::from_metadata(&metadata)?;
            Ok(AnchoredFile {
                display_path,
                file,
                identity,
            })
        }
    }

    /// Confirms that `relative` still resolves to the inode held by `file`.
    /// Callers that publish a result derived from a pathname should perform this
    /// check immediately before publishing it.
    pub(crate) fn verify_binding(&self, relative: &Path, file: &AnchoredFile) -> Result<()> {
        let current = self.open_file(relative)?;
        if current.identity != file.identity {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.join(relative).display().to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(all(test, unix))]
    fn open_file_with_component_hook(
        &self,
        relative: &Path,
        hook: impl FnMut(usize, &Path),
    ) -> Result<AnchoredFile> {
        self.open_file_impl(relative, hook)
    }
}

impl AnchoredParentSyncBatch {
    const DEFAULT_MAX_PENDING: usize = 128;

    pub(crate) fn new() -> Self {
        Self {
            parents: Vec::new(),
            max_pending: Self::DEFAULT_MAX_PENDING,
            completed_count: 0,
            unreported_sync_duration: std::time::Duration::ZERO,
            unreported_sync_count: 0,
        }
    }

    pub(crate) fn with_max_pending(max_pending: usize) -> Self {
        Self {
            parents: Vec::new(),
            max_pending: max_pending.max(1),
            completed_count: 0,
            unreported_sync_duration: std::time::Duration::ZERO,
            unreported_sync_count: 0,
        }
    }

    pub(crate) fn record(&mut self, parent: AnchoredParent) -> Result<()> {
        if self
            .parents
            .iter()
            .any(|existing| existing.same_directory(&parent))
        {
            return Ok(());
        }
        if self.parents.len() >= self.max_pending {
            self.flush()?;
        }
        #[cfg(windows)]
        let parent = {
            let mut parent = parent;
            // Directory flushing requires a write-capable handle on Windows.
            // Rebind by path only after proving that the new handle still
            // names the directory represented by the held anchor.
            parent.directory =
                reopen_windows_directory_for_mutation(&parent.directory, &parent.display_path)?;
            parent
        };
        self.parents.push(parent);
        Ok(())
    }

    /// Merges another deferred durability set while preserving identity-based
    /// deduplication and the bounded descriptor policy of `record`.
    pub(crate) fn merge(&mut self, mut other: Self) -> Result<()> {
        self.completed_count = self.completed_count.saturating_add(other.completed_count);
        self.unreported_sync_duration = self
            .unreported_sync_duration
            .saturating_add(other.unreported_sync_duration);
        self.unreported_sync_count = self
            .unreported_sync_count
            .saturating_add(other.unreported_sync_count);
        for parent in other.parents.drain(..) {
            self.record(parent)?;
        }
        Ok(())
    }

    fn record_directory_handle(&mut self, display_path: &Path, directory: &File) -> Result<()> {
        let directory = directory
            .try_clone()
            .map_err(|error| io_error(display_path, error))?;
        #[cfg(windows)]
        let identity = FileIdentity::from_file(display_path, &directory)?;
        #[cfg(not(windows))]
        let identity = {
            let metadata = directory
                .metadata()
                .map_err(|error| io_error(display_path, error))?;
            FileIdentity::from_metadata(&metadata)?
        };
        self.record(AnchoredParent {
            display_path: display_path.to_path_buf(),
            directory,
            identity,
        })
    }

    pub(crate) fn flush(&mut self) -> Result<()> {
        self.flush_with_progress(None, |_, _| Ok(()))
    }

    pub(crate) fn flush_with_progress(
        &mut self,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
        mut progress: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<()> {
        // A child entry must be durable before the directory entry that makes
        // the child reachable, so synchronize deepest directories first.
        self.parents.sort_by(|left, right| {
            left.display_path
                .components()
                .count()
                .cmp(&right.display_path.components().count())
                .then_with(|| right.display_path.cmp(&left.display_path))
        });
        let total = self.completed_count.saturating_add(self.parents.len());
        if let Some(recorder) = recorder {
            if !self.unreported_sync_duration.is_zero() {
                recorder.record_duration(
                    crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
                    self.unreported_sync_duration,
                );
            }
            for _ in 0..self.unreported_sync_count {
                recorder.directory_fsync();
            }
            self.unreported_sync_duration = std::time::Duration::ZERO;
            self.unreported_sync_count = 0;
        }
        progress(self.completed_count, total)?;
        while let Some(parent) = self.parents.last() {
            match recorder {
                Some(recorder) => recorder.measure(
                    crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
                    || parent.sync_all(),
                )?,
                None => {
                    let started = std::time::Instant::now();
                    parent.sync_all()?;
                    self.unreported_sync_duration = self
                        .unreported_sync_duration
                        .saturating_add(started.elapsed());
                    self.unreported_sync_count = self.unreported_sync_count.saturating_add(1);
                }
            }
            if let Some(recorder) = recorder {
                recorder.directory_fsync();
            }
            self.parents.pop();
            self.completed_count = self.completed_count.saturating_add(1);
            progress(self.completed_count, total)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.parents.len()
    }

    pub(crate) fn total_count(&self) -> usize {
        self.completed_count.saturating_add(self.parents.len())
    }

    pub(crate) fn completed_count(&self) -> usize {
        self.completed_count
    }
}

impl Default for AnchoredParentSyncBatch {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_content_addressed_bytes(
    file: &mut AnchoredFile,
    path: &Path,
    expected: &[u8],
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<()> {
    let metadata = file.metadata()?;
    let actual = if metadata.len() == expected.len() as u64 {
        measure_anchored_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::ExistingValidationRead,
            || file.read_bounded(expected.len() as u64),
        )?
    } else {
        Vec::new()
    };
    if actual != expected {
        return Err(CheckPoError::Corruption(format!(
            "content-addressed destination conflicts with expected bytes: {}",
            path.display()
        )));
    }
    Ok(())
}

fn measure_anchored_io<T>(
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    kind: crate::checkpoint_metrics::IoTimingKind,
    operation: impl FnOnce() -> T,
) -> T {
    match recorder {
        Some(recorder) => recorder.measure(kind, operation),
        None => operation(),
    }
}

impl AnchoredFile {
    pub(crate) fn metadata(&self) -> Result<fs::Metadata> {
        self.file
            .metadata()
            .map_err(|error| io_error(&self.display_path, error))
    }

    pub(crate) fn is_definitely_on_different_volume(
        &self,
        destination_parent: &AnchoredParent,
    ) -> bool {
        self.identity
            .is_definitely_on_different_volume(&destination_parent.identity)
    }

    fn current_version(&self) -> Result<FileVersion> {
        let metadata = self.metadata()?;
        if !metadata.is_file() {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        FileVersion::from_file_metadata(&self.file, &self.display_path, &metadata)
    }

    pub(crate) fn verify_version(&self, expected: &AnchoredFileVersion) -> Result<()> {
        if self.current_version()? != expected.full {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn set_mtime(&self, modified: std::time::SystemTime) -> Result<()> {
        filetime::set_file_handle_times(
            &self.file,
            None,
            Some(filetime::FileTime::from_system_time(modified)),
        )
        .map_err(|error| io_error(&self.display_path, error))
    }

    pub(crate) fn hash(&mut self) -> Result<AnchoredHash> {
        self.hash_with_poll(|| Ok(()))
    }

    pub(crate) fn hash_with_cancellation(
        &mut self,
        cancellation: Option<&crate::CancellationToken>,
    ) -> Result<AnchoredHash> {
        self.hash_with_poll(|| crate::ensure_not_cancelled(cancellation))
    }

    fn hash_with_poll(&mut self, mut poll: impl FnMut() -> Result<()>) -> Result<AnchoredHash> {
        poll()?;
        let before = self.metadata()?;
        let before_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &before)?;
        if before_version.identity != self.identity || !before.is_file() {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }

        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&self.display_path, error))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            poll()?;
            let read = self
                .file
                .read(&mut buffer)
                .map_err(|error| io_error(&self.display_path, error))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        // Poll once more after EOF so cancellation that arrives during the
        // final read cannot be reported as a successful verification.
        poll()?;

        let after = self.metadata()?;
        let after_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &after)?;
        if after_version != before_version {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        poll()?;
        Ok(AnchoredHash {
            object_id: ObjectId::parse(hasher.finalize().to_hex().as_ref())?,
            metadata: after,
            version: AnchoredFileVersion::from_full(after_version),
        })
    }

    pub(crate) fn read_bounded(&mut self, max_bytes: u64) -> Result<Vec<u8>> {
        let before = self.metadata()?;
        let before_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &before)?;
        if before_version.identity != self.identity || !before.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "anchored path is not a no-follow regular file: {}",
                self.display_path.display()
            )));
        }
        if before.len() > max_bytes {
            return Err(CheckPoError::Corruption(format!(
                "anchored file exceeds maximum size of {max_bytes} bytes: {}",
                self.display_path.display()
            )));
        }
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&self.display_path, error))?;
        let capacity = usize::try_from(before.len()).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        std::io::Read::by_ref(&mut self.file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| io_error(&self.display_path, error))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(CheckPoError::Corruption(format!(
                "anchored file exceeds maximum size of {max_bytes} bytes after growing: {}",
                self.display_path.display()
            )));
        }
        let after = self.metadata()?;
        let after_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &after)?;
        if after_version != before_version {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn copy_and_hash_to(
        &mut self,
        writer: &mut impl Write,
        writer_path: &Path,
    ) -> Result<AnchoredHash> {
        self.copy_and_hash_to_inner(writer, writer_path, None, None)
    }

    /// Materializes this held source into a new destination without resolving
    /// the source by pathname again. CoW clone is attempted where the platform
    /// exposes a held-handle API; unsupported/cross-volume cases fall back to
    /// the same checked streaming copy used elsewhere.
    pub(crate) fn clone_or_copy_to_new(
        &mut self,
        destination_parent: &AnchoredParent,
        destination_leaf: &std::ffi::OsStr,
        destination_path: &Path,
    ) -> Result<AnchoredFile> {
        validate_leaf(destination_leaf, &destination_parent.display_path)?;

        #[cfg(target_os = "macos")]
        {
            let flags = rustix::fs::CloneFlags::NOFOLLOW | rustix::fs::CloneFlags::NOOWNERCOPY;
            match rustix::fs::fclonefileat(
                &self.file,
                &destination_parent.directory,
                destination_leaf,
                flags,
            ) {
                Ok(()) => return destination_parent.open_file(destination_leaf),
                Err(error) if clone_fallback_error(error.raw_os_error()) => {}
                Err(error) => {
                    return Err(io_error(
                        destination_path,
                        std::io::Error::from_raw_os_error(error.raw_os_error()),
                    ))
                }
            }
        }

        let mut output = destination_parent.create_new_file(destination_leaf)?;

        #[cfg(target_os = "linux")]
        match rustix::fs::ioctl_ficlone(&output.file, &self.file) {
            Ok(()) => return Ok(output),
            Err(error) if clone_fallback_error(error.raw_os_error()) => {}
            Err(error) => {
                let result = Err(io_error(
                    destination_path,
                    std::io::Error::from_raw_os_error(error.raw_os_error()),
                ));
                let _ = destination_parent.unlink_file_if_bound(destination_leaf, output);
                return result;
            }
        }

        if let Err(error) = self.copy_and_hash_to(&mut output, destination_path) {
            let _ = destination_parent.unlink_file_if_bound(destination_leaf, output);
            return Err(error);
        }
        Ok(output)
    }

    pub(crate) fn copy_and_hash_to_profiled_with_cancellation(
        &mut self,
        writer: &mut impl Write,
        writer_path: &Path,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
        cancellation: Option<&crate::CancellationToken>,
    ) -> Result<AnchoredHash> {
        self.copy_and_hash_to_inner(writer, writer_path, recorder, cancellation)
    }

    fn copy_and_hash_to_inner(
        &mut self,
        writer: &mut impl Write,
        writer_path: &Path,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
        cancellation: Option<&crate::CancellationToken>,
    ) -> Result<AnchoredHash> {
        crate::ensure_not_cancelled(cancellation)?;
        let before = self.metadata()?;
        let before_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &before)?;
        if before_version.identity != self.identity || !before.is_file() {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }

        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&self.display_path, error))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            crate::ensure_not_cancelled(cancellation)?;
            let read = match recorder {
                Some(recorder) => recorder
                    .measure(crate::checkpoint_metrics::IoTimingKind::SourceRead, || {
                        self.file.read(&mut buffer)
                    }),
                None => self.file.read(&mut buffer),
            }
            .map_err(|error| io_error(&self.display_path, error))?;
            if read == 0 {
                break;
            }
            match recorder {
                Some(recorder) => recorder
                    .measure(crate::checkpoint_metrics::IoTimingKind::Hash, || {
                        hasher.update(&buffer[..read])
                    }),
                None => hasher.update(&buffer[..read]),
            };
            match recorder {
                Some(recorder) => recorder
                    .measure(crate::checkpoint_metrics::IoTimingKind::Write, || {
                        writer.write_all(&buffer[..read])
                    }),
                None => writer.write_all(&buffer[..read]),
            }
            .map_err(|error| io_error(writer_path, error))?;
        }
        crate::ensure_not_cancelled(cancellation)?;

        let after = self.metadata()?;
        let after_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &after)?;
        if after_version != before_version {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        Ok(AnchoredHash {
            object_id: ObjectId::parse(hasher.finalize().to_hex().as_ref())?,
            metadata: after,
            version: AnchoredFileVersion::from_full(after_version),
        })
    }

    pub(crate) fn fingerprint(&self) -> Result<Option<String>> {
        let before = self.metadata()?;
        let before_version =
            FileVersion::from_file_metadata(&self.file, &self.display_path, &before)?;

        #[cfg(unix)]
        let fingerprint = {
            use std::os::unix::fs::MetadataExt;
            Some(format!(
                "unix-v1:{}:{}:{}:{}:{}:{}:{}",
                before.dev(),
                before.ino(),
                before.len(),
                before.mtime(),
                before.mtime_nsec(),
                before.ctime(),
                before.ctime_nsec()
            ))
        };

        #[cfg(windows)]
        let fingerprint = {
            use std::fmt::Write as _;
            let basic_info = windows_file_basic_info(&self.file, &self.display_path)?;
            let mut file_id = String::with_capacity(32);
            for byte in before_version.identity.file_id {
                write!(&mut file_id, "{byte:02x}").expect("writing to String cannot fail");
            }
            Some(format!(
                "windows-v3:{}:{}:{}:{}:{}:{}:{}",
                before_version.identity.volume_serial_number,
                file_id,
                before.len(),
                basic_info.CreationTime,
                basic_info.LastWriteTime,
                basic_info.ChangeTime,
                basic_info.FileAttributes
            ))
        };

        #[cfg(not(any(unix, windows)))]
        let fingerprint = None;

        let after = self.metadata()?;
        if FileVersion::from_file_metadata(&self.file, &self.display_path, &after)?
            != before_version
        {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.display().to_string(),
            ));
        }
        Ok(fingerprint)
    }

    pub(crate) fn sync_all(&self) -> Result<()> {
        match self.file.sync_all() {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                reopen_windows_file_for_durability(&self.file, &self.display_path)?
                    .sync_all()
                    .map_err(|error| io_error(&self.display_path, error))
            }
            Err(error) => Err(io_error(&self.display_path, error)),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn clone_fallback_error(raw_os_error: i32) -> bool {
    raw_os_error == libc::EXDEV
        || raw_os_error == libc::ENOTSUP
        || raw_os_error == libc::EOPNOTSUPP
        || raw_os_error == libc::ENOSYS
        || raw_os_error == libc::EINVAL
        || raw_os_error == libc::ENOTTY
        || raw_os_error == libc::EPERM
}

impl Read for AnchoredFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Write for AnchoredFile {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for AnchoredFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

impl AnchoredParent {
    pub(crate) fn display_path(&self) -> &Path {
        &self.display_path
    }

    pub(crate) fn same_directory(&self, other: &Self) -> bool {
        self.identity == other.identity
    }

    pub(crate) fn sync_all(&self) -> Result<()> {
        self.directory
            .sync_all()
            .map_err(|error| io_error(&self.display_path, error))
    }

    pub(crate) fn create_new_file(&self, leaf: &std::ffi::OsStr) -> Result<AnchoredFile> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        #[cfg(unix)]
        let file = {
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;
            let leaf = std::ffi::CString::new(leaf.as_bytes()).map_err(|_| {
                CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
            })?;
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    leaf.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o666,
                )
            };
            if fd < 0 {
                return Err(io_error(&display_path, std::io::Error::last_os_error()));
            }
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = open_windows_relative_file(&self.directory, leaf, true, true)?;
        #[cfg(not(any(unix, windows)))]
        let file = open_new_portable_file_no_follow(&display_path)?;
        #[cfg(windows)]
        let identity = FileIdentity::from_file(&display_path, &file)?;
        #[cfg(not(windows))]
        let identity = {
            let metadata = file
                .metadata()
                .map_err(|error| io_error(&display_path, error))?;
            FileIdentity::from_metadata(&metadata)?
        };
        Ok(AnchoredFile {
            display_path,
            file,
            identity,
        })
    }

    pub(crate) fn create_unique_temporary_file(
        &self,
        purpose: &str,
    ) -> Result<(std::ffi::OsString, AnchoredFile)> {
        for _ in 0..16 {
            let leaf = std::ffi::OsString::from(format!(
                ".checkpo-{purpose}-{}.tmp",
                uuid::Uuid::new_v4().simple()
            ));
            match self.create_new_file(&leaf) {
                Ok(file) => return Ok((leaf, file)),
                Err(CheckPoError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
        Err(CheckPoError::Unexpected(format!(
            "could not allocate a unique atomic-write temporary below {}",
            self.display_path.display()
        )))
    }

    #[cfg(windows)]
    fn create_windows_replace_record(
        &self,
        destination_leaf: &std::ffi::OsStr,
        temporary_leaf: &std::ffi::OsStr,
        tombstone_leaf: &std::ffi::OsStr,
        old_identity: FileIdentity,
        new_identity: FileIdentity,
    ) -> Result<(std::ffi::OsString, AnchoredFile)> {
        use std::os::windows::ffi::OsStrExt;

        let record_leaf = windows_replace_record_leaf(destination_leaf);
        let record = WindowsReplaceRecoveryRecord {
            version: 1,
            destination_leaf_utf16: destination_leaf.encode_wide().collect(),
            temporary_leaf_utf16: temporary_leaf.encode_wide().collect(),
            tombstone_leaf_utf16: tombstone_leaf.encode_wide().collect(),
            old_volume_serial_number: old_identity.volume_serial_number,
            old_file_id: old_identity.file_id,
            new_volume_serial_number: new_identity.volume_serial_number,
            new_file_id: new_identity.file_id,
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|error| json_error(self.display_path.join(&record_leaf), error))?;
        let mut record_file = self.create_new_file(&record_leaf)?;
        let publication = (|| {
            record_file
                .write_all(&bytes)
                .map_err(|error| io_error(self.display_path.join(&record_leaf), error))?;
            record_file.sync_all()?;
            self.sync_all()
        })();
        if let Err(error) = publication {
            let _ = self.unlink_file_if_bound_ref(&record_leaf, &record_file);
            let _ = self.sync_all();
            return Err(error);
        }
        Ok((record_leaf, record_file))
    }

    #[cfg(windows)]
    fn open_optional_windows_file(&self, leaf: &std::ffi::OsStr) -> Result<Option<AnchoredFile>> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        match open_windows_relative_file(&self.directory, leaf, false, false) {
            Ok(file) => anchored_file_from_open_file(display_path, file).map(Some),
            Err(CheckPoError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(windows)]
    fn open_windows_finalization_guard(
        &self,
        leaf: &std::ffi::OsStr,
        expected_identity: FileIdentity,
    ) -> Result<File> {
        let display_path = self.display_path.join(leaf);
        let guard = match open_windows_relative_file_for_finalization(&self.directory, leaf) {
            Ok(guard) => guard,
            Err(CheckPoError::Io { source, .. }) if source.raw_os_error() == Some(32) => {
                return Err(CheckPoError::WorkingTreeChanged(
                    display_path.display().to_string(),
                ))
            }
            Err(error) => return Err(error),
        };
        if FileIdentity::from_file(&display_path, &guard)? != expected_identity {
            return Err(CheckPoError::WorkingTreeChanged(
                display_path.display().to_string(),
            ));
        }
        Ok(guard)
    }

    #[cfg(windows)]
    fn recover_windows_replace_record(&self, destination_leaf: &std::ffi::OsStr) -> Result<bool> {
        validate_leaf(destination_leaf, &self.display_path)?;
        let record_leaf = windows_replace_record_leaf(destination_leaf);
        self.recover_windows_replace_record_at(destination_leaf, &record_leaf)
    }

    #[cfg(windows)]
    fn recover_windows_replace_record_case_insensitive(
        &self,
        destination_leaf: &std::ffi::OsStr,
    ) -> Result<bool> {
        use std::os::windows::ffi::OsStrExt;

        const MAX_RECORD_BYTES: u64 = 64 * 1024;
        validate_leaf(destination_leaf, &self.display_path)?;
        let requested = destination_leaf.encode_wide().collect::<Vec<_>>();
        let entries = super::windows_durability::list_directory_entries(&self.directory)
            .map_err(|error| io_error(&self.display_path, error))?;
        for (leaf, is_directory, is_reparse_point) in entries {
            let name = leaf.to_string_lossy();
            if is_directory
                || is_reparse_point
                || !name.starts_with(".checkpo-replace-")
                || !name.ends_with(".json")
            {
                continue;
            }
            let Some(mut record_file) = self.open_optional_windows_file(&leaf)? else {
                continue;
            };
            let bytes = record_file.read_bounded(MAX_RECORD_BYTES)?;
            let record: WindowsReplaceRecoveryRecord = serde_json::from_slice(&bytes)
                .map_err(|error| json_error(self.display_path.join(&leaf), error))?;
            let matches = super::windows_durability::windows_names_equal(
                &record.destination_leaf_utf16,
                &requested,
            )
            .map_err(|error| io_error(self.display_path.join(&leaf), error))?;
            if matches {
                return self.recover_windows_replace_record_at(destination_leaf, &leaf);
            }
        }
        Ok(false)
    }

    #[cfg(windows)]
    fn recover_windows_replace_record_at(
        &self,
        destination_leaf: &std::ffi::OsStr,
        record_leaf: &std::ffi::OsStr,
    ) -> Result<bool> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::ffi::OsStringExt;

        const MAX_RECORD_BYTES: u64 = 64 * 1024;
        validate_leaf(destination_leaf, &self.display_path)?;
        validate_leaf(record_leaf, &self.display_path)?;
        let Some(mut record_file) = self.open_optional_windows_file(record_leaf)? else {
            return Ok(false);
        };
        let bytes = record_file.read_bounded(MAX_RECORD_BYTES)?;
        let record: WindowsReplaceRecoveryRecord = serde_json::from_slice(&bytes)
            .map_err(|error| json_error(self.display_path.join(record_leaf), error))?;
        if record.version != 1 {
            return Err(CheckPoError::Corruption(format!(
                "unsupported Windows replace recovery record version {}: {}",
                record.version,
                self.display_path.join(record_leaf).display()
            )));
        }

        let recorded_destination = std::ffi::OsString::from_wide(&record.destination_leaf_utf16);
        let temporary_leaf = std::ffi::OsString::from_wide(&record.temporary_leaf_utf16);
        let tombstone_leaf = std::ffi::OsString::from_wide(&record.tombstone_leaf_utf16);
        validate_leaf(&recorded_destination, &self.display_path)?;
        validate_leaf(&temporary_leaf, &self.display_path)?;
        validate_leaf(&tombstone_leaf, &self.display_path)?;
        let recorded_matches = super::windows_durability::windows_names_equal(
            &record.destination_leaf_utf16,
            &destination_leaf.encode_wide().collect::<Vec<_>>(),
        )
        .map_err(|error| io_error(self.display_path.join(record_leaf), error))?;
        if !recorded_matches
            || !tombstone_leaf
                .to_string_lossy()
                .starts_with(".checkpo-replace-")
            || !tombstone_leaf.to_string_lossy().ends_with(".tomb")
        {
            return Err(CheckPoError::Corruption(format!(
                "invalid Windows replace recovery mapping: {}",
                self.display_path.join(record_leaf).display()
            )));
        }

        let old_identity = FileIdentity {
            volume_serial_number: record.old_volume_serial_number,
            file_id: record.old_file_id,
        };
        let new_identity = FileIdentity {
            volume_serial_number: record.new_volume_serial_number,
            file_id: record.new_file_id,
        };
        if old_identity == new_identity {
            return Err(CheckPoError::Corruption(format!(
                "Windows replace recovery record reuses one FileId: {}",
                self.display_path.join(record_leaf).display()
            )));
        }

        let destination = self.open_optional_windows_file(destination_leaf)?;
        let tombstone = self.open_optional_windows_file(&tombstone_leaf)?;
        let temporary = self.open_optional_windows_file(&temporary_leaf)?;

        let finalization_guard;
        match destination.as_ref().map(|file| file.identity) {
            Some(identity) if identity == old_identity => {
                if tombstone.is_some() {
                    return Err(CheckPoError::Corruption(format!(
                        "Windows replace recovery found both old destination and tombstone: {}",
                        self.display_path.join(destination_leaf).display()
                    )));
                }
                // Seeing the old name in-process is not proof that a failed
                // earlier rename barrier is durable. Establish that proof
                // before deleting the staged new FileId.
                finalization_guard =
                    self.open_windows_finalization_guard(destination_leaf, old_identity)?;
                self.sync_all()?;
                if let Some(temporary) = temporary {
                    if temporary.identity == new_identity {
                        self.unlink_file_if_bound_ref(&temporary_leaf, &temporary)?;
                        self.sync_all()?;
                    }
                }
            }
            Some(identity) if identity == new_identity => {
                // Likewise, never interpret a visible new destination as a
                // committed replacement until its directory barrier succeeds.
                // This is essential when recovery follows a sync error in the
                // same process rather than a completed reboot.
                finalization_guard =
                    self.open_windows_finalization_guard(destination_leaf, new_identity)?;
                self.sync_all()?;
                if let Some(tombstone) = tombstone {
                    if tombstone.identity != old_identity {
                        return Err(CheckPoError::Corruption(format!(
                            "Windows replace tombstone FileId changed: {}",
                            self.display_path.join(&tombstone_leaf).display()
                        )));
                    }
                    self.unlink_file_if_bound_ref(&tombstone_leaf, &tombstone)?;
                    self.sync_all()?;
                }
            }
            Some(_) => {
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path
                        .join(destination_leaf)
                        .display()
                        .to_string(),
                ))
            }
            None => {
                let tombstone = tombstone.ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "Windows replace recovery lost both destination and tombstone: {}",
                        self.display_path.join(destination_leaf).display()
                    ))
                })?;
                if tombstone.identity != old_identity {
                    return Err(CheckPoError::Corruption(format!(
                        "Windows replace tombstone FileId changed: {}",
                        self.display_path.join(&tombstone_leaf).display()
                    )));
                }
                let movable =
                    open_windows_relative_file_for_removal(&self.directory, &tombstone_leaf)?;
                if FileIdentity::from_file(&self.display_path.join(&tombstone_leaf), &movable)?
                    != old_identity
                {
                    return Err(CheckPoError::WorkingTreeChanged(
                        self.display_path
                            .join(&tombstone_leaf)
                            .display()
                            .to_string(),
                    ));
                }
                super::windows_durability::rename_open_handle_no_replace_unflushed(
                    &movable,
                    &self.directory,
                    destination_leaf,
                )
                .map_err(|error| io_error(self.display_path.join(destination_leaf), error))?;
                drop(movable);
                finalization_guard =
                    self.open_windows_finalization_guard(destination_leaf, old_identity)?;
                self.sync_all()?;
                if let Some(temporary) = temporary {
                    if temporary.identity == new_identity {
                        self.unlink_file_if_bound_ref(&temporary_leaf, &temporary)?;
                        self.sync_all()?;
                    }
                }
            }
        }

        self.unlink_file_if_bound_ref(record_leaf, &record_file)?;
        self.sync_all()?;
        drop(finalization_guard);
        Ok(true)
    }

    fn write_bytes_atomic(
        &self,
        destination_leaf: &std::ffi::OsStr,
        bytes: &[u8],
        create_new: bool,
    ) -> Result<()> {
        validate_leaf(destination_leaf, &self.display_path)?;
        let (temporary_leaf, mut temporary) = self.create_unique_temporary_file("write")?;
        let result = (|| {
            temporary
                .write_all(bytes)
                .map_err(|error| io_error(&temporary.display_path, error))?;
            temporary.sync_all()?;

            let destination = match self.open_file(destination_leaf) {
                Ok(file) => Some(file),
                Err(CheckPoError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            if create_new && destination.is_some() {
                return Err(io_error(
                    self.display_path.join(destination_leaf),
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "atomic create destination already exists",
                    ),
                ));
            }

            match destination {
                None => {
                    self.rename_no_replace_to(&temporary_leaf, &temporary, self, destination_leaf)?
                }
                Some(destination) => {
                    self.replace_from_temporary(
                        &temporary_leaf,
                        &temporary,
                        destination_leaf,
                        &destination,
                    )?;
                }
            }
            self.verify_file_binding(destination_leaf, &temporary)?;
            self.sync_all()
        })();

        if result.is_err() {
            // Cleanup is identity-bound. If publication already moved the
            // temporary inode, or an attacker replaced its private name, this
            // call refuses to delete the mismatching entry.
            let _ = self.unlink_file_if_bound_ref(&temporary_leaf, &temporary);
            let _ = self.sync_all();
        }
        result
    }

    fn replace_from_temporary(
        &self,
        temporary_leaf: &std::ffi::OsStr,
        temporary: &AnchoredFile,
        destination_leaf: &std::ffi::OsStr,
        destination: &AnchoredFile,
    ) -> Result<()> {
        let result = self.replace_from_temporary_inner(
            temporary_leaf,
            temporary,
            destination_leaf,
            destination,
            true,
            || {},
            #[cfg(windows)]
            |_| Ok(()),
        );
        #[cfg(windows)]
        if result.is_err() {
            let _ = self.recover_windows_replace_record(destination_leaf);
        }
        result
    }

    pub(crate) fn replace_from_temporary_batched(
        &self,
        temporary_leaf: &std::ffi::OsStr,
        temporary: &AnchoredFile,
        destination_leaf: &std::ffi::OsStr,
        destination: &AnchoredFile,
        sync_batch: &mut AnchoredParentSyncBatch,
    ) -> Result<()> {
        let result = self.replace_from_temporary_inner(
            temporary_leaf,
            temporary,
            destination_leaf,
            destination,
            false,
            || {},
            #[cfg(windows)]
            |_| Ok(()),
        );
        #[cfg(windows)]
        if result.is_err() {
            let _ = self.recover_windows_replace_record(destination_leaf);
        }
        match result {
            Ok(()) => self.defer_sync(sync_batch),
            Err(error) => {
                // The exchange may already have changed the directory before
                // a later identity check failed. Do not let an error path drop
                // the only durability barrier for that namespace mutation.
                let _ = self.sync_all();
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_from_temporary_inner(
        &self,
        temporary_leaf: &std::ffi::OsStr,
        temporary: &AnchoredFile,
        destination_leaf: &std::ffi::OsStr,
        destination: &AnchoredFile,
        sync_immediately: bool,
        before_replace: impl FnOnce(),
        #[cfg(windows)] mut after_windows_phase: impl FnMut(ReplaceProtocolPhase) -> Result<()>,
    ) -> Result<()> {
        #[cfg(windows)]
        self.recover_windows_replace_record(destination_leaf)?;
        self.verify_file_binding(temporary_leaf, temporary)?;
        self.verify_file_binding(destination_leaf, destination)?;
        before_replace();

        #[cfg(unix)]
        let result = {
            anchored_exchange_files(self, temporary_leaf, self, destination_leaf)
                .map_err(|error| io_error(self.display_path.join(destination_leaf), error))?;
            self.verify_file_binding(destination_leaf, temporary)?;
            self.verify_file_binding(temporary_leaf, destination)?;
            // Immediate callers preserve the original two-barrier ordering.
            // Batched content-addressed publication can defer both namespace
            // updates to one held-parent barrier before checkpoint commit.
            if sync_immediately {
                self.sync_all()?;
            }
            self.unlink_file_if_bound_ref(temporary_leaf, destination)?;
            if sync_immediately {
                self.sync_all()?;
            }
            Ok(())
        };

        #[cfg(windows)]
        let result = {
            let source = open_windows_relative_file_for_mutation(&self.directory, temporary_leaf)?;
            let source_path = self.display_path.join(temporary_leaf);
            if FileIdentity::from_file(&source_path, &source)? != temporary.identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    source_path.display().to_string(),
                ));
            }
            let displaced =
                open_windows_relative_file_for_removal(&self.directory, destination_leaf)?;
            let destination_path = self.display_path.join(destination_leaf);
            if FileIdentity::from_file(&destination_path, &displaced)? != destination.identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    destination_path.display().to_string(),
                ));
            }

            // Windows has no compare-and-replace-by-FileId primitive. Persist
            // the exact rollback mapping before detaching the verified old
            // destination, then publish with no-replace semantics. A crash can
            // therefore restore the old FileId without guessing its leaf.
            let tombstone = std::ffi::OsString::from(format!(
                ".checkpo-replace-{}.tomb",
                uuid::Uuid::new_v4().simple()
            ));
            let (record_leaf, record_file) = self.create_windows_replace_record(
                destination_leaf,
                temporary_leaf,
                &tombstone,
                destination.identity,
                temporary.identity,
            )?;
            after_windows_phase(ReplaceProtocolPhase::RecoveryRecordDurable)?;

            super::windows_durability::rename_open_handle_no_replace_unflushed(
                &displaced,
                &self.directory,
                &tombstone,
            )
            .map_err(|error| io_error(&destination_path, error))?;

            // Make the recoverable old destination durable before opening the
            // brief no-destination interval required by the Windows protocol.
            self.sync_all()?;
            after_windows_phase(ReplaceProtocolPhase::DestinationDetached)?;
            if let Err(error) = super::windows_durability::rename_open_handle_no_replace(
                &source,
                &self.directory,
                destination_leaf,
            ) {
                let _ = super::windows_durability::rename_open_handle_no_replace_unflushed(
                    &displaced,
                    &self.directory,
                    destination_leaf,
                );
                let _ = self.sync_all();
                return Err(io_error(destination_path, error));
            }
            // The rename handle requested DELETE access, so release it before
            // acquiring a no-FILE_SHARE_DELETE guard on the published name.
            // The caller's ordinary read/write handle does not request DELETE
            // and remains compatible with this guard.
            drop(source);
            let finalization_guard =
                self.open_windows_finalization_guard(destination_leaf, temporary.identity)?;
            // The new destination must be durable before deleting the only
            // recovery copy of the displaced file.
            self.sync_all()?;
            after_windows_phase(ReplaceProtocolPhase::ReplacementPublished)?;
            super::windows_durability::remove_open_handle(displaced, &self.directory, &tombstone)
                .map_err(|error| io_error(self.display_path.join(&tombstone), error))?;
            // The tombstone deletion must be durable while the recovery record
            // still exists. If record deletion later rolls back after a crash,
            // recovery sees new-at-destination and an already-absent tombstone.
            self.sync_all()?;
            self.unlink_file_if_bound_ref(&record_leaf, &record_file)?;
            if sync_immediately {
                self.sync_all()?;
            }
            drop(finalization_guard);
            Ok(())
        };

        #[cfg(not(any(unix, windows)))]
        let result = {
            fs::rename(
                self.display_path.join(temporary_leaf),
                self.display_path.join(destination_leaf),
            )
            .map_err(|error| io_error(self.display_path.join(destination_leaf), error))?;
            self.verify_file_binding(destination_leaf, temporary)?;
            if sync_immediately {
                self.sync_all()?;
            }
            Ok(())
        };

        result
    }

    #[cfg(test)]
    fn replace_from_temporary_with_hook(
        &self,
        temporary_leaf: &std::ffi::OsStr,
        temporary: &AnchoredFile,
        destination_leaf: &std::ffi::OsStr,
        destination: &AnchoredFile,
        before_replace: impl FnOnce(),
    ) -> Result<()> {
        self.replace_from_temporary_inner(
            temporary_leaf,
            temporary,
            destination_leaf,
            destination,
            true,
            before_replace,
            #[cfg(windows)]
            |_| Ok(()),
        )
    }

    #[cfg(all(test, windows))]
    fn replace_from_temporary_stopping_at_windows_phase(
        &self,
        temporary_leaf: &std::ffi::OsStr,
        temporary: &AnchoredFile,
        destination_leaf: &std::ffi::OsStr,
        destination: &AnchoredFile,
        stop_at: ReplaceProtocolPhase,
    ) -> Result<()> {
        self.replace_from_temporary_inner(
            temporary_leaf,
            temporary,
            destination_leaf,
            destination,
            true,
            || {},
            |phase| {
                if phase == stop_at {
                    Err(CheckPoError::Unexpected(format!(
                        "simulated crash at {phase:?}"
                    )))
                } else {
                    Ok(())
                }
            },
        )
    }

    #[cfg(all(test, windows))]
    fn replace_from_temporary_with_windows_phase_hook(
        &self,
        temporary_leaf: &std::ffi::OsStr,
        temporary: &AnchoredFile,
        destination_leaf: &std::ffi::OsStr,
        destination: &AnchoredFile,
        after_windows_phase: impl FnMut(ReplaceProtocolPhase) -> Result<()>,
    ) -> Result<()> {
        self.replace_from_temporary_inner(
            temporary_leaf,
            temporary,
            destination_leaf,
            destination,
            true,
            || {},
            after_windows_phase,
        )
    }

    pub(crate) fn defer_sync(&self, sync_batch: &mut AnchoredParentSyncBatch) -> Result<()> {
        sync_batch.record_directory_handle(&self.display_path, &self.directory)
    }

    pub(crate) fn create_directory(&self, leaf: &std::ffi::OsStr) -> Result<AnchoredParent> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            create_unix_directory_component_exclusive(
                self.directory.as_raw_fd(),
                leaf,
                &display_path,
            )?;
        }
        #[cfg(windows)]
        let directory = open_windows_relative_directory(&self.directory, leaf, true, true)?;
        #[cfg(not(any(unix, windows)))]
        {
            fs::create_dir(&display_path).map_err(|error| io_error(&display_path, error))?;
        }
        #[cfg(not(windows))]
        let directory = {
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                open_unix_component(
                    self.directory.as_raw_fd(),
                    leaf,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    &display_path,
                )?
            }
            #[cfg(not(any(unix, windows)))]
            {
                File::open(&display_path).map_err(|error| io_error(&display_path, error))?
            }
        };
        let metadata = directory
            .metadata()
            .map_err(|error| io_error(&display_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(CheckPoError::Corruption(format!(
                "created anchored path is not a directory: {}",
                display_path.display()
            )));
        }
        #[cfg(windows)]
        let identity = FileIdentity::from_file(&display_path, &directory)?;
        #[cfg(not(windows))]
        let identity = FileIdentity::from_metadata(&metadata)?;
        Ok(AnchoredParent {
            display_path,
            directory,
            identity,
        })
    }

    pub(crate) fn open_directory(&self, leaf: &std::ffi::OsStr) -> Result<AnchoredParent> {
        self.open_directory_impl(leaf, false)
    }

    pub(crate) fn open_directory_for_mutation(
        &self,
        leaf: &std::ffi::OsStr,
    ) -> Result<AnchoredParent> {
        self.open_directory_impl(leaf, true)
    }

    fn open_directory_impl(
        &self,
        leaf: &std::ffi::OsStr,
        _writable: bool,
    ) -> Result<AnchoredParent> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        #[cfg(unix)]
        let directory = {
            use std::os::fd::AsRawFd;
            open_unix_component(
                self.directory.as_raw_fd(),
                leaf,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                &display_path,
            )?
        };
        #[cfg(windows)]
        let directory = open_windows_relative_directory(&self.directory, leaf, false, _writable)?;
        #[cfg(not(any(unix, windows)))]
        let directory =
            File::open(&display_path).map_err(|error| io_error(&display_path, error))?;
        let metadata = directory
            .metadata()
            .map_err(|error| io_error(&display_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(CheckPoError::Corruption(format!(
                "anchored path is not a directory: {}",
                display_path.display()
            )));
        }
        #[cfg(windows)]
        let identity = FileIdentity::from_file(&display_path, &directory)?;
        #[cfg(not(windows))]
        let identity = FileIdentity::from_metadata(&metadata)?;
        Ok(AnchoredParent {
            display_path,
            directory,
            identity,
        })
    }

    #[cfg(not(windows))]
    pub(crate) fn rename_directory_no_replace_to(
        &self,
        source_leaf: &std::ffi::OsStr,
        expected_source: &AnchoredParent,
        destination_parent: &AnchoredParent,
        destination_leaf: &std::ffi::OsStr,
    ) -> Result<()> {
        validate_leaf(source_leaf, &self.display_path)?;
        validate_leaf(destination_leaf, &destination_parent.display_path)?;
        let current = self.open_directory(source_leaf)?;
        if !current.same_directory(expected_source) {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.join(source_leaf).display().to_string(),
            ));
        }
        anchored_rename_directory_no_replace(
            self,
            source_leaf,
            expected_source,
            destination_parent,
            destination_leaf,
        )
        .map_err(|error| {
            io_error(
                destination_parent.display_path.join(destination_leaf),
                error,
            )
        })?;
        let published = destination_parent.open_directory(destination_leaf)?;
        if !published.same_directory(expected_source) {
            return Err(CheckPoError::WorkingTreeChanged(
                destination_parent
                    .display_path
                    .join(destination_leaf)
                    .display()
                    .to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn rename_directory_no_replace_to_owned(
        &self,
        source_leaf: &std::ffi::OsStr,
        expected_source: AnchoredParent,
        destination_parent: &AnchoredParent,
        destination_leaf: &std::ffi::OsStr,
    ) -> Result<()> {
        #[cfg(not(windows))]
        return self.rename_directory_no_replace_to(
            source_leaf,
            &expected_source,
            destination_parent,
            destination_leaf,
        );

        #[cfg(windows)]
        {
            validate_leaf(source_leaf, &self.display_path)?;
            validate_leaf(destination_leaf, &destination_parent.display_path)?;
            let current = self.open_directory_for_mutation(source_leaf)?;
            if !current.same_directory(&expected_source) {
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.join(source_leaf).display().to_string(),
                ));
            }
            drop(current);
            let source_path = self.display_path.join(source_leaf);
            let expected_identity = expected_source.identity;
            let source =
                reopen_windows_directory_for_removal(&expected_source.directory, &source_path)?;
            drop(expected_source);
            if let Err(error) = super::windows_durability::rename_open_directory_handle_no_replace(
                &source,
                &destination_parent.directory,
                destination_leaf,
            ) {
                // NtSetInformationFile may have committed the rename before a
                // later flush/readback failed. Always issue both barriers on an
                // error path so callers never mistake it for "not published".
                let _ = self.sync_all();
                let _ = destination_parent.sync_all();
                return Err(io_error(
                    destination_parent.display_path.join(destination_leaf),
                    error,
                ));
            }
            let published = destination_parent.open_directory(destination_leaf)?;
            if published.identity != expected_identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    destination_parent
                        .display_path
                        .join(destination_leaf)
                        .display()
                        .to_string(),
                ));
            }
            Ok(())
        }
    }

    pub(crate) fn open_file(&self, leaf: &std::ffi::OsStr) -> Result<AnchoredFile> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        #[cfg(unix)]
        let file = {
            use std::os::fd::AsRawFd;
            open_unix_component(
                self.directory.as_raw_fd(),
                leaf,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                &display_path,
            )?
        };
        #[cfg(windows)]
        let file = match open_windows_relative_file(&self.directory, leaf, false, false) {
            Ok(file) => file,
            Err(error)
                if matches!(
                    &error,
                    CheckPoError::Io { source, .. }
                        if source.kind() == std::io::ErrorKind::NotFound
                ) =>
            {
                // A crash between the two Windows no-replace renames leaves a
                // durable recovery record and tombstone. Only a missing target
                // takes this path, so ordinary scanner reads pay no extra open.
                let directory =
                    reopen_windows_directory_for_mutation(&self.directory, &self.display_path)?;
                let recovery_parent = AnchoredParent {
                    display_path: self.display_path.clone(),
                    identity: self.identity,
                    directory,
                };
                if !recovery_parent.recover_windows_replace_record(leaf)?
                    && !recovery_parent.recover_windows_replace_record_case_insensitive(leaf)?
                {
                    return Err(error);
                }
                open_windows_relative_file(&self.directory, leaf, false, false)?
            }
            Err(error) => return Err(error),
        };
        #[cfg(not(any(unix, windows)))]
        let file = open_read_only_portable_file_no_follow(&display_path)?;
        anchored_file_from_open_file(display_path, file)
    }

    #[cfg(windows)]
    pub(crate) fn open_file_without_write_sharing(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
    ) -> Result<AnchoredFile> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        let file = match open_windows_relative_file_for_removal(&self.directory, leaf) {
            Ok(file) => file,
            Err(CheckPoError::Io { source, .. }) if source.raw_os_error() == Some(32) => {
                return Err(CheckPoError::WorkingTreeChanged(
                    display_path.display().to_string(),
                ))
            }
            Err(error) => return Err(error),
        };
        let guarded = anchored_file_from_open_file(display_path.clone(), file)?;
        if guarded.identity != expected.identity {
            return Err(CheckPoError::WorkingTreeChanged(
                display_path.display().to_string(),
            ));
        }
        Ok(guarded)
    }

    pub(crate) fn inspect_metadata_no_follow(
        &self,
        leaf: &std::ffi::OsStr,
    ) -> Result<AnchoredFileMetadata> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);

        #[cfg(unix)]
        {
            use std::mem::MaybeUninit;
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;

            let leaf = std::ffi::CString::new(leaf.as_bytes()).map_err(|_| {
                CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
            })?;
            let mut stat = MaybeUninit::<libc::stat>::uninit();
            let result = unsafe {
                libc::fstatat(
                    self.directory.as_raw_fd(),
                    leaf.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                return Err(io_error(&display_path, std::io::Error::last_os_error()));
            }
            let stat = unsafe { stat.assume_init() };
            let file_kind = stat.st_mode & libc::S_IFMT;
            let is_regular = file_kind == libc::S_IFREG;
            let is_link = file_kind == libc::S_IFLNK;
            let (mtime_seconds, mtime_nanoseconds) = unix_stat_mtime(&stat);
            let (ctime_seconds, ctime_nanoseconds) = unix_stat_ctime(&stat);
            let size_bytes = u64::try_from(stat.st_size).map_err(|_| {
                CheckPoError::Corruption(format!(
                    "file has a negative length: {}",
                    display_path.display()
                ))
            })?;
            Ok(AnchoredFileMetadata {
                size_bytes,
                modified: unix_system_time(mtime_seconds, mtime_nanoseconds, &display_path)?,
                fingerprint: Some(format!(
                    "unix-v1:{}:{}:{}:{}:{}:{}:{}",
                    stat.st_dev,
                    stat.st_ino,
                    size_bytes,
                    mtime_seconds,
                    mtime_nanoseconds,
                    ctime_seconds,
                    ctime_nanoseconds
                )),
                is_regular,
                is_link,
            })
        }

        #[cfg(not(unix))]
        {
            // Windows needs an opened handle to obtain its strong file id and
            // change-time fingerprint. Keep that platform-specific cost while
            // exposing the same scanner API.
            let file = self.open_file(leaf)?;
            let metadata = file.metadata()?;
            Ok(AnchoredFileMetadata {
                size_bytes: metadata.len(),
                modified: metadata
                    .modified()
                    .map_err(|error| io_error(&display_path, error))?,
                fingerprint: file.fingerprint()?,
                is_regular: metadata.is_file(),
                is_link: crate::metadata_is_link_or_reparse(&metadata),
            })
        }
    }

    pub(crate) fn open_file_read_write(&self, leaf: &std::ffi::OsStr) -> Result<AnchoredFile> {
        validate_leaf(leaf, &self.display_path)?;
        let display_path = self.display_path.join(leaf);
        #[cfg(unix)]
        let file = {
            use std::os::fd::AsRawFd;
            open_unix_component(
                self.directory.as_raw_fd(),
                leaf,
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                &display_path,
            )?
        };
        #[cfg(windows)]
        let file = open_windows_relative_file(&self.directory, leaf, true, false)?;
        #[cfg(not(any(unix, windows)))]
        let file = open_existing_portable_file_no_follow(&display_path)?;
        let metadata = file
            .metadata()
            .map_err(|error| io_error(&display_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "anchored path is not a regular file: {}",
                display_path.display()
            )));
        }
        #[cfg(windows)]
        let identity = FileIdentity::from_file(&display_path, &file)?;
        #[cfg(not(windows))]
        let identity = FileIdentity::from_metadata(&metadata)?;
        Ok(AnchoredFile {
            display_path,
            file,
            identity,
        })
    }

    pub(crate) fn verify_file_binding(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
    ) -> Result<()> {
        // Binding verification must also work for read-only CAS/source files.
        // It only compares identity, so requesting write access was both
        // unnecessary and capable of rejecting a valid source.
        let current = self.open_file(leaf)?;
        if current.identity != expected.identity {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.join(leaf).display().to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn rename_no_replace_to(
        &self,
        source_leaf: &std::ffi::OsStr,
        expected_source: &AnchoredFile,
        destination_parent: &AnchoredParent,
        destination_leaf: &std::ffi::OsStr,
    ) -> Result<()> {
        self.rename_no_replace_to_inner(
            source_leaf,
            expected_source,
            (destination_parent, destination_leaf),
            || {},
            || {},
            || {},
        )
    }

    fn rename_no_replace_to_inner(
        &self,
        source_leaf: &std::ffi::OsStr,
        expected_source: &AnchoredFile,
        destination: (&AnchoredParent, &std::ffi::OsStr),
        before_verify: impl FnOnce(),
        after_verify: impl FnOnce(),
        after_publish: impl FnOnce(),
    ) -> Result<()> {
        let (destination_parent, destination_leaf) = destination;
        validate_leaf(source_leaf, &self.display_path)?;
        validate_leaf(destination_leaf, &destination_parent.display_path)?;
        before_verify();
        self.verify_file_binding(source_leaf, expected_source)?;
        after_verify();
        if let Err(error) = anchored_rename_no_replace(
            self,
            source_leaf,
            expected_source,
            destination_parent,
            destination_leaf,
        ) {
            #[cfg(windows)]
            {
                // A post-commit verification error is still a namespace
                // mutation. Flush both parents before returning the failure.
                let _ = self.sync_all();
                let _ = destination_parent.sync_all();
            }
            return Err(io_error(
                destination_parent.display_path.join(destination_leaf),
                error,
            ));
        }
        after_publish();
        if let Err(error) =
            destination_parent.verify_file_binding(destination_leaf, expected_source)
        {
            // Never roll back by pathname alone. If the destination entry was
            // swapped after publication, a plain unlink would delete the
            // replacement rather than the file that this operation moved.
            // The bound unlink either removes `expected_source` through a
            // private tombstone or preserves the mismatching entry.
            let _ = destination_parent.unlink_file_if_bound_ref(destination_leaf, expected_source);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn unlink_file_if_bound(
        &self,
        leaf: &std::ffi::OsStr,
        expected: AnchoredFile,
    ) -> Result<()> {
        self.unlink_file_if_bound_ref(leaf, &expected)
    }

    fn unlink_file_if_bound_ref(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
    ) -> Result<()> {
        self.unlink_file_if_bound_inner(leaf, expected, || {}, |_| {}, |_| {})
    }

    pub(crate) fn unlink_file_if_bound_versioned(
        &self,
        leaf: &std::ffi::OsStr,
        expected: AnchoredFile,
        version: AnchoredFileVersion,
    ) -> Result<()> {
        self.unlink_file_if_bound_versioned_inner(leaf, &expected, &version, || {}, |_| {}, |_| {})
    }

    fn unlink_file_if_bound_versioned_inner(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
        version: &AnchoredFileVersion,
        before_detach: impl FnOnce(),
        after_detach: impl FnOnce(&std::ffi::OsStr),
        before_unlink: impl FnOnce(&std::ffi::OsStr),
    ) -> Result<()> {
        #[cfg(windows)]
        {
            expected.verify_version(version)?;
            self.verify_file_binding(leaf, expected)?;
            before_detach();
            let _ = after_detach;
            let file = open_windows_relative_file_for_removal(&self.directory, leaf)?;
            let display_path = self.display_path.join(leaf);
            if FileIdentity::from_file(&display_path, &file)? != expected.identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    display_path.display().to_string(),
                ));
            }
            let metadata = file
                .metadata()
                .map_err(|error| io_error(&display_path, error))?;
            if FileVersion::from_file_metadata(&file, &display_path, &metadata)? != version.full {
                return Err(CheckPoError::WorkingTreeChanged(
                    display_path.display().to_string(),
                ));
            }
            before_unlink(leaf);
            super::windows_durability::remove_open_handle(file, &self.directory, leaf)
                .map_err(|error| io_error(display_path, error))
        }

        #[cfg(unix)]
        {
            // The complete version captured by the hash is checked twice before
            // detaching the name. Capturing a fresh token here would admit a
            // same-inode write that raced after the verified hash.
            expected.verify_version(version)?;
            self.verify_file_binding(leaf, expected)?;
            expected.verify_version(version)?;
            before_detach();
            let tombstone = self.detach_file_to_unique_tombstone(leaf, expected)?;
            after_detach(&tombstone);
            let detached = match self.open_file(&tombstone) {
                Ok(detached) => detached,
                Err(_) => {
                    self.sync_all()?;
                    return Err(CheckPoError::WorkingTreeChanged(
                        self.display_path.join(&tombstone).display().to_string(),
                    ));
                }
            };

            // POSIX rename changes ctime, so the post-detach version cannot be
            // compared wholesale with the pre-rename token. Identity, length
            // and mtime remain stable and must still match the hashed source.
            let first_detached_version = match detached.current_version() {
                Ok(version) => version,
                Err(_) => return self.rollback_versioned_tombstone(leaf, &tombstone, &detached),
            };
            if first_detached_version.identity != version.stable_content.identity {
                // A replacement, rather than the hashed source, was detached.
                // Never move that replacement back over the original name.
                self.sync_all()?;
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.join(&tombstone).display().to_string(),
                ));
            }
            if first_detached_version.stable_content() != version.stable_content {
                return self.rollback_versioned_tombstone(leaf, &tombstone, &detached);
            }

            before_unlink(&tombstone);
            let second_detached_version = match detached.current_version() {
                Ok(version) => version,
                Err(_) => return self.rollback_versioned_tombstone(leaf, &tombstone, &detached),
            };
            if second_detached_version != first_detached_version
                || second_detached_version.stable_content() != version.stable_content
            {
                return self.rollback_versioned_tombstone(leaf, &tombstone, &detached);
            }
            if self.verify_file_binding(&tombstone, &detached).is_err() {
                return self.rollback_versioned_tombstone(leaf, &tombstone, &detached);
            }

            // There is no portable POSIX compare-and-unlink primitive. A
            // malicious writer can still race in the final interval after the
            // checks above. The held-parent tombstone protocol narrows that
            // interval and preserves every detected replacement.
            anchored_unlink(self, &tombstone, false)
                .map_err(|error| io_error(self.display_path.join(&tombstone), error))
        }

        #[cfg(not(any(unix, windows)))]
        {
            expected.verify_version(version)?;
            self.verify_file_binding(leaf, expected)?;
            expected.verify_version(version)?;
            before_detach();
            let _ = (after_detach, before_unlink);
            anchored_unlink(self, leaf, false)
                .map_err(|error| io_error(self.display_path.join(leaf), error))
        }
    }

    #[cfg(unix)]
    fn rollback_versioned_tombstone(
        &self,
        original_leaf: &std::ffi::OsStr,
        tombstone: &std::ffi::OsStr,
        detached: &AnchoredFile,
    ) -> Result<()> {
        // Roll back only with no-replace semantics. If a concurrent writer
        // installed a new original leaf, keep both that leaf and the durable
        // tombstone instead of overwriting either one.
        let _ = anchored_rename_no_replace(self, tombstone, detached, self, original_leaf);
        self.sync_all()?;
        Err(CheckPoError::WorkingTreeChanged(
            self.display_path.join(original_leaf).display().to_string(),
        ))
    }

    fn unlink_file_if_bound_inner(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
        before_detach: impl FnOnce(),
        after_detach: impl FnOnce(&std::ffi::OsStr),
        before_unlink: impl FnOnce(&std::ffi::OsStr),
    ) -> Result<()> {
        self.verify_file_binding(leaf, expected)?;
        #[cfg(windows)]
        {
            let identity = expected.identity;
            before_detach();
            let _ = (after_detach, before_unlink);
            let display_path = self.display_path.join(leaf);
            let file = open_windows_relative_file_for_unversioned_removal(&self.directory, leaf)?;
            if FileIdentity::from_file(&display_path, &file)? != identity {
                return Err(CheckPoError::WorkingTreeChanged(
                    display_path.display().to_string(),
                ));
            }
            super::windows_durability::remove_open_handle(file, &self.directory, leaf)
                .map_err(|error| io_error(display_path, error))
        }
        #[cfg(unix)]
        {
            before_detach();
            let tombstone = self.detach_file_to_unique_tombstone(leaf, expected)?;
            after_detach(&tombstone);
            let detached = match self.open_file(&tombstone) {
                Ok(detached) => detached,
                Err(_) => {
                    self.sync_all()?;
                    return Err(CheckPoError::WorkingTreeChanged(
                        self.display_path.join(&tombstone).display().to_string(),
                    ));
                }
            };
            if detached.identity != expected.identity {
                // A concurrent replacement was detached. Preserve it under
                // the tombstone name for recovery/inspection; deleting it
                // would repeat the original TOCTOU bug.
                self.sync_all()?;
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.join(&tombstone).display().to_string(),
                ));
            }
            before_unlink(&tombstone);
            if let Err(error) = self.verify_file_binding(&tombstone, expected) {
                self.sync_all()?;
                return Err(error);
            }
            anchored_unlink(self, &tombstone, false)
                .map_err(|error| io_error(self.display_path.join(&tombstone), error))
        }
        #[cfg(not(any(unix, windows)))]
        {
            before_detach();
            self.verify_file_binding(leaf, expected)?;
            anchored_unlink(self, leaf, false)
                .map_err(|error| io_error(self.display_path.join(leaf), error))
        }
    }

    #[cfg(unix)]
    fn detach_file_to_unique_tombstone(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
    ) -> Result<std::ffi::OsString> {
        for _ in 0..16 {
            let tombstone = std::ffi::OsString::from(format!(
                ".checkpo-delete-{}.tomb",
                uuid::Uuid::new_v4().simple()
            ));
            match anchored_rename_no_replace(self, leaf, expected, self, &tombstone) {
                Ok(()) => return Ok(tombstone),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(self.display_path.join(leaf), error)),
            }
        }
        Err(CheckPoError::Unexpected(format!(
            "could not allocate a unique delete tombstone below {}",
            self.display_path.display()
        )))
    }

    pub(crate) fn unlink_dir(&self, leaf: &std::ffi::OsStr) -> Result<()> {
        let expected = self.open_directory_for_mutation(leaf)?;
        self.unlink_dir_if_bound(leaf, expected)
    }

    pub(crate) fn unlink_dir_if_bound(
        &self,
        leaf: &std::ffi::OsStr,
        expected: AnchoredParent,
    ) -> Result<()> {
        #[cfg(windows)]
        {
            validate_leaf(leaf, &self.display_path)?;
            let current = self.open_directory_for_mutation(leaf)?;
            if !current.same_directory(&expected) {
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.join(leaf).display().to_string(),
                ));
            }
            drop(current);
            let display_path = self.display_path.join(leaf);
            let deletion_handle =
                reopen_windows_directory_for_removal(&expected.directory, &display_path)?;
            drop(expected);
            super::windows_durability::remove_open_directory_handle(
                deletion_handle,
                &self.directory,
                leaf,
            )
            .map_err(|error| io_error(display_path, error))
        }
        #[cfg(not(windows))]
        self.unlink_dir_if_bound_inner(leaf, &expected, || {}, |_| {}, |_| {})
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn unlink_dir_if_bound_inner(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredParent,
        before_detach: impl FnOnce(),
        after_detach: impl FnOnce(&std::ffi::OsStr),
        before_unlink: impl FnOnce(&std::ffi::OsStr),
    ) -> Result<()> {
        validate_leaf(leaf, &self.display_path)?;
        let current = self.open_directory_for_mutation(leaf)?;
        if !current.same_directory(expected) {
            return Err(CheckPoError::WorkingTreeChanged(
                self.display_path.join(leaf).display().to_string(),
            ));
        }
        #[cfg(windows)]
        {
            let display_path = self.display_path.join(leaf);
            drop(current);
            before_detach();
            let _ = (after_detach, before_unlink);
            super::windows_durability::remove_open_directory_handle(
                expected
                    .directory
                    .try_clone()
                    .map_err(|error| io_error(&display_path, error))?,
                &self.directory,
                leaf,
            )
            .map_err(|error| io_error(display_path, error))
        }
        #[cfg(unix)]
        {
            drop(current);
            before_detach();
            let tombstone = self.detach_directory_to_unique_tombstone(leaf, expected)?;
            after_detach(&tombstone);
            let detached = match self.open_directory_for_mutation(&tombstone) {
                Ok(detached) => detached,
                Err(_) => {
                    self.sync_all()?;
                    return Err(CheckPoError::WorkingTreeChanged(
                        self.display_path.join(&tombstone).display().to_string(),
                    ));
                }
            };
            if !detached.same_directory(expected) {
                self.sync_all()?;
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.join(&tombstone).display().to_string(),
                ));
            }
            before_unlink(&tombstone);
            let current = self.open_directory_for_mutation(&tombstone)?;
            if !current.same_directory(expected) {
                self.sync_all()?;
                return Err(CheckPoError::WorkingTreeChanged(
                    self.display_path.join(&tombstone).display().to_string(),
                ));
            }
            match anchored_unlink(self, &tombstone, true) {
                Ok(()) => Ok(()),
                Err(unlink_error) => {
                    // `rmdir` commonly fails because the shard is not empty.
                    // Detaching the directory is still required to bind the
                    // removal to `expected`, but a failed removal must not
                    // leave the live shard hidden under a tombstone name.
                    // Restore only with a no-replace rename; if another entry
                    // appeared at `leaf`, preserve both entries and report a
                    // concurrent tree change rather than overwriting it.
                    match self.rename_directory_no_replace_to(&tombstone, expected, self, leaf) {
                        Ok(()) => {
                            self.sync_all()?;
                            Err(io_error(self.display_path.join(&tombstone), unlink_error))
                        }
                        Err(_) => {
                            self.sync_all()?;
                            Err(CheckPoError::WorkingTreeChanged(
                                self.display_path.join(leaf).display().to_string(),
                            ))
                        }
                    }
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            before_detach();
            let result = anchored_unlink(self, leaf, true)
                .map_err(|error| io_error(self.display_path.join(leaf), error));
            drop(current);
            result
        }
    }

    #[cfg(unix)]
    fn detach_directory_to_unique_tombstone(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredParent,
    ) -> Result<std::ffi::OsString> {
        for _ in 0..16 {
            let tombstone = std::ffi::OsString::from(format!(
                ".checkpo-delete-dir-{}.tomb",
                uuid::Uuid::new_v4().simple()
            ));
            match anchored_rename_directory_no_replace(self, leaf, expected, self, &tombstone) {
                Ok(()) => return Ok(tombstone),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(self.display_path.join(leaf), error)),
            }
        }
        Err(CheckPoError::Unexpected(format!(
            "could not allocate a unique directory delete tombstone below {}",
            self.display_path.display()
        )))
    }

    pub(crate) fn remove_tree_contents(&self) -> Result<()> {
        for (leaf, is_directory, is_link) in list_anchored_directory_entries(self)? {
            if is_link {
                return Err(CheckPoError::Corruption(format!(
                    "transaction payload contains a symlink: {}",
                    self.display_path.join(&leaf).display()
                )));
            }
            if is_directory {
                let directory = self.open_directory_for_mutation(&leaf)?;
                directory.remove_tree_contents()?;
                drop(directory);
                self.unlink_dir(&leaf)?;
            } else {
                let file = self.open_file(&leaf)?;
                self.unlink_file_if_bound(&leaf, file)?;
            }
        }
        self.sync_all()
    }

    /// Enumerates names through the held directory handle rather than resolving
    /// `display_path` again. Callers must still reopen and bind-check an entry
    /// before mutating it.
    pub(crate) fn list_entry_names(&self) -> Result<Vec<std::ffi::OsString>> {
        let mut leaves = list_anchored_directory_entries(self)?
            .into_iter()
            .map(|(leaf, _, _)| leaf)
            .collect::<Vec<_>>();
        leaves.sort();
        Ok(leaves)
    }

    #[cfg(all(test, unix))]
    fn rename_no_replace_to_with_hook(
        &self,
        source_leaf: &std::ffi::OsStr,
        expected_source: &AnchoredFile,
        destination_parent: &AnchoredParent,
        destination_leaf: &std::ffi::OsStr,
        hook: impl FnOnce(),
    ) -> Result<()> {
        self.rename_no_replace_to_inner(
            source_leaf,
            expected_source,
            (destination_parent, destination_leaf),
            hook,
            || {},
            || {},
        )
    }

    #[cfg(all(test, unix))]
    fn rename_no_replace_to_with_hooks(
        &self,
        source_leaf: &std::ffi::OsStr,
        expected_source: &AnchoredFile,
        destination_parent: &AnchoredParent,
        destination_leaf: &std::ffi::OsStr,
        after_verify: impl FnOnce(),
        after_publish: impl FnOnce(),
    ) -> Result<()> {
        self.rename_no_replace_to_inner(
            source_leaf,
            expected_source,
            (destination_parent, destination_leaf),
            || {},
            after_verify,
            after_publish,
        )
    }

    #[cfg(all(test, unix))]
    fn unlink_file_if_bound_with_hooks(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
        before_detach: impl FnOnce(),
        after_detach: impl FnOnce(&std::ffi::OsStr),
        before_unlink: impl FnOnce(&std::ffi::OsStr),
    ) -> Result<()> {
        self.unlink_file_if_bound_inner(leaf, expected, before_detach, after_detach, before_unlink)
    }

    #[cfg(all(test, unix))]
    fn unlink_file_if_bound_versioned_with_hooks(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredFile,
        version: &AnchoredFileVersion,
        before_detach: impl FnOnce(),
        after_detach: impl FnOnce(&std::ffi::OsStr),
        before_unlink: impl FnOnce(&std::ffi::OsStr),
    ) -> Result<()> {
        self.unlink_file_if_bound_versioned_inner(
            leaf,
            expected,
            version,
            before_detach,
            after_detach,
            before_unlink,
        )
    }

    #[cfg(all(test, unix))]
    fn unlink_dir_if_bound_with_hooks(
        &self,
        leaf: &std::ffi::OsStr,
        expected: &AnchoredParent,
        before_detach: impl FnOnce(),
        after_detach: impl FnOnce(&std::ffi::OsStr),
        before_unlink: impl FnOnce(&std::ffi::OsStr),
    ) -> Result<()> {
        self.unlink_dir_if_bound_inner(leaf, expected, before_detach, after_detach, before_unlink)
    }
}

#[cfg(unix)]
fn list_anchored_directory_entries(
    parent: &AnchoredParent,
) -> Result<Vec<(std::ffi::OsString, bool, bool)>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    let duplicate = unsafe { libc::dup(parent.directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io_error(
            &parent.display_path,
            std::io::Error::last_os_error(),
        ));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(duplicate) };
        return Err(io_error(&parent.display_path, error));
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        names.push(std::ffi::OsString::from_vec(name.to_vec()));
    }
    unsafe { libc::closedir(stream) };

    let mut entries = Vec::with_capacity(names.len());
    for leaf in names {
        use std::mem::MaybeUninit;
        use std::os::unix::ffi::OsStrExt;
        let value = std::ffi::CString::new(leaf.as_bytes()).map_err(|_| {
            CheckPoError::Corruption(format!(
                "path contains NUL: {}",
                parent.display_path.join(&leaf).display()
            ))
        })?;
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                parent.directory.as_raw_fd(),
                value.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(io_error(
                parent.display_path.join(&leaf),
                std::io::Error::last_os_error(),
            ));
        }
        let stat = unsafe { stat.assume_init() };
        let kind = stat.st_mode & libc::S_IFMT;
        let is_directory = kind == libc::S_IFDIR;
        let is_link = kind == libc::S_IFLNK;
        if !is_directory && !is_link && kind != libc::S_IFREG {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a non-regular file: {}",
                parent.display_path.join(&leaf).display()
            )));
        }
        entries.push((leaf, is_directory, is_link));
    }
    Ok(entries)
}

#[cfg(windows)]
fn list_anchored_directory_entries(
    parent: &AnchoredParent,
) -> Result<Vec<(std::ffi::OsString, bool, bool)>> {
    super::windows_durability::list_directory_entries(&parent.directory)
        .map_err(|error| io_error(&parent.display_path, error))
}

#[cfg(not(any(unix, windows)))]
fn list_anchored_directory_entries(
    parent: &AnchoredParent,
) -> Result<Vec<(std::ffi::OsString, bool, bool)>> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&parent.display_path).map_err(|error| io_error(&parent.display_path, error))?
    {
        let entry = entry.map_err(|error| io_error(&parent.display_path, error))?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| io_error(entry.path(), error))?;
        entries.push((
            entry.file_name(),
            metadata.is_dir(),
            crate::metadata_is_link_or_reparse(&metadata),
        ));
    }
    Ok(entries)
}

fn anchored_file_from_open_file(display_path: PathBuf, file: File) -> Result<AnchoredFile> {
    let metadata = file
        .metadata()
        .map_err(|error| io_error(&display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(format!(
            "anchored path is not a regular file: {}",
            display_path.display()
        )));
    }
    #[cfg(windows)]
    let identity = FileIdentity::from_file(&display_path, &file)?;
    #[cfg(not(windows))]
    let identity = FileIdentity::from_metadata(&metadata)?;
    Ok(AnchoredFile {
        display_path,
        file,
        identity,
    })
}

impl FileIdentity {
    fn is_definitely_on_different_volume(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device != other.device
        }

        #[cfg(windows)]
        {
            self.volume_serial_number != 0
                && other.volume_serial_number != 0
                && self.volume_serial_number != other.volume_serial_number
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = other;
            false
        }
    }

    #[cfg(not(windows))]
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {
                length: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    #[cfg(windows)]
    fn from_file(path: &Path, file: &File) -> Result<Self> {
        use std::mem::MaybeUninit;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
        };

        let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if result == 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        let info = unsafe { info.assume_init() };
        Ok(Self {
            volume_serial_number: info.VolumeSerialNumber,
            file_id: info.FileId.Identifier,
        })
    }
}

#[cfg(windows)]
fn windows_file_basic_info(
    file: &File,
    path: &Path,
) -> Result<windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
    };

    let mut info = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    Ok(unsafe { info.assume_init() })
}

impl FileVersion {
    fn from_file_metadata(file: &File, path: &Path, metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        {
            let _ = (file, path);
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                identity: FileIdentity::from_metadata(metadata)?,
                length: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
            })
        }

        #[cfg(windows)]
        {
            let basic = windows_file_basic_info(file, path)?;
            Ok(Self {
                identity: FileIdentity::from_file(path, file)?,
                length: metadata.len(),
                modified: metadata.modified().ok(),
                changed: basic.ChangeTime,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (file, path);
            Ok(Self {
                identity: FileIdentity::from_metadata(metadata)?,
                length: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    #[cfg(unix)]
    fn stable_content(self) -> StableFileVersion {
        StableFileVersion {
            identity: self.identity,
            length: self.length,
            modified_seconds: self.modified_seconds,
            modified_nanoseconds: self.modified_nanoseconds,
        }
    }
}

impl AnchoredFileVersion {
    fn from_full(full: FileVersion) -> Self {
        Self {
            full,
            #[cfg(unix)]
            stable_content: full.stable_content(),
        }
    }
}

fn validated_relative_components(path: &Path) -> Result<Vec<&std::ffi::OsStr>> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CheckPoError::Corruption(format!(
            "anchored path must be a non-empty relative path: {}",
            path.display()
        )));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe anchored path component: {}",
                    path.display()
                )))
            }
        }
    }
    if components.is_empty() {
        return Err(CheckPoError::Corruption(format!(
            "anchored path has no components: {}",
            path.display()
        )));
    }
    Ok(components)
}

#[cfg(unix)]
fn open_unix_path(parent_fd: libc::c_int, path: &Path, flags: libc::c_int) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let value = std::ffi::CString::new(bytes)
        .map_err(|_| CheckPoError::Corruption(format!("path contains NUL: {}", path.display())))?;
    open_unix_cstring(parent_fd, &value, flags).map_err(|error| io_error(path, error))
}

#[cfg(unix)]
fn open_unix_component(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    flags: libc::c_int,
    display_path: &Path,
) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let value = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
        CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
    })?;
    open_unix_cstring(parent_fd, &value, flags).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            CheckPoError::Corruption(format!(
                "anchored path is not a no-follow regular file: {}",
                display_path.display()
            ))
        } else {
            io_error(display_path, error)
        }
    })
}

#[cfg(unix)]
fn open_unix_cstring(
    parent_fd: libc::c_int,
    value: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<File> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::openat(parent_fd, value.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn create_unix_directory_component(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let value = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
        CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
    })?;
    let result = unsafe { libc::mkdirat(parent_fd, value.as_ptr(), 0o777) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return Ok(false);
    }
    Err(io_error(display_path, error))
}

#[cfg(unix)]
fn create_unix_directory_component_exclusive(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let value = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
        CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
    })?;
    let result = unsafe { libc::mkdirat(parent_fd, value.as_ptr(), 0o777) };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(display_path, std::io::Error::last_os_error()))
    }
}

#[cfg(unix)]
fn unix_stat_mtime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_mtime, stat.st_mtime_nsec)
}

#[cfg(unix)]
fn unix_stat_ctime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_ctime, stat.st_ctime_nsec)
}

#[cfg(unix)]
fn unix_system_time(seconds: i64, nanoseconds: i64, path: &Path) -> Result<std::time::SystemTime> {
    if !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(CheckPoError::Corruption(format!(
            "file has an invalid timestamp: {}",
            path.display()
        )));
    }
    let nanoseconds = nanoseconds as u32;
    let value = if seconds >= 0 {
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(seconds as u64, nanoseconds))
    } else if nanoseconds == 0 {
        std::time::UNIX_EPOCH.checked_sub(std::time::Duration::new(seconds.unsigned_abs(), 0))
    } else {
        // POSIX represents -0.5s as tv_sec=-1,tv_nsec=500_000_000.
        std::time::UNIX_EPOCH.checked_sub(std::time::Duration::new(
            seconds.unsigned_abs() - 1,
            1_000_000_000 - nanoseconds,
        ))
    };
    value.ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "file timestamp is out of range: {}",
            path.display()
        ))
    })
}

fn validate_leaf(leaf: &std::ffi::OsStr, parent: &Path) -> Result<()> {
    let path = Path::new(leaf);
    if leaf.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(CheckPoError::Corruption(format!(
            "unsafe anchored leaf below {}: {}",
            parent.display(),
            path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn anchored_exchange_files(
    left_parent: &AnchoredParent,
    left_leaf: &std::ffi::OsStr,
    right_parent: &AnchoredParent,
    right_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let left = std::ffi::CString::new(left_leaf.as_bytes())?;
    let right = std::ffi::CString::new(right_leaf.as_bytes())?;
    let result = unsafe {
        libc::renameatx_np(
            left_parent.directory.as_raw_fd(),
            left.as_ptr(),
            right_parent.directory.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn anchored_exchange_files(
    left_parent: &AnchoredParent,
    left_leaf: &std::ffi::OsStr,
    right_parent: &AnchoredParent,
    right_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let left = std::ffi::CString::new(left_leaf.as_bytes())?;
    let right = std::ffi::CString::new(right_leaf.as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_parent.directory.as_raw_fd(),
            left.as_ptr(),
            right_parent.directory.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn anchored_rename_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredFile,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::renameatx_np(
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn anchored_rename_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredFile,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
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
fn anchored_rename_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    expected_source: &AnchoredFile,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    let source = open_windows_relative_file_for_mutation(&source_parent.directory, source_leaf)
        .map_err(checkpo_error_into_io)?;
    let display_path = source_parent.display_path.join(source_leaf);
    if FileIdentity::from_file(&display_path, &source).map_err(checkpo_error_into_io)?
        != expected_source.identity
    {
        return Err(std::io::Error::other("rename source identity changed"));
    }
    super::windows_durability::rename_open_handle_no_replace(
        &source,
        &destination_parent.directory,
        destination_leaf,
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn anchored_rename_no_replace(
    _source_parent: &AnchoredParent,
    _source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredFile,
    _destination_parent: &AnchoredParent,
    _destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive handle-relative rename is unavailable on this platform",
    ))
}

#[cfg(target_os = "macos")]
fn anchored_rename_directory_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredParent,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::renameatx_np(
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn anchored_rename_directory_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredParent,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
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

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn anchored_rename_directory_no_replace(
    _source_parent: &AnchoredParent,
    _source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredParent,
    _destination_parent: &AnchoredParent,
    _destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive handle-relative directory rename is unavailable on this platform",
    ))
}

#[cfg(unix)]
fn anchored_unlink(
    parent: &AnchoredParent,
    leaf: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let leaf = std::ffi::CString::new(leaf.as_bytes())?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    let result = unsafe { libc::unlinkat(parent.directory.as_raw_fd(), leaf.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
fn anchored_unlink(
    parent: &AnchoredParent,
    leaf: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<()> {
    let path = parent.display_path.join(leaf);
    if directory {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(windows)]
fn open_windows_directory_no_follow(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| io_error(path, error))
}

#[cfg(windows)]
fn windows_replace_record_leaf(destination_leaf: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStrExt;

    let mut hasher = blake3::Hasher::new();
    for unit in destination_leaf.encode_wide() {
        hasher.update(&unit.to_le_bytes());
    }
    std::ffi::OsString::from(format!(
        ".checkpo-replace-{}.json",
        hasher.finalize().to_hex()
    ))
}

#[cfg(windows)]
fn open_windows_relative_directory(
    parent: &File,
    component: &std::ffi::OsStr,
    create_new: bool,
    writable: bool,
) -> Result<File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    let desired_access = GENERIC_READ
        | FILE_READ_ATTRIBUTES
        | SYNCHRONIZE
        | if create_new || writable {
            GENERIC_WRITE
        } else {
            0
        };
    open_windows_relative(
        parent,
        component,
        desired_access,
        true,
        create_new,
        create_new || writable,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(component), error))
}

#[cfg(windows)]
fn reopen_windows_directory_for_mutation(directory: &File, display_path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    // ReOpenFile is not a reliable way to upgrade a directory handle on
    // Windows. Reacquire a write-through twin by path, but bind it to the
    // already-held read anchor before allowing any mutation. Intermediate
    // path replacement can therefore only produce an identity mismatch, not
    // redirect a later handle-relative operation.
    let reopened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
        )
        .open(display_path)
        .map_err(|error| io_error(display_path, error))?;
    let metadata = reopened
        .metadata()
        .map_err(|error| io_error(display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "mutation anchor is not a regular directory: {}",
            display_path.display()
        )));
    }
    if FileIdentity::from_file(display_path, directory)?
        != FileIdentity::from_file(display_path, &reopened)?
    {
        return Err(CheckPoError::WorkingTreeChanged(
            display_path.display().to_string(),
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
fn reopen_windows_directory_for_removal(directory: &File, display_path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    };

    let reopened = fs::OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
        )
        .open(display_path)
        .map_err(|error| io_error(display_path, error))?;
    let metadata = reopened
        .metadata()
        .map_err(|error| io_error(display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "removal anchor is not a regular directory: {}",
            display_path.display()
        )));
    }
    if FileIdentity::from_file(display_path, directory)?
        != FileIdentity::from_file(display_path, &reopened)?
    {
        return Err(CheckPoError::WorkingTreeChanged(
            display_path.display().to_string(),
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
fn reopen_windows_file_for_durability(file: &File, display_path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let reopened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH)
        .open(display_path)
        .map_err(|error| io_error(display_path, error))?;
    let metadata = reopened
        .metadata()
        .map_err(|error| io_error(display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(format!(
            "durability target is not a regular file: {}",
            display_path.display()
        )));
    }
    if FileIdentity::from_file(display_path, file)?
        != FileIdentity::from_file(display_path, &reopened)?
    {
        return Err(CheckPoError::WorkingTreeChanged(
            display_path.display().to_string(),
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
fn open_windows_relative_file(
    parent: &File,
    leaf: &std::ffi::OsStr,
    read_write: bool,
    create_new: bool,
) -> Result<File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    let desired_access = GENERIC_READ
        | FILE_READ_ATTRIBUTES
        | SYNCHRONIZE
        | if read_write { GENERIC_WRITE } else { 0 };
    open_windows_relative(
        parent,
        leaf,
        desired_access,
        false,
        create_new,
        create_new || read_write,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
fn open_windows_relative_file_for_mutation(parent: &File, leaf: &std::ffi::OsStr) -> Result<File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        true,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
fn open_windows_relative_file_for_removal(parent: &File, leaf: &std::ffi::OsStr) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        false,
        false,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
fn open_windows_relative_file_for_unversioned_removal(
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        false,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
fn open_windows_relative_file_for_finalization(
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        false,
        true,
        false,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
fn checkpo_error_into_io(error: CheckPoError) -> std::io::Error {
    match error {
        CheckPoError::Io { source, .. } => source,
        error => std::io::Error::other(error.to_string()),
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn open_windows_relative(
    parent: &File,
    component: &std::ffi::OsStr,
    desired_access: u32,
    directory: bool,
    create_new: bool,
    write_through: bool,
    share_write: bool,
    share_delete: bool,
) -> std::io::Result<File> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH,
    };
    use windows_sys::Win32::Foundation::{
        RtlNtStatusToDosError, HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut wide = component.encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Windows relative component",
        ));
    }
    let byte_len = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows relative component is too long",
            )
        })?;
    let unicode = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = INVALID_HANDLE_VALUE;
    let mut io_status = MaybeUninit::<IO_STATUS_BLOCK>::zeroed();
    let create_options = FILE_OPEN_REPARSE_POINT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | if write_through { FILE_WRITE_THROUGH } else { 0 }
        | if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            io_status.as_mut_ptr(),
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ
                | if share_write { FILE_SHARE_WRITE } else { 0 }
                | if share_delete { FILE_SHARE_DELETE } else { 0 },
            if create_new { FILE_CREATE } else { FILE_OPEN },
            create_options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(unsafe {
            RtlNtStatusToDosError(status) as i32
        }));
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(not(any(unix, windows)))]
fn open_portable_directory_no_follow(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
fn open_new_portable_file_no_follow(path: &Path) -> Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
fn open_existing_portable_file_no_follow(path: &Path) -> Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
fn open_read_only_portable_file_no_follow(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
fn open_portable_file_no_follow(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| io_error(path, error))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn volume_identity_treats_only_different_devices_as_definitive() {
        let source = FileIdentity {
            device: 7,
            inode: 11,
        };
        let same_volume = FileIdentity {
            device: 7,
            inode: 12,
        };
        let different_volume = FileIdentity {
            device: 8,
            inode: 11,
        };

        assert!(!source.is_definitely_on_different_volume(&same_volume));
        assert!(source.is_definitely_on_different_volume(&different_volume));
    }

    #[test]
    fn anchored_hash_polls_for_cancellation_after_eof() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("empty"), b"").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let mut file = root.open_file(Path::new("empty")).unwrap();
        let mut polls = 0_usize;

        let error = match file.hash_with_poll(|| {
            polls += 1;
            if polls == 3 {
                Err(CheckPoError::Cancelled)
            } else {
                Ok(())
            }
        }) {
            Ok(_) => panic!("cancellation after EOF was ignored"),
            Err(error) => error,
        };

        assert!(matches!(error, CheckPoError::Cancelled));
        assert_eq!(polls, 3);
    }

    fn only_entry_with_prefix(directory: &Path, prefix: &str) -> PathBuf {
        let matches = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "expected one {prefix} tombstone");
        matches.into_iter().next().unwrap()
    }

    #[test]
    fn rejects_intermediate_and_leaf_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("payload"), b"outside").unwrap();
        symlink(&outside, root.join("linked-dir")).unwrap();
        symlink(outside.join("payload"), root.join("linked-file")).unwrap();

        let anchored = AnchoredRoot::open(&root).unwrap();
        assert!(anchored.open_file(Path::new("linked-dir/payload")).is_err());
        assert!(anchored.open_file(Path::new("linked-file")).is_err());
    }

    #[test]
    fn root_path_replacement_cannot_redirect_openat() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let original = temp.path().join("original-root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("payload"), b"approved").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();

        fs::rename(&root, &original).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("payload"), b"attacker").unwrap();

        let mut file = anchored.open_file(Path::new("payload")).unwrap();
        let hash = file.hash().unwrap().object_id;
        assert_eq!(hash, crate::hash_bytes(b"approved"));
        assert!(anchored.verify_binding(Path::new("payload"), &file).is_ok());
        assert!(matches!(
            anchored.verify_root_binding(),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn intermediate_path_swap_after_handle_open_cannot_redirect_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(outside.join("b")).unwrap();
        fs::write(root.join("a/b/payload"), b"approved").unwrap();
        fs::write(outside.join("b/payload"), b"attacker").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();

        let mut swapped = false;
        let mut file = anchored
            .open_file_with_component_hook(Path::new("a/b/payload"), |index, _| {
                if index == 0 && !swapped {
                    fs::rename(root.join("a"), root.join("a-original")).unwrap();
                    symlink(&outside, root.join("a")).unwrap();
                    swapped = true;
                }
            })
            .unwrap();
        assert_eq!(
            file.hash().unwrap().object_id,
            crate::hash_bytes(b"approved")
        );
    }

    #[test]
    fn opened_file_survives_leaf_swap_and_binding_check_detects_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("payload"), b"approved").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        let mut file = anchored.open_file(Path::new("payload")).unwrap();

        fs::rename(root.join("payload"), root.join("payload-original")).unwrap();
        fs::write(root.join("payload"), b"attacker").unwrap();

        assert_eq!(
            file.hash().unwrap().object_id,
            crate::hash_bytes(b"approved")
        );
        assert!(matches!(
            anchored.verify_binding(Path::new("payload"), &file),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn rejects_parent_and_absolute_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        assert!(anchored.open_file(Path::new("../outside")).is_err());
        assert!(anchored.open_file(Path::new("/outside")).is_err());
    }

    #[test]
    fn held_destination_parent_prevents_symlink_swap_redirect() {
        let temp = tempfile::tempdir().unwrap();
        let source_root_path = temp.path().join("source");
        let destination_root_path = temp.path().join("project");
        let outside = temp.path().join("outside");
        fs::create_dir_all(source_root_path.join("staged")).unwrap();
        fs::create_dir_all(destination_root_path.join("Assets")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(source_root_path.join("staged/file.asset"), "approved").unwrap();
        let source_root = AnchoredRoot::open(&source_root_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_root_path).unwrap();
        let expected = source_root
            .open_file(Path::new("staged/file.asset"))
            .unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent(Path::new("staged/file.asset"), false)
            .unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent(Path::new("Assets/file.asset"), false)
            .unwrap();

        source_parent
            .rename_no_replace_to_with_hook(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {
                    fs::rename(
                        destination_root_path.join("Assets"),
                        destination_root_path.join("Assets-original"),
                    )
                    .unwrap();
                    symlink(&outside, destination_root_path.join("Assets")).unwrap();
                },
            )
            .unwrap();

        assert!(!outside.join("file.asset").exists());
        assert_eq!(
            fs::read_to_string(destination_root_path.join("Assets-original/file.asset")).unwrap(),
            "approved"
        );
        assert!(matches!(
            destination_root.verify_parent_binding(Path::new("Assets"), &destination_parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn source_leaf_swap_is_rejected_before_rename() {
        let temp = tempfile::tempdir().unwrap();
        let source_root_path = temp.path().join("source");
        let destination_root_path = temp.path().join("project");
        fs::create_dir_all(source_root_path.join("staged")).unwrap();
        fs::create_dir_all(destination_root_path.join("Assets")).unwrap();
        fs::write(source_root_path.join("staged/file.asset"), "approved").unwrap();
        let source_root = AnchoredRoot::open(&source_root_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_root_path).unwrap();
        let expected = source_root
            .open_file(Path::new("staged/file.asset"))
            .unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent(Path::new("staged/file.asset"), false)
            .unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent(Path::new("Assets/file.asset"), false)
            .unwrap();

        let error = source_parent
            .rename_no_replace_to_with_hook(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {
                    fs::rename(
                        source_root_path.join("staged/file.asset"),
                        source_root_path.join("staged/file-original.asset"),
                    )
                    .unwrap();
                    fs::write(source_root_path.join("staged/file.asset"), "attacker").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert!(!destination_root_path.join("Assets/file.asset").exists());
        assert_eq!(
            fs::read_to_string(source_root_path.join("staged/file-original.asset")).unwrap(),
            "approved"
        );
        assert_eq!(
            fs::read_to_string(source_root_path.join("staged/file.asset")).unwrap(),
            "attacker"
        );
    }

    #[test]
    fn rename_rollback_preserves_source_replacement_after_identity_check() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let destination_path = temp.path().join("destination");
        fs::create_dir(&source_path).unwrap();
        fs::create_dir(&destination_path).unwrap();
        fs::write(source_path.join("payload"), b"approved").unwrap();
        let source_root = AnchoredRoot::open(&source_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_path).unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = source_parent.open_file(&source_leaf).unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();

        let error = source_parent
            .rename_no_replace_to_with_hooks(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {
                    fs::rename(
                        source_path.join("payload"),
                        source_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(source_path.join("payload"), b"replacement").unwrap();
                },
                || {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(source_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(destination_path.join("payload")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn rename_rollback_preserves_destination_replacement_after_publish() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let destination_path = temp.path().join("destination");
        fs::create_dir(&source_path).unwrap();
        fs::create_dir(&destination_path).unwrap();
        fs::write(source_path.join("payload"), b"approved").unwrap();
        let source_root = AnchoredRoot::open(&source_path).unwrap();
        let destination_root = AnchoredRoot::open(&destination_path).unwrap();
        let (source_parent, source_leaf) = source_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = source_parent.open_file(&source_leaf).unwrap();
        let (destination_parent, destination_leaf) = destination_root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();

        let error = source_parent
            .rename_no_replace_to_with_hooks(
                &source_leaf,
                &expected,
                &destination_parent,
                &destination_leaf,
                || {},
                || {
                    fs::rename(
                        destination_path.join("payload"),
                        destination_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(destination_path.join("payload"), b"replacement").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(destination_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(destination_path.join("payload")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn bound_unlink_preserves_replacement_swapped_before_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        let error = parent
            .unlink_file_if_bound_with_hooks(
                &leaf,
                &expected,
                || {
                    fs::rename(
                        root_path.join("payload"),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join("payload"), b"replacement").unwrap();
                },
                |_| {},
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn bound_unlink_preserves_replacement_swapped_after_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        let error = parent
            .unlink_file_if_bound_with_hooks(
                &leaf,
                &expected,
                || {},
                |tombstone| {
                    fs::rename(
                        root_path.join(tombstone),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join(tombstone), b"replacement").unwrap();
                },
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn bound_unlink_rechecks_tombstone_immediately_before_remove() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        let error = parent
            .unlink_file_if_bound_with_hooks(
                &leaf,
                &expected,
                || {},
                |_| {},
                |tombstone| {
                    fs::rename(
                        root_path.join(tombstone),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join(tombstone), b"replacement").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn versioned_unlink_rolls_back_a_same_inode_write_before_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let version = expected.hash().unwrap().version;

        let error = parent
            .unlink_file_if_bound_versioned_with_hooks(
                &leaf,
                &expected,
                &version,
                || fs::write(root_path.join("payload"), b"mutated!").unwrap(),
                |_| {},
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(fs::read(root_path.join("payload")).unwrap(), b"mutated!");
        assert!(fs::read_dir(&root_path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".checkpo-delete-")));
    }

    #[test]
    fn versioned_unlink_compares_post_detach_versions_before_remove() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let hashed = expected.hash().unwrap();
        let original_mtime = filetime::FileTime::from_last_modification_time(&hashed.metadata);
        let version = hashed.version;

        let error = parent
            .unlink_file_if_bound_versioned_with_hooks(
                &leaf,
                &expected,
                &version,
                || {},
                |_| {},
                |tombstone| {
                    let path = root_path.join(tombstone);
                    fs::write(&path, b"mutated!").unwrap();
                    filetime::set_file_mtime(&path, original_mtime).unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(fs::read(root_path.join("payload")).unwrap(), b"mutated!");
    }

    #[test]
    fn versioned_unlink_keeps_a_replacement_swapped_after_detach() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let version = expected.hash().unwrap().version;

        let error = parent
            .unlink_file_if_bound_versioned_with_hooks(
                &leaf,
                &expected,
                &version,
                || {},
                |tombstone| {
                    fs::rename(
                        root_path.join(tombstone),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join(tombstone), b"replacement").unwrap();
                },
                |_| {},
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(only_entry_with_prefix(&root_path, ".checkpo-delete-")).unwrap(),
            b"replacement"
        );
        assert!(!root_path.join("payload").exists());
    }

    #[test]
    fn bound_directory_unlink_preserves_replacement_at_each_boundary() {
        for boundary in 0..3 {
            let temp = tempfile::tempdir().unwrap();
            let root_path = temp.path().join("root");
            fs::create_dir_all(root_path.join("payload")).unwrap();
            let root = AnchoredRoot::open(&root_path).unwrap();
            let (parent, leaf) = root
                .open_parent_for_mutation(Path::new("payload"), false)
                .unwrap();
            let expected = parent.open_directory_for_mutation(&leaf).unwrap();

            let swap_source = || {
                fs::rename(
                    root_path.join("payload"),
                    root_path.join("approved-preserved"),
                )
                .unwrap();
                fs::create_dir(root_path.join("payload")).unwrap();
            };
            let swap_tombstone = |tombstone: &std::ffi::OsStr| {
                fs::rename(
                    root_path.join(tombstone),
                    root_path.join("approved-preserved"),
                )
                .unwrap();
                fs::create_dir(root_path.join(tombstone)).unwrap();
            };
            let error = match boundary {
                0 => parent.unlink_dir_if_bound_with_hooks(
                    &leaf,
                    &expected,
                    swap_source,
                    |_| {},
                    |_| {},
                ),
                1 => parent.unlink_dir_if_bound_with_hooks(
                    &leaf,
                    &expected,
                    || {},
                    swap_tombstone,
                    |_| {},
                ),
                _ => parent.unlink_dir_if_bound_with_hooks(
                    &leaf,
                    &expected,
                    || {},
                    |_| {},
                    swap_tombstone,
                ),
            }
            .unwrap_err();

            assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
            assert!(root_path.join("approved-preserved").is_dir());
            assert!(only_entry_with_prefix(&root_path, ".checkpo-delete-dir-").is_dir());
        }
    }

    #[test]
    fn create_new_file_uses_held_parent_after_path_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("staged")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, _) = root
            .open_parent(Path::new("staged/new.asset"), false)
            .unwrap();
        fs::rename(root_path.join("staged"), root_path.join("staged-original")).unwrap();
        symlink(&outside, root_path.join("staged")).unwrap();

        let mut file = parent
            .create_new_file(std::ffi::OsStr::new("new.asset"))
            .unwrap();
        file.write_all(b"approved").unwrap();
        file.sync_all().unwrap();

        assert!(!outside.join("new.asset").exists());
        assert_eq!(
            fs::read_to_string(root_path.join("staged-original/new.asset")).unwrap(),
            "approved"
        );
    }

    #[test]
    fn held_parent_atomic_write_replaces_value_and_cleans_private_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("refs")).unwrap();
        fs::write(root_path.join("refs/latest"), b"old").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("refs/latest"), false)
            .unwrap();

        parent.write_bytes_atomic(&leaf, b"new", false).unwrap();

        assert_eq!(fs::read(root_path.join("refs/latest")).unwrap(), b"new");
        assert_eq!(fs::read_dir(root_path.join("refs")).unwrap().count(), 1);
    }

    #[test]
    fn held_parent_atomic_create_does_not_replace_existing_value() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("records")).unwrap();
        fs::write(root_path.join("records/id"), b"existing").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        let error = root
            .write_bytes_atomic_new(Path::new("records/id"), b"replacement")
            .unwrap_err();

        assert!(
            matches!(error, CheckPoError::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(root_path.join("records/id")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(root_path.join("records")).unwrap().count(), 1);
    }

    #[test]
    fn held_parent_atomic_write_cannot_follow_parent_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("refs")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root_path.join("refs/latest"), b"old").unwrap();
        fs::write(outside.join("latest"), b"outside").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("refs/latest"), false)
            .unwrap();

        fs::rename(root_path.join("refs"), root_path.join("refs-original")).unwrap();
        symlink(&outside, root_path.join("refs")).unwrap();
        parent.write_bytes_atomic(&leaf, b"new", false).unwrap();

        assert_eq!(
            fs::read(root_path.join("refs-original/latest")).unwrap(),
            b"new"
        );
        assert_eq!(fs::read(outside.join("latest")).unwrap(), b"outside");
        assert!(matches!(
            root.verify_parent_binding(Path::new("refs"), &parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn held_parent_unlink_cannot_be_redirected_by_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("Assets/Nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            root_path.join("Assets/Nested/.checkpo-0123456789abcdef0123456789abcdef.tmp"),
            "approved",
        )
        .unwrap();
        fs::write(
            outside.join(".checkpo-0123456789abcdef0123456789abcdef.tmp"),
            "outside",
        )
        .unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let relative = Path::new("Assets/Nested/.checkpo-0123456789abcdef0123456789abcdef.tmp");
        let (parent, leaf) = root.open_parent_for_mutation(relative, false).unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        fs::rename(
            root_path.join("Assets/Nested"),
            root_path.join("Nested-original"),
        )
        .unwrap();
        symlink(&outside, root_path.join("Assets/Nested")).unwrap();

        parent.unlink_file_if_bound(&leaf, expected).unwrap();
        parent.sync_all().unwrap();

        assert!(!root_path
            .join("Nested-original/.checkpo-0123456789abcdef0123456789abcdef.tmp")
            .exists());
        assert_eq!(
            fs::read_to_string(outside.join(".checkpo-0123456789abcdef0123456789abcdef.tmp"))
                .unwrap(),
            "outside"
        );
        assert!(matches!(
            root.verify_parent_binding(Path::new("Assets/Nested"), &parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn held_parent_directory_unlink_cannot_be_redirected_by_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("journals/checkpoint-delete/tx")).unwrap();
        fs::create_dir_all(outside.join("tx")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let relative = Path::new("journals/checkpoint-delete/tx");
        let (parent, leaf) = root.open_parent_for_mutation(relative, false).unwrap();
        let expected = parent.open_directory_for_mutation(&leaf).unwrap();

        fs::rename(
            root_path.join("journals/checkpoint-delete"),
            root_path.join("journals/checkpoint-delete-original"),
        )
        .unwrap();
        symlink(&outside, root_path.join("journals/checkpoint-delete")).unwrap();

        parent.unlink_dir_if_bound(&leaf, expected).unwrap();
        parent.sync_all().unwrap();

        assert!(!root_path
            .join("journals/checkpoint-delete-original/tx")
            .exists());
        assert!(outside.join("tx").is_dir());
        assert!(matches!(
            root.verify_parent_binding(Path::new("journals/checkpoint-delete"), &parent),
            Err(CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn held_parent_directory_unlink_restores_non_empty_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("objects/loose/ab")).unwrap();
        fs::write(root_path.join("objects/loose/ab/object"), b"payload").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("objects/loose/ab"), false)
            .unwrap();
        let expected = parent.open_directory_for_mutation(&leaf).unwrap();

        let error = parent.unlink_dir_if_bound(&leaf, expected).unwrap_err();

        assert!(matches!(
            error,
            CheckPoError::Io { source, .. }
                if source.kind() == std::io::ErrorKind::DirectoryNotEmpty
        ));
        assert_eq!(
            fs::read(root_path.join("objects/loose/ab/object")).unwrap(),
            b"payload"
        );
        assert_eq!(
            fs::read_dir(root_path.join("objects/loose"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn parent_inspection_rejects_leaf_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(root_path.join("files")).unwrap();
        fs::write(&outside, "outside").unwrap();
        symlink(&outside, root_path.join("files/linked")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root.open_parent(Path::new("files/linked"), false).unwrap();

        assert!(parent.open_file(&leaf).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn volume_identity_treats_only_known_different_volumes_as_definitive() {
        let source = FileIdentity {
            volume_serial_number: 7,
            file_id: [1; 16],
        };
        let same_volume = FileIdentity {
            volume_serial_number: 7,
            file_id: [2; 16],
        };
        let different_volume = FileIdentity {
            volume_serial_number: 8,
            file_id: [1; 16],
        };
        let unknown_volume = FileIdentity {
            volume_serial_number: 0,
            file_id: [3; 16],
        };

        assert!(!source.is_definitely_on_different_volume(&same_volume));
        assert!(source.is_definitely_on_different_volume(&different_volume));
        assert!(!source.is_definitely_on_different_volume(&unknown_volume));
        assert!(!unknown_volume.is_definitely_on_different_volume(&source));
    }

    #[test]
    fn mutation_root_rebinds_to_the_held_identity_and_flushes() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        let rebound =
            reopen_windows_directory_for_mutation(&root.directory, &root.display_path).unwrap();

        assert_eq!(
            FileIdentity::from_file(&root_path, &rebound).unwrap(),
            root.identity
        );
        rebound.sync_all().unwrap();
    }

    #[test]
    fn mutation_root_rebind_rejects_a_path_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let original_path = temp.path().join("original-root");
        fs::create_dir(&root_path).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        fs::rename(&root_path, &original_path).unwrap();
        fs::create_dir(&root_path).unwrap();

        assert!(matches!(
            reopen_windows_directory_for_mutation(&root.directory, &root.display_path),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn same_size_same_mtime_leaf_replacement_changes_handle_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let payload = root.join("payload");
        fs::create_dir_all(&root).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let modified = fs::metadata(&payload).unwrap().modified().unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        let file = anchored.open_file(Path::new("payload")).unwrap();

        fs::rename(&payload, root.join("payload-original")).unwrap();
        fs::write(&payload, b"attacker").unwrap();
        filetime::set_file_mtime(&payload, filetime::FileTime::from_system_time(modified)).unwrap();

        assert!(matches!(
            anchored.verify_binding(Path::new("payload"), &file),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn same_file_same_size_mtime_restore_still_changes_the_version() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let payload = root_path.join("payload");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let modified = fs::metadata(&payload).unwrap().modified().unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let mut file = root.open_file(Path::new("payload")).unwrap();
        let hashed = file.hash().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&payload, b"attacker").unwrap();
        filetime::set_file_mtime(&payload, filetime::FileTime::from_system_time(modified)).unwrap();

        assert!(matches!(
            file.verify_version(&hashed.version),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
    }

    #[test]
    fn no_write_sharing_guard_blocks_in_place_writers() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("payload"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();
        let _guard = parent
            .open_file_without_write_sharing(&leaf, &expected)
            .unwrap();

        let error = fs::OpenOptions::new()
            .write(true)
            .open(root_path.join("payload"))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
    }

    #[test]
    fn versioned_delete_rejects_same_file_content_change() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let payload = root_path.join("payload");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let modified = fs::metadata(&payload).unwrap().modified().unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let mut expected = parent.open_file(&leaf).unwrap();
        let version = expected.hash().unwrap().version;

        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&payload, b"attacker").unwrap();
        filetime::set_file_mtime(&payload, filetime::FileTime::from_system_time(modified)).unwrap();

        assert!(matches!(
            parent.unlink_file_if_bound_versioned(&leaf, expected, version),
            Err(CheckPoError::WorkingTreeChanged(_))
        ));
        assert_eq!(fs::read(&payload).unwrap(), b"attacker");
    }

    #[test]
    fn identity_bound_delete_supports_read_only_files() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let payload = root_path.join("payload");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(&payload, b"approved").unwrap();
        let mut permissions = fs::metadata(&payload).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&payload, permissions).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        parent.unlink_file_if_bound(&leaf, expected).unwrap();
        assert!(!payload.exists());
    }

    #[test]
    fn identity_readback_matches_ntfs_case_insensitive_lookup() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("Payload.asset"), b"approved").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (parent, leaf) = root
            .open_parent_for_mutation(Path::new("payload.asset"), false)
            .unwrap();
        let expected = parent.open_file(&leaf).unwrap();

        parent.unlink_file_if_bound(&leaf, expected).unwrap();
        assert!(!root_path.join("Payload.asset").exists());
    }

    #[test]
    fn conditional_replace_preserves_a_destination_inserted_after_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root
            .open_directory_for_mutation(Path::new(""), false)
            .unwrap();
        let destination = parent
            .open_file(std::ffi::OsStr::new("destination"))
            .unwrap();
        let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();

        let error = parent
            .replace_from_temporary_with_hook(
                std::ffi::OsStr::new("temporary"),
                &temporary,
                std::ffi::OsStr::new("destination"),
                &destination,
                || {
                    fs::rename(
                        root_path.join("destination"),
                        root_path.join("approved-preserved"),
                    )
                    .unwrap();
                    fs::write(root_path.join("destination"), b"attacker").unwrap();
                },
            )
            .unwrap_err();

        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(
            fs::read(root_path.join("destination")).unwrap(),
            b"attacker"
        );
        assert_eq!(
            fs::read(root_path.join("approved-preserved")).unwrap(),
            b"approved"
        );
        assert_eq!(
            fs::read(root_path.join("temporary")).unwrap(),
            b"replacement"
        );
    }

    fn assert_no_windows_replace_artifacts(root: &Path) {
        let artifacts = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|leaf| leaf.to_string_lossy().starts_with(".checkpo-replace-"))
            .collect::<Vec<_>>();
        assert!(
            artifacts.is_empty(),
            "replace artifacts remain: {artifacts:?}"
        );
    }

    #[test]
    fn crash_after_windows_destination_detach_rolls_back_on_missing_open() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("Destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        {
            let root = AnchoredRoot::open(&root_path).unwrap();
            let parent = root
                .open_directory_for_mutation(Path::new(""), false)
                .unwrap();
            let destination = parent
                .open_file(std::ffi::OsStr::new("Destination"))
                .unwrap();
            let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();

            let error = parent
                .replace_from_temporary_stopping_at_windows_phase(
                    std::ffi::OsStr::new("temporary"),
                    &temporary,
                    std::ffi::OsStr::new("Destination"),
                    &destination,
                    ReplaceProtocolPhase::DestinationDetached,
                )
                .unwrap_err();
            assert!(matches!(error, CheckPoError::Unexpected(_)));
            assert!(!root_path.join("Destination").exists());
        }

        let reopened = AnchoredRoot::open(&root_path).unwrap();
        let mut restored = reopened.open_file(Path::new("destination")).unwrap();
        assert_eq!(restored.read_bounded(32).unwrap(), b"approved");
        assert!(!root_path.join("temporary").exists());
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn crash_after_windows_publish_is_completed_by_the_next_replace() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        {
            let root = AnchoredRoot::open(&root_path).unwrap();
            let parent = root
                .open_directory_for_mutation(Path::new(""), false)
                .unwrap();
            let destination = parent
                .open_file(std::ffi::OsStr::new("destination"))
                .unwrap();
            let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();

            let error = parent
                .replace_from_temporary_stopping_at_windows_phase(
                    std::ffi::OsStr::new("temporary"),
                    &temporary,
                    std::ffi::OsStr::new("destination"),
                    &destination,
                    ReplaceProtocolPhase::ReplacementPublished,
                )
                .unwrap_err();
            assert!(matches!(error, CheckPoError::Unexpected(_)));
            assert_eq!(
                fs::read(root_path.join("destination")).unwrap(),
                b"replacement"
            );
        }

        let reopened = AnchoredRoot::open(&root_path).unwrap();
        reopened
            .write_bytes_atomic(Path::new("destination"), b"next")
            .unwrap();
        assert_eq!(fs::read(root_path.join("destination")).unwrap(), b"next");
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn crash_after_windows_record_publication_is_rolled_back_before_next_replace() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        {
            let root = AnchoredRoot::open(&root_path).unwrap();
            let parent = root
                .open_directory_for_mutation(Path::new(""), false)
                .unwrap();
            let destination = parent
                .open_file(std::ffi::OsStr::new("destination"))
                .unwrap();
            let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();
            parent
                .replace_from_temporary_stopping_at_windows_phase(
                    std::ffi::OsStr::new("temporary"),
                    &temporary,
                    std::ffi::OsStr::new("destination"),
                    &destination,
                    ReplaceProtocolPhase::RecoveryRecordDurable,
                )
                .unwrap_err();
        }

        let reopened = AnchoredRoot::open(&root_path).unwrap();
        reopened
            .write_bytes_atomic(Path::new("destination"), b"next")
            .unwrap();
        assert_eq!(fs::read(root_path.join("destination")).unwrap(), b"next");
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn windows_finalization_guard_blocks_destination_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(&root_path).unwrap();
        fs::write(root_path.join("destination"), b"approved").unwrap();
        fs::write(root_path.join("temporary"), b"replacement").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let parent = root
            .open_directory_for_mutation(Path::new(""), false)
            .unwrap();
        let destination = parent
            .open_file(std::ffi::OsStr::new("destination"))
            .unwrap();
        let temporary = parent.open_file(std::ffi::OsStr::new("temporary")).unwrap();
        let mut replacement_attempted = false;

        parent
            .replace_from_temporary_with_windows_phase_hook(
                std::ffi::OsStr::new("temporary"),
                &temporary,
                std::ffi::OsStr::new("destination"),
                &destination,
                |phase| {
                    if phase == ReplaceProtocolPhase::ReplacementPublished {
                        replacement_attempted = true;
                        let error =
                            fs::rename(root_path.join("destination"), root_path.join("stolen"))
                                .unwrap_err();
                        assert!(matches!(error.raw_os_error(), Some(5) | Some(32)));
                    }
                    Ok(())
                },
            )
            .unwrap();

        assert!(replacement_attempted);
        assert_eq!(
            fs::read(root_path.join("destination")).unwrap(),
            b"replacement"
        );
        assert!(!root_path.join("stolen").exists());
        assert_no_windows_replace_artifacts(&root_path);
    }

    #[test]
    fn held_parent_atomic_write_supports_root_and_nested_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("inventory/snapshots")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();

        root.write_bytes_atomic(Path::new("root-head"), b"root")
            .unwrap();
        root.write_bytes_atomic(Path::new("inventory/snapshots/head"), b"nested")
            .unwrap();
        root.write_bytes_atomic(Path::new("root-head"), b"root-replaced")
            .unwrap();
        root.write_bytes_atomic(Path::new("inventory/snapshots/head"), b"nested-replaced")
            .unwrap();

        assert_eq!(
            fs::read(root_path.join("root-head")).unwrap(),
            b"root-replaced"
        );
        assert_eq!(
            fs::read(root_path.join("inventory/snapshots/head")).unwrap(),
            b"nested-replaced"
        );
    }

    #[test]
    fn read_only_anchor_can_be_flushed_without_losing_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("object"), b"payload").unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let object = root.open_file(Path::new("object")).unwrap();

        object.sync_all().unwrap();

        root.verify_binding(Path::new("object"), &object).unwrap();
    }

    #[test]
    fn held_empty_directory_can_be_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir_all(root_path.join("objects/loose/aa")).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let (loose, shard) = root
            .open_parent_for_mutation(Path::new("objects/loose/aa"), false)
            .unwrap();

        loose.unlink_dir(&shard).unwrap();
        loose.sync_all().unwrap();

        assert!(!root_path.join("objects/loose/aa").exists());
    }

    #[test]
    fn concurrent_missing_parent_creation_reopens_the_single_winner() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        fs::create_dir(&root_path).unwrap();
        let root = AnchoredRoot::open(&root_path).unwrap();
        let barrier = Arc::new(Barrier::new(8));

        let identities = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|index| {
                    let barrier = Arc::clone(&barrier);
                    let root = &root;
                    scope.spawn(move || {
                        barrier.wait();
                        let mut sync_batch = AnchoredParentSyncBatch::new();
                        let (parent, _) = root
                            .open_parent_batched(
                                Path::new(&format!("shared/file-{index}")),
                                true,
                                &mut sync_batch,
                            )
                            .unwrap();
                        sync_batch.flush().unwrap();
                        parent.identity
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert!(identities.iter().all(|identity| *identity == identities[0]));
        let metadata = fs::symlink_metadata(root_path.join("shared")).unwrap();
        assert!(metadata.is_dir());
        assert!(!crate::metadata_is_link_or_reparse(&metadata));
    }
}
#[test]
fn sync_batch_keeps_unsynced_parents_after_progress_error() {
    let temp = tempfile::tempdir().unwrap();
    let root_path = temp.path().join("root");
    fs::create_dir_all(root_path.join("one/deep")).unwrap();
    fs::create_dir_all(root_path.join("two")).unwrap();
    let root = AnchoredRoot::open(&root_path).unwrap();
    let mut batch = AnchoredParentSyncBatch::new();
    batch
        .record(root.open_directory(Path::new("one/deep"), false).unwrap())
        .unwrap();
    batch
        .record(root.open_directory(Path::new("two"), false).unwrap())
        .unwrap();

    let error = batch
        .flush_with_progress(None, |completed, _| {
            if completed == 1 {
                Err(CheckPoError::Cancelled)
            } else {
                Ok(())
            }
        })
        .unwrap_err();

    assert!(matches!(error, CheckPoError::Cancelled));
    assert_eq!(batch.pending_count(), 1);
    batch.flush().unwrap();
    assert_eq!(batch.pending_count(), 0);
}

#[cfg(not(windows))]
#[test]
fn sync_batch_reports_capacity_flushes_in_metrics_and_progress() {
    let temp = tempfile::tempdir().unwrap();
    let root_path = temp.path().join("root");
    for index in 0..5 {
        fs::create_dir_all(root_path.join(format!("dir-{index}"))).unwrap();
    }
    let root = AnchoredRoot::open(&root_path).unwrap();
    let mut batch = AnchoredParentSyncBatch::with_max_pending(2);
    for index in 0..5 {
        batch
            .record(
                root.open_directory(Path::new(&format!("dir-{index}")), false)
                    .unwrap(),
            )
            .unwrap();
    }
    assert_eq!(batch.completed_count(), 4);
    assert_eq!(batch.total_count(), 5);
    let recorder = crate::checkpoint_metrics::ArtifactIoRecorder::default();
    let mut progress = Vec::new();

    batch
        .flush_with_progress(Some(&recorder), |completed, total| {
            progress.push((completed, total));
            Ok(())
        })
        .unwrap();

    assert_eq!(recorder.snapshot().directory_fsync_count, 5);
    assert_eq!(progress, vec![(4, 5), (5, 5)]);
}
