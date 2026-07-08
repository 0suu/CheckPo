use crate::{ObjectId, ProjectId, SnapshotId, TrackedUnityFilePath};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMarkerFile {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub created_at_utc: String,
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub project_id: ProjectId,
    pub project_root: crate::ProjectRoot,
    pub storage_root: crate::StorageRoot,
    pub repo_root: PathBuf,
    pub location_status: ProjectLocationStatus,
    pub warnings: Vec<ProjectWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    pub project_id: String,
    pub project_root_path: PathBuf,
    pub storage_root_path: PathBuf,
    pub project_name: Option<String>,
    pub unity_version: Option<String>,
    pub location_status: ProjectLocationStatus,
    pub warnings: Vec<ProjectWarning>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProjectLocationStatus {
    Current,
    MovedFromMissingOrDifferentMarker,
    CopiedSuspected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProjectWarningKind {
    CopiedProjectSuspected,
    ProjectMoved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectWarning {
    pub kind: ProjectWarningKind,
    pub message: String,
    pub location_status: ProjectLocationStatus,
    pub previous_project_root_path: PathBuf,
    pub current_project_root_path: PathBuf,
    pub previous_path_exists: bool,
    pub previous_marker_has_same_project_id: bool,
    pub requires_user_decision: bool,
    pub destructive_operations_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryFile {
    pub schema_version: u32,
    pub projects: std::collections::BTreeMap<String, RegistryProjectEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryProjectEntry {
    pub storage_root_path: PathBuf,
    pub last_project_root_path: PathBuf,
    pub project_name: Option<String>,
    pub updated_at_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryConfig {
    pub schema_version: u32,
    pub repo_format_version: u32,
    pub project_id: ProjectId,
    pub hash_algorithm: String,
    pub snapshot_format: String,
    pub object_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotFile {
    // The serialized field order and serde names are part of canonical-json-v1.
    // Changing them changes snapshot ids and breaks digest verification for
    // existing snapshots.
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

#[derive(Clone, Default)]
pub struct CreateCheckpointOptions {
    pub init_if_needed: bool,
    pub progress: Option<std::sync::Arc<dyn Fn(OperationProgress) + Send + Sync>>,
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
    pub unchanged_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperationPlan {
    pub checkpoint_id: SnapshotId,
    pub kind: OperationPlanKind,
    pub selected_paths: Option<Vec<TrackedUnityFilePath>>,
    pub operations: Vec<FileOperation>,
    pub warnings: Vec<String>,
    pub restore_count: usize,
    pub replace_count: usize,
    pub delete_count: usize,
    pub staged_bytes: u64,
    pub backup_bytes: u64,
    pub estimated_temporary_bytes: u64,
    pub has_changes: bool,
}

impl OperationPlan {
    pub fn new(
        checkpoint_id: SnapshotId,
        kind: OperationPlanKind,
        selected_paths: Option<Vec<TrackedUnityFilePath>>,
        mut operations: Vec<FileOperation>,
    ) -> Self {
        operations.sort_by(|a, b| a.path.cmp(&b.path));
        let restore_count = operations
            .iter()
            .filter(|operation| operation.operation_type == FileOperationType::Restore)
            .count();
        let replace_count = operations
            .iter()
            .filter(|operation| operation.operation_type == FileOperationType::Replace)
            .count();
        let delete_count = operations
            .iter()
            .filter(|operation| operation.operation_type == FileOperationType::Delete)
            .count();
        let staged_bytes = operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Restore | FileOperationType::Replace
                )
            })
            .filter_map(|operation| operation.after_size_bytes)
            .sum();
        let backup_bytes = operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Delete | FileOperationType::Replace
                )
            })
            .filter_map(|operation| operation.before_size_bytes)
            .sum();
        Self {
            checkpoint_id,
            kind,
            selected_paths,
            has_changes: !operations.is_empty(),
            operations,
            warnings: Vec::new(),
            restore_count,
            replace_count,
            delete_count,
            staged_bytes,
            backup_bytes,
            estimated_temporary_bytes: staged_bytes + backup_bytes,
        }
    }

    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationPlanKind {
    Restore,
    Discard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileOperation {
    pub operation_type: FileOperationType,
    pub path: TrackedUnityFilePath,
    pub before_hash: Option<ObjectId>,
    pub before_size_bytes: Option<u64>,
    pub after_hash: Option<ObjectId>,
    pub after_size_bytes: Option<u64>,
    pub after_modified_at_utc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileOperationType {
    Restore,
    Replace,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOptions {
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub checkpoint_id: SnapshotId,
    pub plan: OperationPlan,
    pub applied: bool,
    pub transaction_id: Option<String>,
    pub journal_path: Option<PathBuf>,
}

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
    pub object_count: usize,
    pub missing_object_count: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSummary {
    pub checkpoint_count: usize,
    pub unique_blob_count: usize,
    pub logical_size_bytes: u64,
    pub stored_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageGcPlan {
    pub checkpoint_count: usize,
    pub object_file_count: usize,
    pub referenced_blob_count: usize,
    pub unreferenced_blob_count: usize,
    pub unreferenced_logical_bytes: u64,
    pub unreferenced_blobs: Vec<UnreferencedBlob>,
    pub missing_references: Vec<MissingBlobReference>,
    pub invalid_object_locations: Vec<InvalidObjectLocation>,
    pub skipped_snapshots: Vec<SkippedSnapshot>,
    pub has_integrity_problems: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageGcResult {
    pub plan: StorageGcPlan,
    pub applied: bool,
    pub deleted_blob_count: usize,
    pub deleted_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreferencedBlob {
    pub object_id: ObjectId,
    pub object_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingBlobReference {
    pub checkpoint_id: SnapshotId,
    pub path: TrackedUnityFilePath,
    pub object_id: ObjectId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidObjectLocation {
    pub object_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedSnapshot {
    pub checkpoint_id: SnapshotId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingTransaction {
    pub transaction_id: String,
    pub state: String,
    pub journal_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionRecoveryResult {
    pub recovered_transaction_count: usize,
    pub failed_transaction_count: usize,
    pub recovered_transaction_ids: Vec<String>,
    pub failed_transactions: Vec<TransactionRecoveryFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionRecoveryFailure {
    pub transaction_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionCleanupResult {
    pub deleted_directory_count: usize,
    pub deleted_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanTempFile {
    pub path: TrackedUnityFilePath,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempFileCleanupPlan {
    pub file_count: usize,
    pub total_bytes: u64,
    pub files: Vec<OrphanTempFile>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationProgress {
    pub phase: String,
    pub completed: usize,
    pub total: usize,
    pub current_item: Option<String>,
}

#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken").finish_non_exhaustive()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub fn ensure_not_cancelled(cancellation: Option<&CancellationToken>) -> crate::Result<()> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(crate::CheckPoError::Cancelled);
    }
    Ok(())
}

pub fn report_operation_progress(
    progress: Option<&dyn Fn(OperationProgress)>,
    phase: impl Into<String>,
    completed: usize,
    total: usize,
    current_item: Option<String>,
) {
    if let Some(progress) = progress {
        progress(OperationProgress {
            phase: phase.into(),
            completed,
            total,
            current_item,
        });
    }
}
