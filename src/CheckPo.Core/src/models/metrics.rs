use serde::{Deserialize, Serialize};

/// Detailed wall-clock timings for one checkpoint creation.
///
/// Top-level phase timings are mutually exclusive. The nested scan and I/O
/// timings are diagnostic breakdowns and must not be added to the top-level
/// phase timings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointCreateMetrics {
    pub total_micros: u64,
    pub setup_micros: u64,
    pub baseline_load_micros: u64,
    pub scan_total_micros: u64,
    pub object_preload_micros: u64,
    pub object_store_micros: u64,
    pub object_store_parallelism: usize,
    pub object_integrity_cache_update_micros: u64,
    pub manifest_build_micros: u64,
    pub manifest_store_micros: u64,
    pub durability_barrier_micros: u64,
    pub object_readback_micros: u64,
    pub root_journal_ref_commit_micros: u64,
    pub snapshot_index_update_micros: u64,
    pub file_fingerprint_update_micros: u64,
    pub unattributed_micros: u64,
    pub scan: CheckpointScanMetrics,
    pub io: CheckpointIoMetrics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointScanMetrics {
    pub enumerate_micros: u64,
    pub fingerprint_assessment_micros: u64,
    pub hash_wall_micros: u64,
    pub finalize_micros: u64,
    pub hashed_file_count: usize,
    pub hashed_bytes: u64,
    pub reused_file_count: usize,
    pub reused_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointIoMetrics {
    pub loose_objects: CheckpointArtifactIoMetrics,
    pub manifest_chunks: CheckpointArtifactIoMetrics,
    pub snapshot_root: CheckpointArtifactIoMetrics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointArtifactIoMetrics {
    pub existence_check_micros: u64,
    /// Directory safety checks and mkdir work, excluding directory fsync.
    pub directory_prepare_micros: u64,
    pub source_read_micros: u64,
    pub hash_micros: u64,
    pub write_micros: u64,
    pub file_fsync_micros: u64,
    pub publish_micros: u64,
    pub directory_fsync_micros: u64,
    pub existing_validation_read_micros: u64,
    pub post_write_readback_micros: u64,
    pub checked_count: usize,
    pub existing_count: usize,
    pub written_count: usize,
    pub repaired_count: usize,
    pub file_fsync_count: usize,
    pub directory_fsync_count: usize,
    pub post_write_readback_count: usize,
    pub directory_create_count: usize,
    /// Number of timed hash segments (buffer updates for loose objects,
    /// complete digest calculations for manifest chunks).
    pub hash_operation_count: usize,
    pub checked_bytes: u64,
    pub written_bytes: u64,
}
