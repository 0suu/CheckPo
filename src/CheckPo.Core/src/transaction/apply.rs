use super::*;
use rayon::prelude::*;

const DEFAULT_TRANSACTION_STAGE_CONCURRENCY: usize = 8;
pub(super) const TRANSACTION_BACKUP_FILE_BATCH_SIZE: usize = 64;
const TRANSACTION_BACKUP_PARENT_MAX_PENDING: usize = 32;

struct StagingOperationOutcome {
    index: usize,
    result: Result<()>,
    sync_batch: crate::storage::AnchoredParentSyncBatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransactionFaultPoint {
    ApplyingJournalWritten,
    ProjectFileBackedUp,
    BackupDirectoryBarrierBefore,
    BackupDirectoryBarrierAfter,
    BackupSourceCleanupBefore,
    BackupSourceCleanupAfter,
    ProjectDirectoriesRemoved,
    ProjectDirectoriesCreated,
    ProjectFileRestored,
    ProjectMetadataUpdated,
    ProjectRestoreDirectoryBarrierBefore,
    ProjectRestoreDirectoryBarrierAfter,
    StagedPayloadCleanupBefore,
    StagedPayloadCleanupAfter,
    OperationsAppliedBeforeCommit,
}

pub(super) type TransactionFaultHook<'a> = Option<&'a dyn Fn(TransactionFaultPoint) -> Result<()>>;

pub fn apply_plan(
    project: &ProjectContext,
    plan: OperationPlan,
    options: ApplyOptions,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<ApplyResult> {
    apply_plan_inner(project, plan, options, progress, cancellation, None)
}

pub(super) fn apply_plan_inner(
    project: &ProjectContext,
    plan: OperationPlan,
    options: ApplyOptions,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    fault_hook: TransactionFaultHook<'_>,
) -> Result<ApplyResult> {
    apply_plan_inner_with_quarantine_resolution(
        project,
        plan,
        options,
        progress,
        cancellation,
        fault_hook,
        None,
    )
}

pub(crate) fn apply_restore_plan_and_resolve_quarantines(
    project: &ProjectContext,
    plan: OperationPlan,
    options: ApplyOptions,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    checkpoint_id: &SnapshotId,
) -> Result<ApplyResult> {
    apply_plan_inner_with_quarantine_resolution(
        project,
        plan,
        options,
        progress,
        cancellation,
        None,
        Some(checkpoint_id),
    )
}

fn apply_plan_inner_with_quarantine_resolution(
    project: &ProjectContext,
    plan: OperationPlan,
    options: ApplyOptions,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    fault_hook: TransactionFaultHook<'_>,
    resolve_quarantines_for: Option<&SnapshotId>,
) -> Result<ApplyResult> {
    if !options.yes {
        return Err(crate::user_error("apply requires --yes."));
    }
    crate::ensure_project_location_allows_mutation(project)?;
    let _lock = crate::acquire_project_repository_lock(project, "transaction-apply")?;
    ensure_no_pending_transactions(project)?;
    if plan.kind != OperationPlanKind::Restore {
        crate::ensure_no_unresolved_transaction_quarantines(project)?;
    }
    if !plan.warnings.is_empty() {
        return Err(crate::user_error(format!(
            "operation cannot be applied while scan warnings exist: {}",
            plan.warnings.join("; ")
        )));
    }
    validate_expected_plan(project, &plan)?;
    if !plan.has_changes {
        if let Some(checkpoint_id) = resolve_quarantines_for {
            super::recovery::resolve_unverified_transaction_quarantines_unlocked(
                project,
                checkpoint_id,
            )?;
        }
        return Ok(ApplyResult {
            checkpoint_id: plan.checkpoint_id.clone(),
            plan,
            applied: false,
            transaction_id: None,
            journal_path: None,
            warnings: Vec::new(),
        });
    }
    ensure_capacity_for_plan(project, &plan)?;
    crate::ensure_not_cancelled(cancellation)?;
    let transaction_id = Uuid::new_v4().simple().to_string();
    let journal_root = journals_dir(&project.repo_root).join(&transaction_id);
    let staged_root = journal_root.join("staged");
    let backup_root = journal_root.join("backup");
    let journals = journals_dir(&project.repo_root);
    crate::create_dir_all_no_follow(&journals, &staged_root)?;
    crate::create_dir_all_no_follow(&journals, &backup_root)?;
    crate::storage::sync_parent_chain(&backup_root, &journals_dir(&project.repo_root))?;
    let mut journal = TransactionJournal {
        schema_version: TRANSACTION_JOURNAL_SCHEMA_VERSION,
        transaction_id: transaction_id.clone(),
        state: JournalState::Created,
        checkpoint_id: plan.checkpoint_id.clone(),
        kind: plan.kind,
        operations: plan.operations.clone(),
        directories_to_remove: plan.directories_to_remove.clone(),
        directories_to_create: plan.directories_to_create.clone(),
        created_at_utc: crate::now_utc_string(),
        updated_at_utc: crate::now_utc_string(),
    };
    let journal_path = journal_root.join("journal.json");
    write_journal(&journal_path, &journal)?;
    let prepare_result = (|| {
        let snapshot = load_project_snapshot(project, &plan.checkpoint_id)?;
        let snapshot_files = snapshot
            .files
            .iter()
            .map(|file| (file.path.clone(), file))
            .collect::<BTreeMap<_, _>>();
        let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
        let mut staged_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
        let mut destination_parents = BTreeSet::new();
        for operation in &plan.operations {
            if matches!(
                operation.operation_type,
                FileOperationType::Restore | FileOperationType::Replace
            ) {
                let staged_path = staged_path(&staged_root, &operation.path);
                let parent = staged_path.parent().ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "staged destination has no parent: {}",
                        staged_path.display()
                    ))
                })?;
                destination_parents.insert(parent.to_path_buf());
            }
        }
        // Prepare each unique destination directory serially. Workers only
        // open existing parents and therefore cannot race directory creation.
        for parent in destination_parents {
            crate::ensure_not_cancelled(cancellation)?;
            prepare_stage_destination_parent(
                project,
                &anchored_repo,
                &parent,
                &mut staged_sync_batch,
            )?;
        }
        anchored_repo.verify_root_binding()?;
        crate::ensure_not_cancelled(cancellation)?;

        let parallelism = transaction_stage_parallelism();
        for (group_index, operations) in plan.operations.chunks(parallelism).enumerate() {
            let group_start = group_index.saturating_mul(parallelism);
            let mut outcomes = operations
                .par_iter()
                .enumerate()
                .map(|(offset, operation)| {
                    stage_operation(
                        project,
                        &anchored_repo,
                        &snapshot_files,
                        &staged_root,
                        operation,
                        group_start.saturating_add(offset),
                        cancellation,
                    )
                })
                .collect::<Vec<_>>();
            outcomes.sort_by_key(|outcome| outcome.index);
            for outcome in outcomes {
                outcome.result?;
                staged_sync_batch.merge(outcome.sync_batch)?;
                let index = outcome.index;
                let operation = &plan.operations[index];
                report_operation_progress(
                    progress,
                    "staging",
                    index + 1,
                    plan.operations.len(),
                    Some(operation.path.to_string()),
                );
            }
            crate::ensure_not_cancelled(cancellation)?;
        }

        // A cancellation requested while a worker copies one large file is
        // observed at the next bounded group boundary and before Staged.
        crate::ensure_not_cancelled(cancellation)?;
        staged_sync_batch.flush()?;
        anchored_repo.verify_root_binding()?;
        journal.state = JournalState::Staged;
        journal.updated_at_utc = crate::now_utc_string();
        write_journal(&journal_path, &journal)?;
        recheck_preconditions(project, &plan)?;
        invalidate_operation_fingerprints(project, &plan.operations)?;
        journal.state = JournalState::Applying;
        journal.updated_at_utc = crate::now_utc_string();
        write_journal(&journal_path, &journal)
    })();
    if let Err(error) = prepare_result {
        journal.state = JournalState::Recovered;
        journal.updated_at_utc = crate::now_utc_string();
        if let Err(abort_error) = write_journal(&journal_path, &journal) {
            return Err(CheckPoError::Unexpected(format!(
                "{error}; additionally failed to abort transaction {transaction_id}: {abort_error}"
            )));
        }
        return Err(error);
    }

    inject_transaction_fault(fault_hook, TransactionFaultPoint::ApplyingJournalWritten)?;
    let backup_operations = plan
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.operation_type,
                FileOperationType::Delete | FileOperationType::Replace
            )
        })
        .collect::<Vec<_>>();
    let backup_total = backup_operations.len();
    let mut backup_completed = 0_usize;
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    for operations in backup_operations.chunks(TRANSACTION_BACKUP_FILE_BATCH_SIZE) {
        let mut backup_destination_sync_batch =
            crate::storage::AnchoredParentSyncBatch::with_max_pending(
                TRANSACTION_BACKUP_PARENT_MAX_PENDING,
            );
        let mut backup_source_sync_batch =
            crate::storage::AnchoredParentSyncBatch::with_max_pending(
                TRANSACTION_BACKUP_PARENT_MAX_PENDING,
            );
        let mut deferred_backup_sources = Vec::with_capacity(operations.len());
        for operation in operations {
            let backup_path = staged_path(&backup_root, &operation.path);
            report_operation_progress(
                progress,
                "backingUp",
                backup_completed,
                backup_total,
                Some(operation.path.to_string()),
            );
            if let Some(deferred) = backup_project_file_deferred(
                project,
                &anchored_project,
                &anchored_repo,
                operation,
                &backup_path,
                &mut backup_destination_sync_batch,
                &mut backup_source_sync_batch,
            )? {
                deferred_backup_sources.push(deferred);
            }
            backup_completed += 1;
            report_operation_progress(
                progress,
                "backingUp",
                backup_completed,
                backup_total,
                Some(operation.path.to_string()),
            );
            inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectFileBackedUp)?;
        }

        inject_transaction_fault(
            fault_hook,
            TransactionFaultPoint::BackupDirectoryBarrierBefore,
        )?;
        backup_destination_sync_batch.flush()?;
        inject_transaction_fault(
            fault_hook,
            TransactionFaultPoint::BackupDirectoryBarrierAfter,
        )?;
        inject_transaction_fault(fault_hook, TransactionFaultPoint::BackupSourceCleanupBefore)?;
        for deferred in deferred_backup_sources {
            remove_deferred_backup_source(
                &anchored_project,
                deferred,
                &mut backup_source_sync_batch,
            )?;
        }
        backup_source_sync_batch.flush()?;
        anchored_project.verify_root_binding()?;
        anchored_repo.verify_root_binding()?;
        inject_transaction_fault(fault_hook, TransactionFaultPoint::BackupSourceCleanupAfter)?;
    }
    for (index, directory) in plan.directories_to_remove.iter().enumerate() {
        report_operation_progress(
            progress,
            "removingDirectories",
            index,
            plan.directories_to_remove.len(),
            Some(directory.to_string()),
        );
        remove_project_directory(project, directory)?;
        report_operation_progress(
            progress,
            "removingDirectories",
            index + 1,
            plan.directories_to_remove.len(),
            Some(directory.to_string()),
        );
    }
    inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectDirectoriesRemoved)?;
    for (index, directory) in plan.directories_to_create.iter().enumerate() {
        report_operation_progress(
            progress,
            "creatingDirectories",
            index,
            plan.directories_to_create.len(),
            Some(directory.to_string()),
        );
        create_project_directory(project, directory)?;
        report_operation_progress(
            progress,
            "creatingDirectories",
            index + 1,
            plan.directories_to_create.len(),
            Some(directory.to_string()),
        );
    }
    inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectDirectoriesCreated)?;

    let mut project_restore_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    let mut staged_cleanup_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    let mut staged_sources_to_remove = Vec::new();
    let mut restore_rename_count = 0_usize;
    let mut restore_copy_fallback_count = 0_usize;
    let has_restore_operations = plan.operations.iter().any(|operation| {
        matches!(
            operation.operation_type,
            FileOperationType::Restore | FileOperationType::Replace
        )
    });
    for (index, operation) in plan.operations.iter().enumerate() {
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        match operation.operation_type {
            FileOperationType::Delete => {}
            FileOperationType::Restore => {
                let staged = staged_path(&staged_root, &operation.path);
                let staged_source_remains = restore_new_staged_file_to_project_deferred(
                    project,
                    operation,
                    &staged,
                    &destination,
                    &transaction_id,
                    &mut staged_cleanup_sync_batch,
                    &mut project_restore_sync_batch,
                )?;
                if staged_source_remains {
                    staged_sources_to_remove.push((
                        staged,
                        required_after_hash(operation)?.clone(),
                        operation.after_size_bytes.ok_or_else(|| {
                            CheckPoError::Corruption(format!(
                                "restore operation missing after size for {}",
                                operation.path
                            ))
                        })?,
                        operation.after_modified_at_utc.clone(),
                    ));
                    restore_copy_fallback_count += 1;
                } else {
                    restore_rename_count += 1;
                }
                inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectFileRestored)?;
            }
            FileOperationType::Replace => {
                let staged = staged_path(&staged_root, &operation.path);
                let staged_source_remains = restore_new_staged_file_to_project_deferred(
                    project,
                    operation,
                    &staged,
                    &destination,
                    &transaction_id,
                    &mut staged_cleanup_sync_batch,
                    &mut project_restore_sync_batch,
                )?;
                if staged_source_remains {
                    staged_sources_to_remove.push((
                        staged,
                        required_after_hash(operation)?.clone(),
                        operation.after_size_bytes.ok_or_else(|| {
                            CheckPoError::Corruption(format!(
                                "replace operation missing after size for {}",
                                operation.path
                            ))
                        })?,
                        operation.after_modified_at_utc.clone(),
                    ));
                    restore_copy_fallback_count += 1;
                } else {
                    restore_rename_count += 1;
                }
                inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectFileRestored)?;
            }
            FileOperationType::SetMetadata => {
                let target = operation.after_modified_at_utc.as_deref().ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "metadata operation missing target mtime for {}",
                        operation.path
                    ))
                })?;
                set_project_file_mtime(project, operation, target)?;
                inject_transaction_fault(
                    fault_hook,
                    TransactionFaultPoint::ProjectMetadataUpdated,
                )?;
            }
        }
        report_operation_progress(
            progress,
            "applying",
            index + 1,
            plan.operations.len(),
            Some(operation.path.to_string()),
        );
    }
    report_operation_progress(progress, "finalizing", 0, 0, None);
    if has_restore_operations {
        inject_transaction_fault(
            fault_hook,
            TransactionFaultPoint::ProjectRestoreDirectoryBarrierBefore,
        )?;
        project_restore_sync_batch.flush()?;
        inject_transaction_fault(
            fault_hook,
            TransactionFaultPoint::ProjectRestoreDirectoryBarrierAfter,
        )?;
        inject_transaction_fault(
            fault_hook,
            TransactionFaultPoint::StagedPayloadCleanupBefore,
        )?;
        let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
        for (staged, expected_hash, expected_size, expected_mtime) in staged_sources_to_remove {
            let relative = staged.strip_prefix(&project.repo_root).map_err(|_| {
                CheckPoError::Corruption(format!(
                    "staged cleanup path is outside repository {}: {}",
                    project.repo_root.display(),
                    staged.display()
                ))
            })?;
            let (parent, leaf) = anchored_repo.open_parent_for_mutation(relative, false)?;
            let mut file = parent.open_file(&leaf)?;
            let hashed = file.hash()?;
            if hashed.object_id != expected_hash || hashed.metadata.len() != expected_size {
                return Err(CheckPoError::ObjectHashMismatch(format!(
                    "staged cleanup source changed: {}",
                    staged.display()
                )));
            }
            verify_file_mtime(&hashed.metadata, &staged, expected_mtime.as_deref())?;
            parent.unlink_file_if_bound(&leaf, file)?;
            staged_cleanup_sync_batch.record(parent)?;
        }
        staged_cleanup_sync_batch.flush()?;
        anchored_repo.verify_root_binding()?;
        inject_transaction_fault(fault_hook, TransactionFaultPoint::StagedPayloadCleanupAfter)?;
        tracing::info!(
            transaction_id,
            restore_rename_count,
            restore_copy_fallback_count,
            "restore publication strategies completed"
        );
    }
    inject_transaction_fault(
        fault_hook,
        TransactionFaultPoint::OperationsAppliedBeforeCommit,
    )?;
    journal.state = JournalState::Committed;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&journal_path, &journal)?;
    let mut warnings = Vec::new();
    if let Some(checkpoint_id) = resolve_quarantines_for {
        if let Err(error) = super::recovery::resolve_unverified_transaction_quarantines_unlocked(
            project,
            checkpoint_id,
        ) {
            let warning = format!(
                "restore was committed, but transaction quarantine resolution failed and remains pending: {error}"
            );
            crate::diagnostics::log_warning("restore-quarantine-resolution", &warning);
            warnings.push(warning);
        }
    }
    report_operation_progress(progress, "complete", 1, 1, None);
    Ok(ApplyResult {
        checkpoint_id: plan.checkpoint_id.clone(),
        plan,
        applied: true,
        transaction_id: Some(transaction_id),
        journal_path: Some(journal_path),
        warnings,
    })
}

fn stage_operation(
    project: &ProjectContext,
    anchored_repo: &crate::storage::AnchoredRoot,
    snapshot_files: &BTreeMap<TrackedUnityFilePath, &crate::SnapshotEntry>,
    staged_root: &Path,
    operation: &FileOperation,
    index: usize,
    cancellation: Option<&CancellationToken>,
) -> StagingOperationOutcome {
    let mut sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    let result = if matches!(
        operation.operation_type,
        FileOperationType::Restore | FileOperationType::Replace
    ) {
        (|| {
            let file = snapshot_files.get(&operation.path).ok_or_else(|| {
                CheckPoError::Corruption(format!("snapshot entry missing for {}", operation.path))
            })?;
            let staged_path = staged_path(staged_root, &operation.path);
            stage_object_for_transaction_prepared(
                project,
                anchored_repo,
                file.content_hash(),
                &staged_path,
                file.content_size_bytes(),
                operation.after_modified_at_utc.as_deref(),
                &mut sync_batch,
                cancellation,
            )
        })()
    } else {
        Ok(())
    };
    StagingOperationOutcome {
        index,
        result,
        sync_batch,
    }
}

fn transaction_stage_parallelism() -> usize {
    select_transaction_stage_parallelism(
        std::env::var("CHECKPO_TRANSACTION_STAGE_CONCURRENCY")
            .ok()
            .as_deref(),
        std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1),
    )
}

fn select_transaction_stage_parallelism(configured: Option<&str>, available: usize) -> usize {
    configured
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=16).contains(value))
        .unwrap_or_else(|| available.clamp(1, DEFAULT_TRANSACTION_STAGE_CONCURRENCY))
}

fn inject_transaction_fault(
    fault_hook: TransactionFaultHook<'_>,
    point: TransactionFaultPoint,
) -> Result<()> {
    if let Some(hook) = fault_hook {
        hook(point)?;
    }
    Ok(())
}

fn ensure_capacity_for_plan(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    ensure_available_space(
        "checkpoint storage",
        project.repo_root.as_path(),
        estimated_repository_required_bytes(plan),
    )?;
    ensure_available_space(
        "Unity project",
        project.project_root.as_path(),
        estimated_project_required_bytes(plan),
    )
}

pub(super) fn estimated_repository_required_bytes(plan: &OperationPlan) -> u64 {
    plan.staged_bytes.saturating_add(plan.backup_bytes)
}

pub(super) fn estimated_project_required_bytes(plan: &OperationPlan) -> u64 {
    plan.operations
        .iter()
        .map(|operation| match operation.operation_type {
            FileOperationType::Restore => operation.after_size_bytes.unwrap_or(0),
            FileOperationType::Replace => operation
                .after_size_bytes
                .unwrap_or(0)
                .saturating_sub(operation.before_size_bytes.unwrap_or(0)),
            FileOperationType::Delete => 0,
            FileOperationType::SetMetadata => 0,
        })
        .sum()
}

pub(super) fn ensure_available_space(label: &str, path: &Path, required_bytes: u64) -> Result<()> {
    if required_bytes == 0 {
        return Ok(());
    }
    let available_bytes = crate::available_space_bytes(path)?;
    if available_bytes < required_bytes {
        return Err(crate::user_error(format!(
            "not enough free space in {label}: need {}, available {} ({})",
            format_bytes(required_bytes),
            format_bytes(available_bytes),
            path.display()
        )));
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    }
}

#[cfg(test)]
mod staging_parallelism_tests {
    use super::select_transaction_stage_parallelism;

    #[test]
    fn staging_parallelism_accepts_only_bounded_overrides() {
        assert_eq!(select_transaction_stage_parallelism(Some("1"), 32), 1);
        assert_eq!(select_transaction_stage_parallelism(Some("16"), 2), 16);
        assert_eq!(select_transaction_stage_parallelism(Some("0"), 32), 8);
        assert_eq!(select_transaction_stage_parallelism(Some("17"), 4), 4);
        assert_eq!(select_transaction_stage_parallelism(Some("invalid"), 0), 1);
        assert_eq!(select_transaction_stage_parallelism(None, 64), 8);
    }
}
