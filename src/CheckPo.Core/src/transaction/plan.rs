use super::*;

pub fn build_plan(
    project: &ProjectContext,
    checkpoint_id: SnapshotId,
    kind: OperationPlanKind,
    selected: Option<&[TrackedUnityFilePath]>,
) -> Result<OperationPlan> {
    build_plan_with_progress_and_cancellation(project, checkpoint_id, kind, selected, None, None)
}

pub fn build_plan_with_progress_and_cancellation(
    project: &ProjectContext,
    checkpoint_id: SnapshotId,
    kind: OperationPlanKind,
    selected: Option<&[TrackedUnityFilePath]>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<OperationPlan> {
    crate::ensure_not_cancelled(cancellation)?;
    let snapshot = load_project_snapshot(project, &checkpoint_id)?;
    let snapshot_map = snapshot
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let mut operations = Vec::new();
    let mut warnings = Vec::new();
    match kind {
        OperationPlanKind::Restore => {
            let (working, scan_warnings, _) =
                crate::scan_project_for_checkpoint(project, progress, cancellation)?;
            warnings.extend(
                scan_warnings
                    .iter()
                    .map(crate::scanner::format_scan_warning),
            );
            let working_map = working
                .into_iter()
                .map(|file| {
                    (
                        file.path,
                        CurrentFileState {
                            hash: file.hash,
                            size_bytes: file.size_bytes,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let total = snapshot.files.len() + working_map.len();
            for (index, file) in snapshot.files.iter().enumerate() {
                crate::ensure_not_cancelled(cancellation)?;
                ensure_project_parent_is_safe(project, &file.path)?;
                match working_map.get(&file.path) {
                    None => operations.push(FileOperation {
                        operation_type: FileOperationType::Restore,
                        path: file.path.clone(),
                        before_hash: None,
                        before_size_bytes: None,
                        after_hash: Some(file.content_hash().clone()),
                        after_size_bytes: Some(file.content_size_bytes()),
                        after_modified_at_utc: Some(file.modified_at_utc.clone()),
                    }),
                    Some(current) if &current.hash != file.content_hash() => {
                        operations.push(FileOperation {
                            operation_type: FileOperationType::Replace,
                            path: file.path.clone(),
                            before_hash: Some(current.hash.clone()),
                            before_size_bytes: Some(current.size_bytes),
                            after_hash: Some(file.content_hash().clone()),
                            after_size_bytes: Some(file.content_size_bytes()),
                            after_modified_at_utc: Some(file.modified_at_utc.clone()),
                        })
                    }
                    Some(_) => {}
                }
                report_operation_progress(
                    progress,
                    "planning",
                    index + 1,
                    total,
                    Some(file.path.to_string()),
                );
            }
            for (offset, (path, current)) in working_map.into_iter().enumerate() {
                crate::ensure_not_cancelled(cancellation)?;
                let current_item = path.to_string();
                if !snapshot_map.contains_key(&path) {
                    operations.push(FileOperation {
                        operation_type: FileOperationType::Delete,
                        before_hash: Some(current.hash),
                        before_size_bytes: Some(current.size_bytes),
                        path,
                        after_hash: None,
                        after_size_bytes: None,
                        after_modified_at_utc: None,
                    });
                }
                report_operation_progress(
                    progress,
                    "planning",
                    snapshot.files.len() + offset + 1,
                    total,
                    Some(current_item),
                );
            }
        }
        OperationPlanKind::Discard => {
            let selected = selected.ok_or_else(|| {
                CheckPoError::InvalidTrackedPath(
                    "discard requires selected tracked paths".to_string(),
                )
            })?;
            let selected_paths = selected.iter().cloned().collect::<BTreeSet<_>>();
            let total = selected_paths.len();
            for (index, path) in selected_paths.iter().enumerate() {
                crate::ensure_not_cancelled(cancellation)?;
                let current = current_file_state(project, path)?;
                match snapshot_map.get(path) {
                    Some(file) => match current {
                        None => operations.push(FileOperation {
                            operation_type: FileOperationType::Restore,
                            path: path.clone(),
                            before_hash: None,
                            before_size_bytes: None,
                            after_hash: Some(file.content_hash().clone()),
                            after_size_bytes: Some(file.content_size_bytes()),
                            after_modified_at_utc: Some(file.modified_at_utc.clone()),
                        }),
                        Some(current) if &current.hash != file.content_hash() => {
                            operations.push(FileOperation {
                                operation_type: FileOperationType::Replace,
                                path: path.clone(),
                                before_hash: Some(current.hash),
                                before_size_bytes: Some(current.size_bytes),
                                after_hash: Some(file.content_hash().clone()),
                                after_size_bytes: Some(file.content_size_bytes()),
                                after_modified_at_utc: Some(file.modified_at_utc.clone()),
                            })
                        }
                        Some(_) => {}
                    },
                    None => {
                        if let Some(current) = current {
                            operations.push(FileOperation {
                                operation_type: FileOperationType::Delete,
                                path: path.clone(),
                                before_hash: Some(current.hash),
                                before_size_bytes: Some(current.size_bytes),
                                after_hash: None,
                                after_size_bytes: None,
                                after_modified_at_utc: None,
                            });
                        }
                    }
                }
                report_operation_progress(
                    progress,
                    "planning",
                    index + 1,
                    total,
                    Some(path.to_string()),
                );
            }
        }
    }
    Ok(OperationPlan::new(
        checkpoint_id,
        kind,
        selected.map(|paths| {
            paths
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        }),
        operations,
    )
    .with_warnings(warnings))
}

pub(super) fn validate_expected_plan(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    validate_plan_shape(project, plan)?;
    let current = build_plan(
        project,
        plan.checkpoint_id.clone(),
        plan.kind,
        plan.selected_paths.as_deref(),
    )?;
    if !current.warnings.is_empty() {
        return Err(crate::user_error(format!(
            "operation cannot be applied while scan warnings exist: {}",
            current.warnings.join("; ")
        )));
    }
    if &current != plan {
        return Err(CheckPoError::WorkingTreeChanged(
            "operation plan changed after preview".to_string(),
        ));
    }
    Ok(())
}

fn validate_plan_shape(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    if !plan.warnings.is_empty() {
        return Err(CheckPoError::Corruption(
            "operation plan contains scan warnings and cannot be applied".to_string(),
        ));
    }
    let mut sorted = plan.operations.clone();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    if sorted != plan.operations {
        return Err(CheckPoError::Corruption(
            "operation plan is not sorted".to_string(),
        ));
    }
    if sorted
        .windows(2)
        .any(|window| window[0].path == window[1].path)
    {
        return Err(CheckPoError::Corruption(
            "operation plan contains duplicate paths".to_string(),
        ));
    }
    if plan.restore_count
        != plan
            .operations
            .iter()
            .filter(|operation| operation.operation_type == FileOperationType::Restore)
            .count()
        || plan.replace_count
            != plan
                .operations
                .iter()
                .filter(|operation| operation.operation_type == FileOperationType::Replace)
                .count()
        || plan.delete_count
            != plan
                .operations
                .iter()
                .filter(|operation| operation.operation_type == FileOperationType::Delete)
                .count()
        || plan.has_changes == plan.operations.is_empty()
    {
        return Err(CheckPoError::Corruption(
            "operation plan counts are inconsistent".to_string(),
        ));
    }
    let expected_staged_bytes = plan
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.operation_type,
                FileOperationType::Restore | FileOperationType::Replace
            )
        })
        .filter_map(|operation| operation.after_size_bytes)
        .sum::<u64>();
    let expected_backup_bytes = plan
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.operation_type,
                FileOperationType::Delete | FileOperationType::Replace
            )
        })
        .filter_map(|operation| operation.before_size_bytes)
        .sum::<u64>();
    if plan.staged_bytes != expected_staged_bytes
        || plan.backup_bytes != expected_backup_bytes
        || plan.estimated_temporary_bytes != expected_staged_bytes + expected_backup_bytes
    {
        return Err(CheckPoError::Corruption(
            "operation plan byte estimates are inconsistent".to_string(),
        ));
    }
    match plan.kind {
        OperationPlanKind::Restore if plan.selected_paths.is_some() => {
            return Err(CheckPoError::Corruption(
                "restore plan must not include selected paths".to_string(),
            ));
        }
        OperationPlanKind::Discard if plan.selected_paths.is_none() => {
            return Err(CheckPoError::Corruption(
                "discard plan requires selected paths".to_string(),
            ));
        }
        _ => {}
    }

    let snapshot = load_project_snapshot(project, &plan.checkpoint_id)?;
    let snapshot_files = snapshot
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    for operation in &plan.operations {
        match operation.operation_type {
            FileOperationType::Restore => {
                let file = snapshot_files.get(&operation.path).ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "restore operation missing snapshot entry for {}",
                        operation.path
                    ))
                })?;
                if operation.before_hash.is_some()
                    || operation.before_size_bytes.is_some()
                    || operation.after_hash.as_ref() != Some(file.content_hash())
                    || operation.after_size_bytes != Some(file.content_size_bytes())
                    || operation.after_modified_at_utc.as_deref()
                        != Some(file.modified_at_utc.as_str())
                {
                    return Err(CheckPoError::Corruption(format!(
                        "invalid restore operation for {}",
                        operation.path
                    )));
                }
            }
            FileOperationType::Replace => {
                let file = snapshot_files.get(&operation.path).ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "replace operation missing snapshot entry for {}",
                        operation.path
                    ))
                })?;
                if operation.before_hash.is_none()
                    || operation.before_size_bytes.is_none()
                    || operation.after_hash.as_ref() != Some(file.content_hash())
                    || operation.after_size_bytes != Some(file.content_size_bytes())
                    || operation.after_modified_at_utc.as_deref()
                        != Some(file.modified_at_utc.as_str())
                {
                    return Err(CheckPoError::Corruption(format!(
                        "invalid replace operation for {}",
                        operation.path
                    )));
                }
            }
            FileOperationType::Delete => {
                if operation.before_hash.is_none()
                    || operation.before_size_bytes.is_none()
                    || operation.after_hash.is_some()
                    || operation.after_size_bytes.is_some()
                    || operation.after_modified_at_utc.is_some()
                    || snapshot_files.contains_key(&operation.path)
                {
                    return Err(CheckPoError::Corruption(format!(
                        "invalid delete operation for {}",
                        operation.path
                    )));
                }
            }
        }
    }
    Ok(())
}
