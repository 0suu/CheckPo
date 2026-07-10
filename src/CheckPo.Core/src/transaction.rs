mod apply;
mod journal;
mod plan;
mod project_file_ops;
mod recovery;
#[cfg(test)]
mod tests;

use crate::{
    acquire_repository_lock, hash_file, load_project_snapshot, move_file_no_replace,
    report_operation_progress, storage::copy_object_to_file, write_json_atomic, ApplyOptions,
    ApplyResult, CancellationToken, CheckPoError, FileOperation, FileOperationType, ObjectId,
    OperationPlan, OperationPlanKind, OperationProgress, PendingTransaction, ProjectContext,
    Result, SnapshotId, TrackedUnityFilePath, TransactionCleanupResult, TransactionRecoveryFailure,
    TransactionRecoveryResult,
};
use filetime::FileTime;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub use apply::apply_plan;
#[cfg(test)]
use apply::{
    apply_plan_inner, ensure_available_space, estimated_project_required_bytes,
    estimated_repository_required_bytes, TransactionFaultPoint,
};
pub use journal::{
    cleanup_journals, ensure_no_pending_transactions, pending_transactions,
    pending_transactions_for_project,
};
use journal::{
    directory_is_empty_or_missing, journals_dir, validate_transaction_journal_identity,
    write_journal, JournalState, TransactionJournal, JOURNAL_STATE_UNREADABLE,
};
pub use plan::build_plan_with_progress_and_cancellation;
use plan::validate_expected_plan;
#[cfg(test)]
use project_file_ops::backup_project_file_by_reflink_or_copy;
use project_file_ops::*;
use recovery::invalidate_operation_fingerprints;
pub use recovery::recover_transactions;
