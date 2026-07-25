use super::*;

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
        let entries = super::super::windows_durability::list_directory_entries(&self.directory)
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
            let matches = super::super::windows_durability::windows_names_equal(
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
        let recorded_matches = super::super::windows_durability::windows_names_equal(
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
                super::super::windows_durability::rename_open_handle_no_replace_unflushed(
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

    pub(super) fn write_bytes_atomic(
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

    pub(super) fn replace_from_temporary(
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

            super::super::windows_durability::rename_open_handle_no_replace_unflushed(
                &displaced,
                &self.directory,
                &tombstone,
            )
            .map_err(|error| io_error(&destination_path, error))?;

            // Make the recoverable old destination durable before opening the
            // brief no-destination interval required by the Windows protocol.
            self.sync_all()?;
            after_windows_phase(ReplaceProtocolPhase::DestinationDetached)?;
            if let Err(error) = super::super::windows_durability::rename_open_handle_no_replace(
                &source,
                &self.directory,
                destination_leaf,
            ) {
                let _ = super::super::windows_durability::rename_open_handle_no_replace_unflushed(
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
            super::super::windows_durability::remove_open_handle(
                displaced,
                &self.directory,
                &tombstone,
            )
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

    #[cfg(all(test, windows))]
    pub(super) fn replace_from_temporary_with_hook(
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
    pub(super) fn replace_from_temporary_stopping_at_windows_phase(
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
    pub(super) fn replace_from_temporary_with_windows_phase_hook(
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
            if let Err(error) =
                super::super::windows_durability::rename_open_directory_handle_no_replace(
                    &source,
                    &destination_parent.directory,
                    destination_leaf,
                )
            {
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

    pub(super) fn unlink_file_if_bound_ref(
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
            super::super::windows_durability::remove_open_handle(file, &self.directory, leaf)
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
            super::super::windows_durability::remove_open_handle(file, &self.directory, leaf)
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
            super::super::windows_durability::remove_open_directory_handle(
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
            super::super::windows_durability::remove_open_directory_handle(
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
    pub(super) fn rename_no_replace_to_with_hook(
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
    pub(super) fn rename_no_replace_to_with_hooks(
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
    pub(super) fn unlink_file_if_bound_with_hooks(
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
    pub(super) fn unlink_file_if_bound_versioned_with_hooks(
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
    pub(super) fn unlink_dir_if_bound_with_hooks(
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
    super::super::windows_durability::list_directory_entries(&parent.directory)
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
