use super::{CancellationToken, CheckpointCreateMetrics, OperationProgress};
use crate::SnapshotId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct CreateCheckpointOptions {
    pub init_if_needed: bool,
    pub progress: Option<Arc<dyn Fn(OperationProgress) + Send + Sync>>,
    pub cancellation: Option<CancellationToken>,
}

#[derive(Clone, Default)]
pub struct DiffOptions {
    pub progress: Option<Arc<dyn Fn(OperationProgress) + Send + Sync>>,
    pub cancellation: Option<CancellationToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointSummary {
    pub checkpoint_id: SnapshotId,
    pub name: String,
    pub created_at_utc: String,
    pub file_count: usize,
    pub logical_size_bytes: u64,
    pub newly_stored_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfiledCheckpointResult {
    #[serde(flatten)]
    pub summary: CheckpointSummary,
    pub create_metrics: CheckpointCreateMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointDeleteResult {
    pub deleted_checkpoint_id: SnapshotId,
    pub deleted_snapshot_path: PathBuf,
    pub remaining_checkpoint_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub unknown: Vec<String>,
    pub unchanged_count: usize,
    pub complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointListResult {
    pub checkpoints: Vec<CheckpointSummary>,
    pub warnings: Vec<String>,
}
