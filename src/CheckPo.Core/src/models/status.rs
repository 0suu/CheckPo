use super::{CheckpointSummary, PendingTransaction, ProjectView, UnresolvedTransactionQuarantine};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSummary {
    pub checkpoint_count: usize,
    pub unique_blob_count: usize,
    pub logical_size_bytes: u64,
    pub stored_size_bytes: u64,
    #[serde(default)]
    pub recovery_rescue_file_count: usize,
    #[serde(default)]
    pub recovery_rescue_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageIndexSummary {
    pub checkpoint_count: usize,
    pub unique_blob_count: usize,
    pub logical_size_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StorageSummaryDetail {
    IndexOnly,
    Exact,
}

#[derive(Debug, Clone, Copy)]
pub struct ProjectStatusOptions {
    pub storage_detail: StorageSummaryDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStorageSummary {
    pub detail: StorageSummaryDetail,
    pub checkpoint_count: usize,
    pub unique_blob_count: usize,
    pub logical_size_bytes: u64,
    pub stored_size_bytes: Option<u64>,
    pub recovery_rescue_file_count: Option<usize>,
    pub recovery_rescue_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatus {
    pub project: ProjectView,
    pub project_path: PathBuf,
    pub checkpoint_index: CheckpointIndexStatus,
    pub checkpoints: Option<Vec<CheckpointSummary>>,
    pub storage: Option<ProjectStorageSummary>,
    pub pending_transactions: Vec<PendingTransaction>,
    pub unresolved_quarantines: Vec<UnresolvedTransactionQuarantine>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckpointIndexState {
    Current,
    Missing,
    Stale,
    Incompatible,
    Corrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointIndexStatus {
    pub state: CheckpointIndexState,
    pub rebuildable: bool,
    pub detail: Option<String>,
}

impl CheckpointIndexStatus {
    pub fn current() -> Self {
        Self {
            state: CheckpointIndexState::Current,
            rebuildable: false,
            detail: None,
        }
    }
}
