use super::*;

impl AnchoredRoot {
    const MAX_ANCHORED_JSON_BYTES: u64 = 512 * 1024 * 1024;

    #[cfg(unix)]
    pub(crate) fn from_held_parent(parent: AnchoredParent) -> Self {
        Self {
            display_path: parent.display_path,
            identity: parent.identity,
            #[cfg(any(unix, windows))]
            directory: parent.directory,
        }
    }

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

    #[cfg(unix)]
    pub(crate) fn make_file_private(&self, relative: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let file = self.open_file(relative)?;
        file.file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error(self.display_path.join(relative), error))?;
        file.file
            .sync_all()
            .map_err(|error| io_error(self.display_path.join(relative), error))?;
        self.verify_binding(relative, &file)?;
        self.verify_root_binding()
    }

    #[cfg(not(unix))]
    pub(crate) fn make_file_private(&self, _relative: &Path) -> Result<()> {
        Ok(())
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
    pub(super) fn open_file_with_component_hook(
        &self,
        relative: &Path,
        hook: impl FnMut(usize, &Path),
    ) -> Result<AnchoredFile> {
        self.open_file_impl(relative, hook)
    }
}
