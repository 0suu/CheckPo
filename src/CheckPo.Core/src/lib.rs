mod checkpoint;
mod checkpoint_create_journal;
mod checkpoint_metrics;
mod checkpoint_names;
mod db;
mod diagnostics;
mod diff;
mod discard;
mod maintenance;
mod models;
mod path;
mod project;
mod restore;
mod scanner;
mod storage;
mod storage_root_setting;
mod transaction;
mod verify;

pub use checkpoint::{
    create_checkpoint, create_checkpoint_profiled, delete_checkpoint, list_checkpoints,
    list_checkpoints_for_project, list_checkpoints_with_warnings_for_project,
    recover_checkpoint_deletions, rename_checkpoint,
};
pub use db::{
    checkpoint_index_status, checkpoint_summaries_and_storage_summary_from_index, rebuild_index,
    rebuild_index_for_project_with_progress_and_cancellation, storage_index_summary_from_index,
    storage_summary_from_index, CachedFileFingerprint, FileFingerprintUpdate,
};
pub use diagnostics::{
    diagnostic_log_directory, init_diagnostics, log_checkpoint_create_metrics, log_operation_error,
    DiagnosticsGuard,
};
pub use diff::{
    diff_checkpoint, diff_checkpoint_metadata, diff_checkpoint_metadata_with_cancellation,
    diff_checkpoint_with_options,
};
pub use discard::{
    apply_discard_files_plan, apply_discard_plan_with_progress_and_cancellation,
    preview_discard_files, preview_discard_with_progress_and_cancellation,
};
pub use maintenance::{
    analyze_gc, analyze_gc_with_progress_and_cancellation, analyze_orphan_temp_files,
    apply_gc_with_expected_plan, apply_gc_with_expected_plan_and_progress_and_cancellation,
    cleanup_orphan_temp_files_with_expected_plan, storage_summary,
};
pub use models::CancellationToken;
pub use models::{
    ApplyOptions, ApplyResult, CheckpointArtifactIoMetrics, CheckpointCreateMetrics,
    CheckpointDeleteResult, CheckpointIndexState, CheckpointIndexStatus, CheckpointIoMetrics,
    CheckpointListResult, CheckpointScanMetrics, CheckpointSummary, CreateCheckpointOptions,
    DiffOptions, DiffResult, FileOperation, FileOperationType, InvalidManifestChunkLocation,
    InvalidObjectLocation, MissingBlobReference, OperationPlan, OperationPlanKind,
    OperationProgress, OrphanTempFile, PendingTransaction, ProfiledCheckpointResult,
    ProjectContext, ProjectLocationStatus, ProjectMarkerFile, ProjectView, ProjectWarning,
    ProjectWarningKind, RebuildIndexResult, RegistryFile, RegistryProjectEntry, RepositoryConfig,
    RepositoryTempFile, ScanWarning, ScannedFile, SkippedSnapshot, SnapshotContent, SnapshotEntry,
    SnapshotFile, StorageGcPlan, StorageGcResult, StorageIndexSummary, StorageSummary,
    TempFileCleanupPlan, TempFileCleanupResult, TransactionCleanupCandidate,
    TransactionCleanupPlan, TransactionCleanupResult, TransactionQuarantineResult,
    TransactionRecoveryFailure, TransactionRecoveryResult, UnreferencedBlob,
    UnreferencedInventoryNode, UnreferencedManifestChunk, UnresolvedTransactionQuarantine,
    VerificationResult, OPERATION_PLAN_SCHEMA_VERSION, STORAGE_GC_PLAN_SCHEMA_VERSION,
    TEMP_FILE_CLEANUP_PLAN_SCHEMA_VERSION, TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION,
};
pub use path::{
    hash_bytes, parse_tracked_paths, ObjectId, ProjectId, ProjectRoot, SnapshotId, StorageRoot,
    TrackedUnityFilePath,
};
pub use project::{
    confirm_project_location, default_storage_root, init_project, init_project_with_storage_root,
    load_project, load_project_view, marker_path, project_view, registry_path,
    start_as_separate_project, start_as_separate_project_with_storage_root,
};
pub use restore::{
    apply_restore_plan, apply_restore_plan_with_progress_and_cancellation, preview_restore,
    preview_restore_with_progress_and_cancellation,
};
pub use storage::{
    canonical_snapshot_bytes, db_path, file_fingerprint_db_path, load_snapshot, object_path,
    read_json, read_latest_snapshot_id, snapshot_id_from_bytes, snapshot_path,
};
pub use storage_root_setting::set_project_storage_root;
pub use transaction::{
    analyze_transaction_cleanup, apply_plan, cleanup_journals_with_expected_plan,
    pending_transactions, pending_transactions_for_project, quarantine_transaction,
    recover_transactions, unresolved_transaction_quarantines,
    unresolved_transaction_quarantines_for_project,
};
pub use verify::{
    verify_checkpoint, verify_checkpoint_with_progress_and_cancellation, verify_project,
    verify_project_with_progress_and_cancellation,
};

pub(crate) use checkpoint::recover_checkpoint_deletions_unlocked;
pub(crate) use checkpoint_create_journal::{
    recover_checkpoint_creations_unlocked, CreateJournalHandle,
};
pub(crate) use checkpoint_names::{
    apply_checkpoint_name_overrides, read_checkpoint_name_overrides,
    remove_checkpoint_name_override, write_checkpoint_name_overrides,
};
pub(crate) use db::{
    delete_snapshot_from_index, index_snapshot_with_index_connection, invalidate_file_fingerprints,
    list_checkpoint_summaries_from_index, load_file_fingerprints,
    load_object_integrity_fingerprints, open_index_connection, rebuild_index_for_project_unlocked,
    refresh_file_fingerprints, refresh_object_integrity_fingerprints,
    ObjectIntegrityFingerprintUpdate,
};
pub(crate) use models::{ensure_not_cancelled, report_operation_progress};
pub(crate) use path::{
    is_checkpo_atomic_materialization_temporary_file, is_checkpo_owned_temporary_file,
    is_checkpo_temporary_file, relative_path_from_project,
};
pub(crate) use project::{
    acquire_project_location_lock, acquire_project_repository_lock,
    acquire_project_repository_shared_lock, acquire_registry_lock,
    ensure_project_location_allows_mutation, ensure_repo_outside_project, load_project_marker,
    load_registry, normalize_existing_dir, project_location_status_and_warnings,
    update_registry_locked, validate_unity_project_root,
};
pub(crate) use scanner::{scan_project_for_checkpoint, scan_project_for_checkpoint_with_baseline};
pub(crate) use storage::{
    acquire_repository_lock, acquire_repository_shared_lock, available_space_bytes, canonical_utc,
    checkpoint_names_path, create_absolute_dir_all_no_follow, create_dir_all_no_follow,
    ensure_regular_directory_no_follow, ensure_regular_file_no_follow, init_repo_layout,
    list_snapshot_ids, load_project_snapshot, load_repo_config, metadata_is_link_or_reparse,
    now_utc_string, object_id_from_loose_relative_path, prepare_snapshot,
    publish_prepared_snapshot_root, publish_prepared_snapshot_root_profiled,
    put_object_from_anchored_file_with_known_hash_profiled_batched, repo_root, snapshots_dir,
    store_prepared_snapshot_chunks_profiled_batched, sync_parent_dir,
    validate_repository_layout_no_follow, verify_stored_object_profiled, write_latest_snapshot_id,
    RepositoryLock,
};
pub(crate) use transaction::{
    build_plan_with_progress_and_cancellation, ensure_no_pending_transactions,
    ensure_no_unresolved_transaction_quarantines,
};

/// Debug-build test seam for corruption and recovery integration tests.
/// Production builds intentionally expose no direct snapshot-store mutator.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn __debug_test_save_snapshot(
    repo_root: &std::path::Path,
    snapshot: &SnapshotFile,
) -> Result<SnapshotId> {
    storage::save_snapshot(repo_root, snapshot)
}

/// Debug-build test seam for registering a deliberately crafted snapshot root.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn __debug_test_add_snapshot_to_inventory(
    repo_root: &std::path::Path,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
) -> Result<()> {
    let expected = storage::inventory_head_id(repo_root, project_id)?;
    storage::add_snapshot_to_inventory_if_head(
        repo_root,
        project_id,
        snapshot_id,
        &expected,
        &format!("debug-add-{snapshot_id}"),
    )
    .map(|_| ())
}

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CheckPoError {
    #[error("{0}")]
    User(String),
    #[error("invalid Unity project: {0}")]
    InvalidProject(String),
    #[error("invalid tracked path: {0}")]
    InvalidTrackedPath(String),
    #[error("invalid id: {0}")]
    InvalidId(String),
    #[error("outside tracked scope: {0}")]
    OutsideTrackedScope(String),
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    #[error("object missing: {0}")]
    ObjectMissing(String),
    #[error("object hash mismatch: {0}")]
    ObjectHashMismatch(String),
    #[error("working tree changed: {0}")]
    WorkingTreeChanged(String),
    #[error("repository locked: {0}")]
    RepositoryLocked(String),
    #[error("requested checkpoint storage {requested} conflicts with the registered storage {registered}")]
    StorageRootConflict {
        requested: PathBuf,
        registered: PathBuf,
    },
    #[error("checkpoint storage is unavailable: {0}")]
    StorageRootUnavailable(String),
    #[error("pending transaction: {0}")]
    PendingTransaction(String),
    #[error("unresolved transaction quarantine: {0}")]
    UnresolvedTransactionQuarantine(String),
    #[error("index unavailable: {0}")]
    IndexUnavailable(String),
    #[error("unsupported {artifact} version {found}; this CheckPo supports version {supported}")]
    UnsupportedFormat {
        artifact: String,
        found: u32,
        supported: u32,
    },
    #[error("{0}")]
    CopiedProjectSuspected(String),
    #[error("folder metadata cannot be changed independently: {0}")]
    UnsafeFolderMetaOperation(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("json error at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("database error at {path}: {source}")]
    Db {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("corruption: {0}")]
    Corruption(String),
    #[error("unexpected error: {0}")]
    Unexpected(String),
}

pub type Result<T> = std::result::Result<T, CheckPoError>;

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> CheckPoError {
    CheckPoError::Io {
        path: path.into(),
        source,
    }
}

pub(crate) fn json_error(path: impl Into<PathBuf>, source: serde_json::Error) -> CheckPoError {
    CheckPoError::Json {
        path: path.into(),
        source,
    }
}

pub(crate) fn db_error(path: impl Into<PathBuf>, source: rusqlite::Error) -> CheckPoError {
    CheckPoError::Db {
        path: path.into(),
        source,
    }
}

pub(crate) fn user_error(message: impl Into<String>) -> CheckPoError {
    CheckPoError::User(message.into())
}
