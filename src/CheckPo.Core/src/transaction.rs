mod apply;
mod journal;
mod plan;
mod project_file_ops;
mod recovery;
#[cfg(test)]
mod tests;

use crate::{
    load_project_snapshot, report_operation_progress, ApplyOptions, ApplyResult, CancellationToken,
    CheckPoError, FileOperation, FileOperationType, ObjectId, OperationPlan, OperationPlanKind,
    OperationProgress, PendingTransaction, ProjectContext, Result, SnapshotId,
    TrackedUnityFilePath, TransactionCleanupCandidate, TransactionCleanupPlan,
    TransactionCleanupResult, TransactionQuarantineResult, TransactionRecoveryConflict,
    TransactionRecoveryConflictPlan, TransactionRecoveryConflictResult, TransactionRecoveryFailure,
    TransactionRecoveryResult, UnresolvedTransactionQuarantine,
};
#[cfg(test)]
use filetime::FileTime;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub use apply::apply_plan;
pub(crate) use apply::apply_restore_plan_and_resolve_quarantines;
#[cfg(test)]
use apply::{
    apply_plan_inner, apply_plan_inner_authoritative, ensure_available_space,
    estimated_project_required_bytes, estimated_repository_required_bytes, TransactionFaultPoint,
    TRANSACTION_BACKUP_FILE_BATCH_SIZE,
};
pub use journal::{
    analyze_transaction_cleanup, cleanup_journals_with_expected_plan,
    ensure_no_pending_transactions, pending_transactions, pending_transactions_for_project,
};
use journal::{
    dir_size, journals_dir, read_transaction_journal, validate_transaction_journal_identity,
    write_journal, JournalState, TransactionIntent, TransactionJournal, JOURNAL_STATE_UNREADABLE,
    TRANSACTION_JOURNAL_SCHEMA_VERSION,
};
pub use plan::build_plan_with_progress_and_cancellation;
pub(crate) use plan::normalize_discard_selection;
use plan::{
    validate_expected_plan, validate_journal_directory_topology, validate_journal_operations,
};
#[cfg(test)]
use project_file_ops::backup_project_file_by_reflink_or_copy;
use project_file_ops::*;
use recovery::invalidate_operation_fingerprints;
pub use recovery::{
    analyze_transaction_recovery_conflicts, ensure_no_unresolved_transaction_quarantines,
    quarantine_transaction, recover_transaction_with_conflict_export, recover_transactions,
    unresolved_transaction_quarantines, unresolved_transaction_quarantines_for_project,
};
