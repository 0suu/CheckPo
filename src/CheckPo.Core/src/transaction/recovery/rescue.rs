use super::*;

fn ensure_recovery_rescue_capacity(
    project: &ProjectContext,
    plan: &TransactionRecoveryConflictPlan,
) -> Result<()> {
    let root = project
        .repo_root
        .join(recovery_rescue_files_relative_for_transaction(
            &plan.transaction_id,
        ));
    let mut required_bytes = 0_u64;
    let mut seen = BTreeSet::new();
    for conflict in plan
        .conflicts
        .iter()
        .filter(|conflict| !conflict.metadata_only)
    {
        if !seen.insert(conflict.current_hash.clone()) {
            continue;
        }
        let path = root.join(conflict.current_hash.as_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "recovery rescue object is unsafe: {}",
                    path.display()
                )))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                required_bytes =
                    required_bytes
                        .checked_add(conflict.size_bytes)
                        .ok_or_else(|| {
                            CheckPoError::Corruption("recovery rescue size overflow".to_string())
                        })?;
            }
            Err(error) => return Err(crate::io_error(&path, error)),
        }
    }
    super::apply::ensure_checkpoint_storage_available_space(&project.repo_root, required_bytes)
}

pub(super) fn prepare_recovery_conflict_rescue(
    project: &ProjectContext,
    journal: &TransactionJournal,
    plan: &TransactionRecoveryConflictPlan,
    selected: &BTreeSet<TrackedUnityFilePath>,
    export_directory: Option<&Path>,
) -> Result<()> {
    if journal.transaction_id != plan.transaction_id || journal.checkpoint_id != plan.checkpoint_id
    {
        return Err(CheckPoError::WorkingTreeChanged(
            "recovery transaction changed while preparing rescue data".to_string(),
        ));
    }
    ensure_recovery_rescue_capacity(project, plan)?;
    let rescue_files_root = project
        .repo_root
        .join(recovery_rescue_files_relative_for_transaction(
            &plan.transaction_id,
        ));
    let before_paths = journal_before_paths(&journal.operations);
    for conflict in &plan.conflicts {
        verify_recovery_conflict_is_current(project, conflict, &before_paths)?;
        if !conflict.metadata_only {
            let source = conflict
                .path
                .to_project_path(project.project_root.as_path());
            preserve_after_file_for_recovery(
                project,
                &source,
                &conflict.current_hash,
                &rescue_files_root,
            )?;
        }
    }
    // Publish the durable path-to-copy mapping only after every private copy
    // and any user-visible export have been fully verified.
    for conflict in &plan.conflicts {
        verify_recovery_conflict_is_current(project, conflict, &before_paths)?;
    }
    write_recovery_rescue_record(
        project,
        &RecoveryRescueRecord {
            schema_version: RECOVERY_RESCUE_RECORD_SCHEMA_VERSION,
            transaction_id: plan.transaction_id.clone(),
            checkpoint_id: plan.checkpoint_id.clone(),
            plan_id: plan.plan_id.clone(),
            created_at_utc: crate::now_utc_string(),
            state: RecoveryRescueState::Prepared,
            export_directory: export_directory.map(Path::to_path_buf),
            entries: plan
                .conflicts
                .iter()
                .cloned()
                .map(|conflict| RecoveryRescueEntry {
                    exported: selected.contains(&conflict.path),
                    conflict,
                })
                .collect(),
            completed_paths: Vec::new(),
        },
    )
}

#[cfg(test)]
pub(in crate::transaction) fn prepare_recovery_conflict_rescue_for_test(
    project: &ProjectContext,
    plan: &TransactionRecoveryConflictPlan,
) -> Result<()> {
    let pending = pending_transaction_by_id(project, &plan.transaction_id)?;
    let journal = read_valid_recovery_journal(project, &pending)?;
    prepare_recovery_conflict_rescue(project, &journal, plan, &BTreeSet::new(), None)
}

#[cfg(test)]
pub(in crate::transaction) fn prepare_recovery_conflict_rescue_and_remove_first_for_test(
    project: &ProjectContext,
    plan: &TransactionRecoveryConflictPlan,
) -> Result<()> {
    prepare_recovery_conflict_rescue_for_test(project, plan)?;
    let conflict = plan
        .conflicts
        .iter()
        .find(|conflict| !conflict.metadata_only)
        .ok_or_else(|| CheckPoError::Unexpected("test plan has no content conflict".to_string()))?;
    let source = conflict
        .path
        .to_project_path(project.project_root.as_path());
    remove_anchored_project_file(project, &source, &conflict.current_hash)
}

fn verify_recovery_conflict_is_current(
    project: &ProjectContext,
    conflict: &TransactionRecoveryConflict,
    before_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<()> {
    let current = current_file_state_for_recovery(project, &conflict.path, before_paths)?
        .ok_or_else(|| CheckPoError::WorkingTreeChanged(conflict.path.to_string()))?;
    if current.hash != conflict.current_hash
        || current.size_bytes != conflict.size_bytes
        || current.modified_at_utc != conflict.modified_at_utc
    {
        return Err(CheckPoError::WorkingTreeChanged(conflict.path.to_string()));
    }
    Ok(())
}

fn recovery_rescue_record_relative(record: &RecoveryRescueRecord) -> PathBuf {
    Path::new("recovery-rescues")
        .join(&record.transaction_id)
        .join("records")
        .join(format!("{}.json", record.plan_id))
}

fn recovery_rescue_files_relative(record: &RecoveryRescueRecord) -> PathBuf {
    recovery_rescue_files_relative_for_transaction(&record.transaction_id)
}

fn recovery_rescue_files_relative_for_transaction(transaction_id: &str) -> PathBuf {
    Path::new("recovery-rescues")
        .join(transaction_id)
        .join("objects")
}

fn recovery_rescue_active_relative(transaction_id: &str) -> PathBuf {
    Path::new("recovery-rescues")
        .join(transaction_id)
        .join("active.json")
}

fn write_recovery_rescue_record(
    project: &ProjectContext,
    record: &RecoveryRescueRecord,
) -> Result<()> {
    validate_transaction_id(&record.transaction_id)?;
    validate_recovery_conflict_plan_id(&record.plan_id)?;
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    repo.write_json_atomic(&recovery_rescue_record_relative(record), record)?;
    repo.write_json_atomic(
        &recovery_rescue_active_relative(&record.transaction_id),
        record,
    )
}

fn read_active_recovery_rescue_record(
    project: &ProjectContext,
    transaction_id: &str,
) -> Result<Option<RecoveryRescueRecord>> {
    let path = project
        .repo_root
        .join(recovery_rescue_active_relative(transaction_id));
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(&path, error)),
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            return Err(CheckPoError::Corruption(format!(
                "recovery rescue record is not a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => {}
    }
    let bytes = crate::storage::AnchoredRoot::open(&project.repo_root)?
        .read_bytes_bounded_path(&path, MAX_RECOVERY_RESCUE_RECORD_BYTES)?;
    let record: RecoveryRescueRecord =
        serde_json::from_slice(&bytes).map_err(|error| crate::json_error(&path, error))?;
    validate_recovery_rescue_record(&record, transaction_id)?;
    Ok(Some(record))
}

fn validate_recovery_rescue_record(
    record: &RecoveryRescueRecord,
    transaction_id: &str,
) -> Result<()> {
    if record.schema_version != RECOVERY_RESCUE_RECORD_SCHEMA_VERSION {
        return Err(CheckPoError::Corruption(format!(
            "unsupported recovery rescue schema version: {}",
            record.schema_version
        )));
    }
    validate_transaction_id(&record.transaction_id)?;
    validate_recovery_conflict_plan_id(&record.plan_id)?;
    if record.transaction_id != transaction_id || record.entries.is_empty() {
        return Err(CheckPoError::Corruption(
            "recovery rescue record identity is invalid".to_string(),
        ));
    }
    let entry_paths = record
        .entries
        .iter()
        .map(|entry| entry.conflict.path.clone())
        .collect::<BTreeSet<_>>();
    if entry_paths.len() != record.entries.len()
        || record
            .completed_paths
            .iter()
            .any(|path| !entry_paths.contains(path))
        || record.completed_paths.iter().collect::<BTreeSet<_>>().len()
            != record.completed_paths.len()
    {
        return Err(CheckPoError::Corruption(
            "recovery rescue record contains invalid paths".to_string(),
        ));
    }
    if record
        .export_directory
        .as_deref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(CheckPoError::Corruption(
            "recovery rescue export path is not absolute".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn recover_one_with_active_rescue(
    project: &ProjectContext,
    pending: &PendingTransaction,
) -> Result<()> {
    let Some(mut rescue) = read_active_recovery_rescue_record(project, &pending.transaction_id)?
    else {
        return recover_one(project, pending);
    };
    if rescue.state == RecoveryRescueState::Recovered {
        return recover_one(project, pending);
    }
    let journal = read_valid_recovery_journal(project, pending)?;
    resolve_recovery_conflict_rescue(project, &journal, &mut rescue)?;
    recover_one(project, pending)?;
    rescue.state = RecoveryRescueState::Recovered;
    write_recovery_rescue_record(project, &rescue)
}

fn resolve_recovery_conflict_rescue(
    project: &ProjectContext,
    journal: &TransactionJournal,
    rescue: &mut RecoveryRescueRecord,
) -> Result<()> {
    if rescue.transaction_id != journal.transaction_id
        || rescue.checkpoint_id != journal.checkpoint_id
    {
        return Err(CheckPoError::Corruption(
            "recovery rescue record does not match its transaction journal".to_string(),
        ));
    }
    verify_recovery_rescue_payload(project, rescue)?;
    rescue.state = RecoveryRescueState::Resolving;
    write_recovery_rescue_record(project, rescue)?;
    let before_paths = journal_before_paths(&journal.operations);
    for entry in rescue.entries.clone() {
        if rescue.completed_paths.contains(&entry.conflict.path) {
            continue;
        }
        let operation = journal
            .operations
            .iter()
            .find(|operation| operation.path == entry.conflict.path)
            .ok_or_else(|| {
                CheckPoError::Corruption(format!(
                    "recovery conflict operation is missing for {}",
                    entry.conflict.path
                ))
            })?;
        let current =
            current_file_state_for_recovery(project, &entry.conflict.path, &before_paths)?;
        let matches_before = current.as_ref().map(|state| &state.hash)
            == operation.before_hash.as_ref()
            && current.as_ref().map(|state| state.size_bytes) == operation.before_size_bytes
            && current.as_ref().map(|state| state.modified_at_utc.as_str())
                == operation.before_modified_at_utc.as_deref();
        let matches_rescued = current.as_ref().is_some_and(|state| {
            state.hash == entry.conflict.current_hash
                && state.size_bytes == entry.conflict.size_bytes
                && state.modified_at_utc == entry.conflict.modified_at_utc
        });
        if entry.conflict.metadata_only {
            if !matches_before {
                if !matches_rescued {
                    return Err(CheckPoError::WorkingTreeChanged(
                        entry.conflict.path.to_string(),
                    ));
                }
                restore_before_mtime_for_recovery(project, operation)?;
            }
        } else if !matches_before {
            if current.is_none() {
                // A crash may occur after the identity-bound unlink and before
                // the completion record update. The durable rescue copy makes
                // treating that state as completed safe and repeatable.
            } else if matches_rescued {
                if journal.kind == OperationPlanKind::Discard {
                    super::plan::ensure_discard_folder_meta_operation_is_safe(
                        project,
                        &operation.path,
                    )?;
                }
                let source = entry
                    .conflict
                    .path
                    .to_project_path(project.project_root.as_path());
                remove_anchored_project_file(project, &source, &entry.conflict.current_hash)?;
                if journal.kind == OperationPlanKind::Discard {
                    super::plan::ensure_discard_folder_meta_operation_is_safe(
                        project,
                        &operation.path,
                    )?;
                }
            } else {
                return Err(CheckPoError::WorkingTreeChanged(
                    entry.conflict.path.to_string(),
                ));
            }
        }
        rescue.completed_paths.push(entry.conflict.path);
        rescue.completed_paths.sort();
        write_recovery_rescue_record(project, rescue)?;
    }
    Ok(())
}

fn verify_recovery_rescue_payload(
    project: &ProjectContext,
    rescue: &RecoveryRescueRecord,
) -> Result<()> {
    let repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let files_root = recovery_rescue_files_relative(rescue);
    for entry in rescue
        .entries
        .iter()
        .filter(|entry| !entry.conflict.metadata_only)
    {
        let relative = files_root.join(entry.conflict.current_hash.as_str());
        let (parent, leaf) = repo.open_parent(&relative, false)?;
        let mut file = parent.open_file(&leaf).map_err(|error| {
            CheckPoError::Corruption(format!(
                "recovery rescue copy is unavailable for {}: {error}",
                entry.conflict.path
            ))
        })?;
        let hashed = file.hash()?;
        if hashed.object_id != entry.conflict.current_hash
            || hashed.metadata.len() != entry.conflict.size_bytes
        {
            return Err(CheckPoError::Corruption(format!(
                "recovery rescue copy is damaged for {}",
                entry.conflict.path
            )));
        }
        parent.verify_file_binding(&leaf, &file)?;
    }
    repo.verify_root_binding()
}
