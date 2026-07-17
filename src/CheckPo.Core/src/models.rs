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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryConfig {
    pub schema_version: u32,
    pub repo_format_version: u32,
    pub project_id: ProjectId,
    pub hash_algorithm: String,
    pub snapshot_format: String,
    pub object_format: String,
    pub manifest_chunk_format: String,
    pub manifest_storage_format: String,
    pub path_key_policy: String,
}

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

pub const OPERATION_PLAN_SCHEMA_VERSION: u32 = 1;
pub const TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION: u32 = 1;
pub const STORAGE_GC_PLAN_SCHEMA_VERSION: u32 = 2;
pub const TEMP_FILE_CLEANUP_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationPlan {
    pub schema_version: u32,
    pub checkpoint_id: SnapshotId,
    pub kind: OperationPlanKind,
    pub selected_paths: Option<Vec<TrackedUnityFilePath>>,
    pub operations: Vec<FileOperation>,
    pub directories_to_remove: Vec<TrackedUnityFilePath>,
    pub directories_to_create: Vec<TrackedUnityFilePath>,
    pub warnings: Vec<String>,
    pub restore_count: usize,
    pub replace_count: usize,
    pub delete_count: usize,
    pub metadata_count: usize,
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
        let metadata_count = operations
            .iter()
            .filter(|operation| operation.operation_type == FileOperationType::SetMetadata)
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
            .fold(0_u64, u64::saturating_add);
        let backup_bytes = operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation.operation_type,
                    FileOperationType::Delete | FileOperationType::Replace
                )
            })
            .filter_map(|operation| operation.before_size_bytes)
            .fold(0_u64, u64::saturating_add);
        Self {
            schema_version: OPERATION_PLAN_SCHEMA_VERSION,
            checkpoint_id,
            kind,
            selected_paths,
            has_changes: !operations.is_empty(),
            operations,
            directories_to_remove: Vec::new(),
            directories_to_create: Vec::new(),
            warnings: Vec::new(),
            restore_count,
            replace_count,
            delete_count,
            metadata_count,
            staged_bytes,
            backup_bytes,
            estimated_temporary_bytes: staged_bytes.saturating_add(backup_bytes),
        }
    }

    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }

    pub(crate) fn with_directory_changes(
        mut self,
        mut directories_to_remove: Vec<TrackedUnityFilePath>,
        mut directories_to_create: Vec<TrackedUnityFilePath>,
    ) -> Self {
        directories_to_remove.sort_by(|left, right| {
            right
                .as_str()
                .matches('/')
                .count()
                .cmp(&left.as_str().matches('/').count())
                .then_with(|| left.cmp(right))
        });
        directories_to_remove.dedup();
        directories_to_create.sort();
        directories_to_create.dedup();
        self.has_changes = self.has_changes
            || !directories_to_remove.is_empty()
            || !directories_to_create.is_empty();
        self.directories_to_remove = directories_to_remove;
        self.directories_to_create = directories_to_create;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointListResult {
    pub checkpoints: Vec<CheckpointSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationPlanKind {
    Restore,
    Discard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileOperation {
    pub operation_type: FileOperationType,
    pub path: TrackedUnityFilePath,
    pub before_hash: Option<ObjectId>,
    pub before_size_bytes: Option<u64>,
    pub before_modified_at_utc: Option<String>,
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
    SetMetadata,
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
    pub warnings: Vec<String>,
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
    pub referenced_object_count: usize,
    pub unavailable_referenced_object_count: usize,
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
pub struct StorageIndexSummary {
    pub checkpoint_count: usize,
    pub unique_blob_count: usize,
    pub logical_size_bytes: u64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionCleanupCandidate {
    pub location: String,
    pub transaction_id: String,
    pub state: String,
    pub journal_digest: String,
    pub file_count: usize,
    pub size_bytes: u64,
    pub tree_metadata_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionCleanupPlan {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub directory_count: usize,
    pub file_count: usize,
    pub total_bytes: u64,
    pub candidates: Vec<TransactionCleanupCandidate>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionQuarantineResult {
    pub transaction_id: String,
    pub quarantine_path: PathBuf,
    pub preserved_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvedTransactionQuarantine {
    pub transaction_id: String,
    pub quarantined_at_utc: Option<String>,
    pub quarantine_path: PathBuf,
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
