use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransactionFaultPoint {
    ApplyingJournalWritten,
    ProjectFileBackedUp,
    ProjectDirectoriesRemoved,
    ProjectDirectoriesCreated,
    ProjectFileRestored,
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
    if !options.yes {
        return Err(crate::user_error("apply requires --yes."));
    }
    crate::ensure_project_location_allows_mutation(project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "transaction-apply")?;
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
        return Ok(ApplyResult {
            checkpoint_id: plan.checkpoint_id.clone(),
            plan,
            applied: false,
            transaction_id: None,
            journal_path: None,
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
    let backup_copy_mode = backup_copy_mode(project);
    let snapshot = load_project_snapshot(project, &plan.checkpoint_id)?;
    let snapshot_files = snapshot
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let mut staged_directories = BTreeSet::new();
    for (index, operation) in plan.operations.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        if matches!(
            operation.operation_type,
            FileOperationType::Restore | FileOperationType::Replace
        ) {
            let file = snapshot_files.get(&operation.path).ok_or_else(|| {
                CheckPoError::Corruption(format!("snapshot entry missing for {}", operation.path))
            })?;
            let staged_path = staged_path(&staged_root, &operation.path);
            copy_object_to_file(
                &project.repo_root,
                file.content_hash(),
                &staged_path,
                file.content_size_bytes(),
            )?;
            collect_parent_directories(&mut staged_directories, &staged_path, &staged_root)?;
        }
        report_operation_progress(
            progress,
            "staging",
            index + 1,
            plan.operations.len(),
            Some(operation.path.to_string()),
        );
    }
    sync_directories(&staged_directories)?;
    journal.state = JournalState::Staged;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&journal_path, &journal)?;
    recheck_preconditions(project, &plan)?;
    invalidate_operation_fingerprints(project, &plan.operations)?;
    journal.state = JournalState::Applying;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&journal_path, &journal)?;
    inject_transaction_fault(fault_hook, TransactionFaultPoint::ApplyingJournalWritten)?;
    for operation in &plan.operations {
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        let backup_path = staged_path(&backup_root, &operation.path);
        if matches!(
            operation.operation_type,
            FileOperationType::Delete | FileOperationType::Replace
        ) {
            ensure_project_parent_is_safe(project, &operation.path)?;
            backup_project_file(
                project,
                operation,
                &destination,
                &backup_path,
                backup_copy_mode,
            )?;
            inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectFileBackedUp)?;
        }
    }
    for directory in &plan.directories_to_remove {
        remove_project_directory(project, directory)?;
    }
    inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectDirectoriesRemoved)?;
    for directory in &plan.directories_to_create {
        create_project_directory(project, directory)?;
    }
    inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectDirectoriesCreated)?;

    let mut restored_directories = BTreeSet::new();
    for (index, operation) in plan.operations.iter().enumerate() {
        let destination = operation
            .path
            .to_project_path(project.project_root.as_path());
        match operation.operation_type {
            FileOperationType::Delete => {}
            FileOperationType::Restore | FileOperationType::Replace => {
                let staged = staged_path(&staged_root, &operation.path);
                restore_staged_file_to_project(project, operation, &staged, &destination)?;
                restore_mtime(&destination, operation.after_modified_at_utc.as_deref())?;
                sync_project_file(&destination)?;
                if let Some(parent) = destination.parent() {
                    restored_directories.insert(parent.to_path_buf());
                }
                inject_transaction_fault(fault_hook, TransactionFaultPoint::ProjectFileRestored)?;
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
    sync_directories(&restored_directories)?;
    inject_transaction_fault(
        fault_hook,
        TransactionFaultPoint::OperationsAppliedBeforeCommit,
    )?;
    journal.state = JournalState::Committed;
    journal.updated_at_utc = crate::now_utc_string();
    write_journal(&journal_path, &journal)?;
    report_operation_progress(progress, "complete", 1, 1, None);
    Ok(ApplyResult {
        checkpoint_id: plan.checkpoint_id.clone(),
        plan,
        applied: true,
        transaction_id: Some(transaction_id),
        journal_path: Some(journal_path),
    })
}

fn collect_parent_directories(
    directories: &mut BTreeSet<PathBuf>,
    path: &Path,
    stop_at: &Path,
) -> Result<()> {
    if !path.starts_with(stop_at) {
        return Err(CheckPoError::Unexpected(format!(
            "cannot collect directories outside {}: {}",
            stop_at.display(),
            path.display()
        )));
    }
    let mut current = path.parent();
    while let Some(directory) = current {
        directories.insert(directory.to_path_buf());
        if directory == stop_at {
            return Ok(());
        }
        current = directory.parent();
    }
    Err(CheckPoError::Unexpected(format!(
        "directory chain did not reach {} from {}",
        stop_at.display(),
        path.display()
    )))
}

#[cfg(not(windows))]
fn sync_directories(directories: &BTreeSet<PathBuf>) -> Result<()> {
    let mut ordered = directories.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for directory in ordered {
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| crate::io_error(directory, error))?;
    }
    Ok(())
}

#[cfg(windows)]
fn sync_directories(_directories: &BTreeSet<PathBuf>) -> Result<()> {
    Ok(())
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
