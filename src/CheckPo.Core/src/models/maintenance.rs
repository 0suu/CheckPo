use crate::{ObjectId, SnapshotId, TrackedUnityFilePath};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const STORAGE_GC_PLAN_SCHEMA_VERSION: u32 = 2;
pub const TEMP_FILE_CLEANUP_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationResult {
    pub is_valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildIndexResult {
    pub snapshot_count: usize,
    pub referenced_object_count: usize,
    pub unavailable_referenced_object_count: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StorageGcPlan {
    pub schema_version: u32,
    pub plan_id: String,
    pub checkpoint_count: usize,
    pub object_file_count: usize,
    pub referenced_blob_count: usize,
    pub unreferenced_blob_count: usize,
    pub unreferenced_logical_bytes: u64,
    pub manifest_chunk_file_count: usize,
    pub referenced_manifest_chunk_count: usize,
    pub unreferenced_manifest_chunk_count: usize,
    pub unreferenced_manifest_chunk_bytes: u64,
    pub unreferenced_inventory_node_count: usize,
    pub unreferenced_inventory_node_bytes: u64,
    pub unreferenced_blobs: Vec<UnreferencedBlob>,
    pub unreferenced_manifest_chunks: Vec<UnreferencedManifestChunk>,
    pub unreferenced_inventory_nodes: Vec<UnreferencedInventoryNode>,
    pub missing_references: Vec<MissingBlobReference>,
    pub invalid_object_locations: Vec<InvalidObjectLocation>,
    pub invalid_manifest_chunk_locations: Vec<InvalidManifestChunkLocation>,
    pub skipped_snapshots: Vec<SkippedSnapshot>,
    pub has_integrity_problems: bool,
    pub details_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageGcResult {
    pub plan: StorageGcPlan,
    pub applied: bool,
    pub completed: bool,
    pub committed_partially: bool,
    pub deleted_blob_count: usize,
    pub deleted_manifest_chunk_count: usize,
    pub deleted_manifest_chunk_bytes: u64,
    pub deleted_inventory_node_count: usize,
    pub deleted_inventory_node_bytes: u64,
    pub deleted_bytes: u64,
    pub failed_candidate: Option<PathBuf>,
    pub failure: Option<String>,
    pub remaining_candidate_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnreferencedBlob {
    pub object_id: ObjectId,
    pub object_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnreferencedManifestChunk {
    pub chunk_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnreferencedInventoryNode {
    pub node_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvalidManifestChunkLocation {
    pub chunk_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MissingBlobReference {
    pub checkpoint_id: SnapshotId,
    pub path: TrackedUnityFilePath,
    pub object_id: ObjectId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvalidObjectLocation {
    pub object_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkippedSnapshot {
    pub checkpoint_id: SnapshotId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrphanTempFile {
    pub path: TrackedUnityFilePath,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryTempFile {
    pub file_name: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TempFileCleanupPlan {
    pub schema_version: u32,
    pub plan_id: String,
    pub file_count: usize,
    pub total_bytes: u64,
    pub files: Vec<OrphanTempFile>,
    pub repository_files: Vec<RepositoryTempFile>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempFileCleanupResult {
    pub plan: TempFileCleanupPlan,
    pub deleted_file_count: usize,
    pub deleted_bytes: u64,
    pub warnings: Vec<String>,
}
