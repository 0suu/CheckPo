use super::*;

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

    pub(super) fn record_directory_handle(
        &mut self,
        display_path: &Path,
        directory: &File,
    ) -> Result<()> {
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
