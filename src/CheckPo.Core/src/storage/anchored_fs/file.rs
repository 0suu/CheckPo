use super::*;

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

    pub(super) fn current_version(&self) -> Result<FileVersion> {
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

    pub(super) fn hash_with_poll(
        &mut self,
        mut poll: impl FnMut() -> Result<()>,
    ) -> Result<AnchoredHash> {
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

    pub(super) fn copy_and_hash_to_inner(
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
