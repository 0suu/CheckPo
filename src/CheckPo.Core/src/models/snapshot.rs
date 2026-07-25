use crate::{ObjectId, ProjectId, SnapshotId, TrackedUnityFilePath};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotFile {
    // This is the format-neutral in-memory view. Snapshot v2 persists the
    // header as a root chunk and the entries as a Merkle radix manifest.
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub parent_snapshot_id: Option<SnapshotId>,
    pub created_at_utc: String,
    pub name: String,
    pub tool_version: String,
    pub tracked_roots: Vec<String>,
    pub files: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEntry {
    pub path: TrackedUnityFilePath,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub content: SnapshotContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SnapshotContent {
    Whole { hash: ObjectId, size_bytes: u64 },
}

impl SnapshotEntry {
    pub fn content_hash(&self) -> &ObjectId {
        match &self.content {
            SnapshotContent::Whole { hash, .. } => hash,
        }
    }

    pub fn content_size_bytes(&self) -> u64 {
        match &self.content {
            SnapshotContent::Whole { size_bytes, .. } => *size_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedFile {
    pub path: TrackedUnityFilePath,
    pub full_path: PathBuf,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub hash: ObjectId,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanWarning {
    pub relative_path: String,
    pub reason: String,
}
