use crate::{ObjectId, ProjectId, SnapshotId, TrackedUnityFilePath};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const OPERATION_PLAN_SCHEMA_VERSION: u32 = 2;
pub const TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION: u32 = 1;
pub const TRANSACTION_RECOVERY_CONFLICT_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationPlan {
    pub schema_version: u32,
    pub authorization: String,
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
            authorization: String::new(),
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
    pub recovery_conflict_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionRecoveryConflict {
    pub path: TrackedUnityFilePath,
    pub current_hash: ObjectId,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub metadata_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionRecoveryConflictPlan {
    pub schema_version: u32,
    pub plan_id: String,
    pub transaction_id: String,
    pub checkpoint_id: SnapshotId,
    pub conflicts: Vec<TransactionRecoveryConflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionRecoveryConflictResult {
    pub transaction_id: String,
    pub recovered: bool,
    pub export_directory: Option<PathBuf>,
    pub exported_paths: Vec<TrackedUnityFilePath>,
    pub restored_without_export_count: usize,
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
