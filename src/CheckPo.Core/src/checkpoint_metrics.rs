use crate::CheckpointArtifactIoMetrics;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub(crate) enum IoTimingKind {
    ExistenceCheck,
    DirectoryPrepare,
    SourceRead,
    Hash,
    Write,
    FileFsync,
    Publish,
    DirectoryFsync,
    ExistingValidationRead,
    PostWriteReadback,
}

#[derive(Default)]
pub(crate) struct ArtifactIoRecorder {
    existence_check_nanos: AtomicU64,
    directory_prepare_nanos: AtomicU64,
    source_read_nanos: AtomicU64,
    hash_nanos: AtomicU64,
    write_nanos: AtomicU64,
    file_fsync_nanos: AtomicU64,
    publish_nanos: AtomicU64,
    directory_fsync_nanos: AtomicU64,
    existing_validation_read_nanos: AtomicU64,
    post_write_readback_nanos: AtomicU64,
    checked_count: AtomicU64,
    existing_count: AtomicU64,
    written_count: AtomicU64,
    repaired_count: AtomicU64,
    file_fsync_count: AtomicU64,
    directory_fsync_count: AtomicU64,
    post_write_readback_count: AtomicU64,
    directory_create_count: AtomicU64,
    hash_operation_count: AtomicU64,
    checked_bytes: AtomicU64,
    written_bytes: AtomicU64,
}

impl ArtifactIoRecorder {
    pub(crate) fn measure<T>(&self, kind: IoTimingKind, operation: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = operation();
        self.record_duration(kind, started.elapsed());
        if matches!(kind, IoTimingKind::Hash) {
            self.hash_operation_count.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(kind, IoTimingKind::PostWriteReadback) {
            self.post_write_readback_count
                .fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    pub(crate) fn record_duration(&self, kind: IoTimingKind, duration: Duration) {
        let nanos = duration_nanos(duration);
        let target = match kind {
            IoTimingKind::ExistenceCheck => &self.existence_check_nanos,
            IoTimingKind::DirectoryPrepare => &self.directory_prepare_nanos,
            IoTimingKind::SourceRead => &self.source_read_nanos,
            IoTimingKind::Hash => &self.hash_nanos,
            IoTimingKind::Write => &self.write_nanos,
            IoTimingKind::FileFsync => &self.file_fsync_nanos,
            IoTimingKind::Publish => &self.publish_nanos,
            IoTimingKind::DirectoryFsync => &self.directory_fsync_nanos,
            IoTimingKind::ExistingValidationRead => &self.existing_validation_read_nanos,
            IoTimingKind::PostWriteReadback => &self.post_write_readback_nanos,
        };
        target.fetch_add(nanos, Ordering::Relaxed);
    }

    pub(crate) fn checked(&self, size_bytes: u64) {
        self.checked_count.fetch_add(1, Ordering::Relaxed);
        self.checked_bytes.fetch_add(size_bytes, Ordering::Relaxed);
    }

    pub(crate) fn existing(&self) {
        self.existing_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn written(&self, size_bytes: u64) {
        self.written_count.fetch_add(1, Ordering::Relaxed);
        self.written_bytes.fetch_add(size_bytes, Ordering::Relaxed);
    }

    pub(crate) fn repaired(&self) {
        self.repaired_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn file_fsync(&self) {
        self.file_fsync_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn directory_fsync(&self) {
        #[cfg(not(windows))]
        self.directory_fsync_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn directory_created(&self) {
        self.directory_create_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> CheckpointArtifactIoMetrics {
        CheckpointArtifactIoMetrics {
            existence_check_micros: nanos_to_micros(
                self.existence_check_nanos.load(Ordering::Relaxed),
            ),
            directory_prepare_micros: nanos_to_micros(
                self.directory_prepare_nanos.load(Ordering::Relaxed),
            ),
            source_read_micros: nanos_to_micros(self.source_read_nanos.load(Ordering::Relaxed)),
            hash_micros: nanos_to_micros(self.hash_nanos.load(Ordering::Relaxed)),
            write_micros: nanos_to_micros(self.write_nanos.load(Ordering::Relaxed)),
            file_fsync_micros: nanos_to_micros(self.file_fsync_nanos.load(Ordering::Relaxed)),
            publish_micros: nanos_to_micros(self.publish_nanos.load(Ordering::Relaxed)),
            directory_fsync_micros: nanos_to_micros(
                self.directory_fsync_nanos.load(Ordering::Relaxed),
            ),
            existing_validation_read_micros: nanos_to_micros(
                self.existing_validation_read_nanos.load(Ordering::Relaxed),
            ),
            post_write_readback_micros: nanos_to_micros(
                self.post_write_readback_nanos.load(Ordering::Relaxed),
            ),
            checked_count: count_to_usize(self.checked_count.load(Ordering::Relaxed)),
            existing_count: count_to_usize(self.existing_count.load(Ordering::Relaxed)),
            written_count: count_to_usize(self.written_count.load(Ordering::Relaxed)),
            repaired_count: count_to_usize(self.repaired_count.load(Ordering::Relaxed)),
            file_fsync_count: count_to_usize(self.file_fsync_count.load(Ordering::Relaxed)),
            directory_fsync_count: count_to_usize(
                self.directory_fsync_count.load(Ordering::Relaxed),
            ),
            post_write_readback_count: count_to_usize(
                self.post_write_readback_count.load(Ordering::Relaxed),
            ),
            directory_create_count: count_to_usize(
                self.directory_create_count.load(Ordering::Relaxed),
            ),
            hash_operation_count: count_to_usize(self.hash_operation_count.load(Ordering::Relaxed)),
            checked_bytes: self.checked_bytes.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
        }
    }
}

pub(crate) fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn nanos_to_micros(nanos: u64) -> u64 {
    nanos / 1_000
}

fn count_to_usize(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}
