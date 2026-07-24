use super::*;

const OPERATION_PLAN_AUTH_KEY_PATH: &str = "locks/operation-plan-auth.key";
const OPERATION_PLAN_AUTH_KEY_BYTES: u64 = 64;

pub fn build_plan_with_progress_and_cancellation(
    project: &ProjectContext,
    checkpoint_id: SnapshotId,
    kind: OperationPlanKind,
    selected: Option<&[TrackedUnityFilePath]>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<OperationPlan> {
    let mut plan = build_unsigned_plan_with_progress_and_cancellation(
        project,
        checkpoint_id,
        kind,
        selected,
        progress,
        cancellation,
    )?;
    authorize_operation_plan(project, &mut plan)?;
    Ok(plan)
}

fn build_unsigned_plan_with_progress_and_cancellation(
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
    let effective_selected = match (kind, selected) {
        (OperationPlanKind::Discard, Some(selected)) => Some(
            normalize_discard_selection_with_snapshot(project, snapshot_map.keys(), selected)?,
        ),
        _ => None,
    };
    let mut operations = Vec::new();
    let mut warnings = Vec::new();
    match kind {
        OperationPlanKind::Restore => {
            let (working, scan_warnings, _) = crate::scan_project_for_checkpoint_with_baseline(
                project,
                Some(&snapshot),
                progress,
                cancellation,
            )?;
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
                            modified_at_utc: file.modified_at_utc,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let total = snapshot.files.len() + working_map.len();
            for (index, file) in snapshot.files.iter().enumerate() {
                crate::ensure_not_cancelled(cancellation)?;
                match working_map.get(&file.path) {
                    None => operations.push(FileOperation {
                        operation_type: FileOperationType::Restore,
                        path: file.path.clone(),
                        before_hash: None,
                        before_size_bytes: None,
                        before_modified_at_utc: None,
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
                            before_modified_at_utc: Some(current.modified_at_utc.clone()),
                            after_hash: Some(file.content_hash().clone()),
                            after_size_bytes: Some(file.content_size_bytes()),
                            after_modified_at_utc: Some(file.modified_at_utc.clone()),
                        })
                    }
                    Some(current) if current.modified_at_utc != file.modified_at_utc => operations
                        .push(FileOperation {
                            operation_type: FileOperationType::SetMetadata,
                            path: file.path.clone(),
                            before_hash: Some(current.hash.clone()),
                            before_size_bytes: Some(current.size_bytes),
                            before_modified_at_utc: Some(current.modified_at_utc.clone()),
                            after_hash: Some(file.content_hash().clone()),
                            after_size_bytes: Some(file.content_size_bytes()),
                            after_modified_at_utc: Some(file.modified_at_utc.clone()),
                        }),
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
                        before_modified_at_utc: Some(current.modified_at_utc),
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
            let selected = effective_selected.as_deref().ok_or_else(|| {
                CheckPoError::InvalidTrackedPath(
                    "discard requires selected tracked paths".to_string(),
                )
            })?;
            let selected_paths = selected.iter().cloned().collect::<BTreeSet<_>>();
            let total = selected_paths.len();
            for (index, path) in selected_paths.iter().enumerate() {
                crate::ensure_not_cancelled(cancellation)?;
                let snapshot_file = snapshot_map.get(path);
                let current = current_file_state_for_discard(
                    project,
                    path,
                    snapshot_file.is_some(),
                    &selected_paths,
                )?;
                match snapshot_file {
                    Some(file) => match current {
                        None => operations.push(FileOperation {
                            operation_type: FileOperationType::Restore,
                            path: path.clone(),
                            before_hash: None,
                            before_size_bytes: None,
                            before_modified_at_utc: None,
                            after_hash: Some(file.content_hash().clone()),
                            after_size_bytes: Some(file.content_size_bytes()),
                            after_modified_at_utc: Some(file.modified_at_utc.clone()),
                        }),
                        Some(current) if &current.hash != file.content_hash() => {
                            operations.push(FileOperation {
                                operation_type: FileOperationType::Replace,
                                path: path.clone(),
                                before_hash: Some(current.hash.clone()),
                                before_size_bytes: Some(current.size_bytes),
                                before_modified_at_utc: Some(current.modified_at_utc.clone()),
                                after_hash: Some(file.content_hash().clone()),
                                after_size_bytes: Some(file.content_size_bytes()),
                                after_modified_at_utc: Some(file.modified_at_utc.clone()),
                            })
                        }
                        Some(current) if current.modified_at_utc != file.modified_at_utc => {
                            operations.push(FileOperation {
                                operation_type: FileOperationType::SetMetadata,
                                path: path.clone(),
                                before_hash: Some(current.hash),
                                before_size_bytes: Some(current.size_bytes),
                                before_modified_at_utc: Some(current.modified_at_utc),
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
                                before_modified_at_utc: Some(current.modified_at_utc),
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
    let (directories_to_remove, directories_to_create) =
        plan_directory_topology_changes(project, &operations)?;
    Ok(OperationPlan::new(
        checkpoint_id,
        kind,
        effective_selected.as_deref().map(|paths| {
            paths
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        }),
        operations,
    )
    .with_directory_changes(directories_to_remove, directories_to_create)
    .with_warnings(warnings))
}

fn operation_plan_authorization_key(project: &ProjectContext) -> Result<[u8; 32]> {
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let relative = Path::new(OPERATION_PLAN_AUTH_KEY_PATH);
    let key_path = project.repo_root.join(relative);
    let bytes =
        match anchored_repo.read_bytes_bounded_path(&key_path, OPERATION_PLAN_AUTH_KEY_BYTES) {
            Ok(bytes) => bytes,
            Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                let generated =
                    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()).into_bytes();
                match anchored_repo.write_bytes_atomic_new(relative, &generated) {
                    Ok(()) => generated,
                    Err(CheckPoError::Io { source, .. })
                        if source.kind() == ErrorKind::AlreadyExists =>
                    {
                        anchored_repo
                            .read_bytes_bounded_path(&key_path, OPERATION_PLAN_AUTH_KEY_BYTES)?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
    if bytes.len() != OPERATION_PLAN_AUTH_KEY_BYTES as usize
        || !bytes.iter().all(u8::is_ascii_hexdigit)
    {
        return Err(CheckPoError::Corruption(format!(
            "operation plan authorization key is invalid: {}",
            key_path.display()
        )));
    }
    Ok(*blake3::hash(&bytes).as_bytes())
}

fn operation_plan_authorization(project: &ProjectContext, plan: &OperationPlan) -> Result<String> {
    let mut payload = plan.clone();
    payload.authorization.clear();
    let bytes = serde_json::to_vec(&payload)
        .map_err(|error| crate::json_error("operation plan authorization", error))?;
    Ok(
        blake3::keyed_hash(&operation_plan_authorization_key(project)?, &bytes)
            .to_hex()
            .to_string(),
    )
}

fn authorize_operation_plan(project: &ProjectContext, plan: &mut OperationPlan) -> Result<()> {
    plan.authorization = operation_plan_authorization(project, plan)?;
    Ok(())
}

fn authorization_matches(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn validate_operation_plan_authorization(
    project: &ProjectContext,
    plan: &OperationPlan,
) -> Result<()> {
    let expected = operation_plan_authorization(project, plan)?;
    if !authorization_matches(&plan.authorization, &expected) {
        return Err(CheckPoError::Corruption(
            "operation plan authorization is invalid; preview the operation again".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn verify_full_restore_target(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
) -> Result<()> {
    let remaining = build_unsigned_plan_with_progress_and_cancellation(
        project,
        checkpoint_id.clone(),
        OperationPlanKind::Restore,
        None,
        None,
        None,
    )?;
    if !remaining.warnings.is_empty() || remaining.has_changes {
        return Err(CheckPoError::WorkingTreeChanged(
            "the working tree does not exactly match the restored checkpoint".to_string(),
        ));
    }
    Ok(())
}

fn plan_directory_topology_changes(
    project: &ProjectContext,
    operations: &[FileOperation],
) -> Result<(Vec<TrackedUnityFilePath>, Vec<TrackedUnityFilePath>)> {
    let before_files = operations
        .iter()
        .filter(|operation| operation.before_hash.is_some())
        .map(|operation| operation.path.clone())
        .collect::<BTreeSet<_>>();
    let deleted_files = operations
        .iter()
        .filter(|operation| operation.operation_type == FileOperationType::Delete)
        .map(|operation| operation.path.clone())
        .collect::<BTreeSet<_>>();
    let after_files = operations
        .iter()
        .filter(|operation| operation.after_hash.is_some())
        .map(|operation| operation.path.clone())
        .collect::<Vec<_>>();
    let mut remove = BTreeSet::new();
    let mut create = BTreeSet::new();

    for path in &after_files {
        let destination = path.to_project_path(project.project_root.as_path());
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
                return Err(CheckPoError::InvalidTrackedPath(format!(
                    "{path} is a symbolic link or reparse point"
                )))
            }
            Ok(metadata) if metadata.is_dir() => {
                for entry in walkdir::WalkDir::new(&destination)
                    .follow_links(false)
                    .contents_first(true)
                {
                    let entry =
                        entry.map_err(|error| CheckPoError::Corruption(error.to_string()))?;
                    let metadata = fs::symlink_metadata(entry.path())
                        .map_err(|error| crate::io_error(entry.path(), error))?;
                    if crate::metadata_is_link_or_reparse(&metadata) {
                        return Err(CheckPoError::InvalidTrackedPath(format!(
                            "directory topology contains a symbolic link or reparse point: {}",
                            entry.path().display()
                        )));
                    }
                    if metadata.is_dir() {
                        let relative = crate::relative_path_from_project(
                            project.project_root.as_path(),
                            entry.path(),
                        )?;
                        remove.insert(TrackedUnityFilePath::parse(&relative)?);
                    } else if metadata.is_file() {
                        let relative = crate::relative_path_from_project(
                            project.project_root.as_path(),
                            entry.path(),
                        )?;
                        let tracked = TrackedUnityFilePath::parse(&relative)?;
                        if !deleted_files.contains(&tracked) {
                            return Err(CheckPoError::WorkingTreeChanged(format!(
                                "directory blocker contains an unplanned file: {tracked}"
                            )));
                        }
                    } else {
                        return Err(CheckPoError::InvalidTrackedPath(format!(
                            "directory topology contains a non-regular entry: {}",
                            entry.path().display()
                        )));
                    }
                }
            }
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(CheckPoError::InvalidTrackedPath(format!(
                    "{path} is not a regular file or directory"
                )))
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => return Err(crate::io_error(&destination, error)),
        }

        let segments = path.as_str().split('/').collect::<Vec<_>>();
        let mut current = project.project_root.as_path().to_path_buf();
        let mut missing_or_blocked = false;
        for index in 0..segments.len().saturating_sub(1) {
            current.push(segments[index]);
            if index == 0 {
                continue;
            }
            let directory_path = TrackedUnityFilePath::parse(&segments[..=index].join("/"))?;
            if missing_or_blocked {
                create.insert(directory_path);
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => {
                    return Err(CheckPoError::InvalidTrackedPath(format!(
                        "{} contains a symbolic link or reparse point parent: {}",
                        path,
                        current.display()
                    )))
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(metadata) if metadata.is_file() && before_files.contains(&directory_path) => {
                    create.insert(directory_path);
                    missing_or_blocked = true;
                }
                Ok(_) => {
                    return Err(CheckPoError::InvalidTrackedPath(format!(
                        "unsafe parent component for {}: {}",
                        path,
                        current.display()
                    )))
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    create.insert(directory_path);
                    missing_or_blocked = true;
                }
                Err(error) => return Err(crate::io_error(&current, error)),
            }
        }
    }

    let mut remove = remove.into_iter().collect::<Vec<_>>();
    remove.sort_by(|left, right| {
        right
            .as_str()
            .matches('/')
            .count()
            .cmp(&left.as_str().matches('/').count())
            .then_with(|| left.cmp(right))
    });
    let create = create.into_iter().collect();
    Ok((remove, create))
}

pub(crate) fn validate_discard_request_scope(
    project: &ProjectContext,
    requested: &[TrackedUnityFilePath],
    plan: &OperationPlan,
) -> Result<()> {
    if plan.kind != OperationPlanKind::Discard {
        return Err(CheckPoError::Corruption(
            "discard request requires a discard operation plan".to_string(),
        ));
    }
    reject_selected_directory_meta(project, requested)?;
    let requested = requested.iter().cloned().collect::<BTreeSet<_>>();
    if requested.is_empty() {
        return Err(CheckPoError::InvalidTrackedPath(
            "discard requires selected tracked paths".to_string(),
        ));
    }
    let selected = plan.selected_paths.as_ref().ok_or_else(|| {
        CheckPoError::Corruption("discard plan requires selected paths".to_string())
    })?;
    let selected_set = selected.iter().cloned().collect::<BTreeSet<_>>();
    if selected_set.len() != selected.len() || !selected.iter().eq(selected_set.iter()) {
        return Err(CheckPoError::Corruption(
            "discard plan selected paths are not normalized".to_string(),
        ));
    }
    if !requested.is_subset(&selected_set) {
        return Err(CheckPoError::WorkingTreeChanged(
            "discard path set changed after preview".to_string(),
        ));
    }

    // The preview owns the destructive scope. Validate its frozen expansion
    // without walking the current tree, because Unity may legitimately create
    // new files or companions after preview.
    let mut authorized = requested.clone();
    for candidate in &selected_set {
        if requested
            .iter()
            .any(|root| path_is_same_or_descendant(candidate, root))
        {
            authorized.insert(candidate.clone());
        }
    }
    loop {
        let previous_len = authorized.len();
        for path in authorized.clone() {
            if let Some(companion) = unity_asset_companion_path(&path) {
                if selected_set.contains(&companion) {
                    authorized.insert(companion);
                }
            }
        }
        for candidate in selected_set
            .difference(&authorized)
            .cloned()
            .collect::<Vec<_>>()
        {
            let deletes_blocker = plan.operations.iter().any(|operation| {
                operation.path == candidate && operation.operation_type == FileOperationType::Delete
            });
            let creates_replacement_directory = plan.directories_to_create.contains(&candidate);
            let blocks_authorized_target = plan.operations.iter().any(|operation| {
                authorized.contains(&operation.path)
                    && matches!(
                        operation.operation_type,
                        FileOperationType::Restore | FileOperationType::Replace
                    )
                    && path_is_strict_descendant(&operation.path, &candidate)
            });
            if deletes_blocker && creates_replacement_directory && blocks_authorized_target {
                authorized.insert(candidate);
            }
        }
        if authorized.len() == previous_len {
            break;
        }
    }
    if authorized != selected_set {
        return Err(CheckPoError::WorkingTreeChanged(
            "discard path set changed after preview".to_string(),
        ));
    }
    validate_operation_plan_authorization(project, plan)?;
    Ok(())
}

fn path_is_same_or_descendant(
    candidate: &TrackedUnityFilePath,
    root: &TrackedUnityFilePath,
) -> bool {
    candidate == root || path_is_strict_descendant(candidate, root)
}

fn path_is_strict_descendant(
    candidate: &TrackedUnityFilePath,
    root: &TrackedUnityFilePath,
) -> bool {
    candidate
        .as_str()
        .strip_prefix(root.as_str())
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalize_discard_selection_with_snapshot<'a>(
    project: &ProjectContext,
    snapshot_paths: impl Iterator<Item = &'a TrackedUnityFilePath>,
    selected: &[TrackedUnityFilePath],
) -> Result<Vec<TrackedUnityFilePath>> {
    reject_selected_directory_meta(project, selected)?;
    let snapshot_paths = snapshot_paths.cloned().collect::<BTreeSet<_>>();
    let mut effective = selected.iter().cloned().collect::<BTreeSet<_>>();
    loop {
        let previous_len = effective.len();
        for path in effective.clone() {
            let Some(companion) = unity_asset_companion_path(&path) else {
                continue;
            };
            if snapshot_paths.contains(&companion)
                || current_companion_is_regular_file(project, &companion)?
            {
                effective.insert(companion);
            }
        }
        for path in effective.clone() {
            if snapshot_paths.contains(&path) {
                add_required_discard_topology_paths(project, &path, &mut effective)?;
            }
        }
        if effective.len() == previous_len {
            break;
        }
    }
    Ok(effective.into_iter().collect())
}

fn reject_selected_directory_meta(
    project: &ProjectContext,
    selected: &[TrackedUnityFilePath],
) -> Result<()> {
    for path in selected {
        let value = path.as_str();
        if !value.starts_with("Assets/") {
            continue;
        }
        let Some(asset) = value.strip_suffix(".meta") else {
            continue;
        };
        let asset = TrackedUnityFilePath::parse(asset)?;
        if matches!(
            current_path_kind(project, &asset)?,
            CurrentPathKind::Directory
        ) {
            return Err(CheckPoError::UnsafeFolderMetaOperation(path.to_string()));
        }
    }
    Ok(())
}

pub(super) fn ensure_discard_folder_meta_operation_is_safe(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let value = path.as_str();
    if !value.starts_with("Assets/") {
        return Ok(());
    }
    let Some(asset) = value.strip_suffix(".meta") else {
        return Ok(());
    };
    let asset = TrackedUnityFilePath::parse(asset)?;
    if matches!(
        current_path_kind(project, &asset)?,
        CurrentPathKind::Directory
    ) {
        return Err(CheckPoError::WorkingTreeChanged(format!(
            "{} became a Unity folder while discard was running",
            path
        )));
    }
    Ok(())
}

#[derive(Debug)]
enum CurrentPathKind {
    Missing,
    File,
    Directory,
    BlockedByFile(TrackedUnityFilePath),
}

fn current_path_kind(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<CurrentPathKind> {
    let segments = path.as_str().split('/').collect::<Vec<_>>();
    let mut current = project.project_root.as_path().to_path_buf();
    for index in 0..segments.len().saturating_sub(1) {
        current.push(segments[index]);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(CurrentPathKind::Missing)
            }
            Err(error) => return Err(crate::io_error(&current, error)),
        };
        if crate::metadata_is_link_or_reparse(&metadata) {
            return Err(CheckPoError::InvalidTrackedPath(format!(
                "{} contains a symbolic link or reparse point parent: {}",
                path,
                current.display()
            )));
        }
        if metadata.is_file() {
            let blocker = TrackedUnityFilePath::parse(&segments[..=index].join("/"))?;
            return Ok(CurrentPathKind::BlockedByFile(blocker));
        }
        if !metadata.is_dir() {
            return Err(CheckPoError::InvalidTrackedPath(format!(
                "unsafe parent component for {}: {}",
                path,
                current.display()
            )));
        }
    }

    let full_path = path.to_project_path(project.project_root.as_path());
    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) => Err(
            CheckPoError::InvalidTrackedPath(format!("{path} is a symbolic link or reparse point")),
        ),
        Ok(metadata) if metadata.is_file() => Ok(CurrentPathKind::File),
        Ok(metadata) if metadata.is_dir() => Ok(CurrentPathKind::Directory),
        Ok(_) => Err(CheckPoError::InvalidTrackedPath(format!(
            "{path} is not a regular file or directory"
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(CurrentPathKind::Missing),
        Err(error) => Err(crate::io_error(&full_path, error)),
    }
}

fn add_required_discard_topology_paths(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    effective: &mut BTreeSet<TrackedUnityFilePath>,
) -> Result<()> {
    match current_path_kind(project, path)? {
        CurrentPathKind::BlockedByFile(blocker) => {
            effective.insert(blocker);
        }
        CurrentPathKind::Directory => {
            let root = path.to_project_path(project.project_root.as_path());
            for entry in walkdir::WalkDir::new(&root)
                .follow_links(false)
                .min_depth(1)
            {
                let entry =
                    entry.map_err(|error| CheckPoError::WorkingTreeChanged(error.to_string()))?;
                let metadata = fs::symlink_metadata(entry.path())
                    .map_err(|error| crate::io_error(entry.path(), error))?;
                if crate::metadata_is_link_or_reparse(&metadata) {
                    return Err(CheckPoError::InvalidTrackedPath(format!(
                        "discard topology contains a symbolic link or reparse point: {}",
                        entry.path().display()
                    )));
                }
                if metadata.is_dir() {
                    continue;
                }
                if !metadata.is_file() {
                    return Err(CheckPoError::InvalidTrackedPath(format!(
                        "discard topology contains a non-regular entry: {}",
                        entry.path().display()
                    )));
                }
                let relative = crate::relative_path_from_project(
                    project.project_root.as_path(),
                    entry.path(),
                )?;
                effective.insert(TrackedUnityFilePath::parse(&relative)?);
            }
        }
        CurrentPathKind::Missing | CurrentPathKind::File => {}
    }
    Ok(())
}

fn current_file_state_for_discard(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
    exists_in_snapshot: bool,
    selected_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<Option<CurrentFileState>> {
    match current_path_kind(project, path)? {
        CurrentPathKind::Missing => Ok(None),
        CurrentPathKind::File => current_file_state(project, path),
        CurrentPathKind::BlockedByFile(blocker) if selected_paths.contains(&blocker) => Ok(None),
        CurrentPathKind::Directory
            if !exists_in_snapshot
                && selected_paths.iter().any(|candidate| {
                    candidate
                        .as_str()
                        .strip_prefix(path.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/'))
                }) =>
        {
            Ok(None)
        }
        CurrentPathKind::Directory | CurrentPathKind::BlockedByFile(_) if exists_in_snapshot => {
            Ok(None)
        }
        CurrentPathKind::Directory | CurrentPathKind::BlockedByFile(_) => {
            Err(CheckPoError::InvalidTrackedPath(path.to_string()))
        }
    }
}

fn unity_asset_companion_path(path: &TrackedUnityFilePath) -> Option<TrackedUnityFilePath> {
    let value = path.as_str();
    if !value.starts_with("Assets/") {
        return None;
    }
    let candidate = match value.strip_suffix(".meta") {
        Some(asset) => asset.to_string(),
        None => format!("{value}.meta"),
    };
    TrackedUnityFilePath::parse(&candidate).ok()
}

fn current_companion_is_regular_file(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<bool> {
    current_path_kind(project, path).map(|kind| matches!(kind, CurrentPathKind::File))
}

pub(super) fn validate_expected_plan(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    validate_plan_shape(project, plan)?;
    validate_operation_plan_authorization(project, plan)?;
    // The preview freezes the authorized paths. Changes outside that fixed
    // plan remain ordinary working-tree differences instead of expanding a
    // restore into newly discovered destructive operations.
    recheck_preconditions(project, plan)
}

fn validate_plan_shape(project: &ProjectContext, plan: &OperationPlan) -> Result<()> {
    if plan.schema_version != crate::OPERATION_PLAN_SCHEMA_VERSION {
        return Err(CheckPoError::Corruption(format!(
            "unsupported operation plan schema version: found {}, supported {}",
            plan.schema_version,
            crate::OPERATION_PLAN_SCHEMA_VERSION
        )));
    }
    if !plan.warnings.is_empty() {
        return Err(CheckPoError::Corruption(
            "operation plan contains scan warnings and cannot be applied".to_string(),
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
        || plan.metadata_count
            != plan
                .operations
                .iter()
                .filter(|operation| operation.operation_type == FileOperationType::SetMetadata)
                .count()
        || plan.has_changes
            != (!plan.operations.is_empty()
                || !plan.directories_to_remove.is_empty()
                || !plan.directories_to_create.is_empty())
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
        .try_fold(0_u64, |total, value| total.checked_add(value))
        .ok_or_else(|| CheckPoError::Corruption("operation staged byte total overflow".into()))?;
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
        .try_fold(0_u64, |total, value| total.checked_add(value))
        .ok_or_else(|| CheckPoError::Corruption("operation backup byte total overflow".into()))?;
    let expected_temporary_bytes = expected_staged_bytes
        .checked_add(expected_backup_bytes)
        .ok_or_else(|| {
            CheckPoError::Corruption("operation temporary byte total overflow".into())
        })?;
    if plan.staged_bytes != expected_staged_bytes
        || plan.backup_bytes != expected_backup_bytes
        || plan.estimated_temporary_bytes != expected_temporary_bytes
    {
        return Err(CheckPoError::Corruption(
            "operation plan byte estimates are inconsistent".to_string(),
        ));
    }
    let mut expected_remove = plan.directories_to_remove.clone();
    expected_remove.sort_by(|left, right| {
        right
            .as_str()
            .matches('/')
            .count()
            .cmp(&left.as_str().matches('/').count())
            .then_with(|| left.cmp(right))
    });
    expected_remove.dedup();
    let mut expected_create = plan.directories_to_create.clone();
    expected_create.sort();
    expected_create.dedup();
    if expected_remove != plan.directories_to_remove
        || expected_create != plan.directories_to_create
    {
        return Err(CheckPoError::Corruption(
            "operation plan directory topology is not normalized".to_string(),
        ));
    }
    match plan.kind {
        OperationPlanKind::Restore if plan.selected_paths.is_some() => {
            return Err(CheckPoError::Corruption(
                "restore plan must not include selected paths".to_string(),
            ));
        }
        OperationPlanKind::Discard => {
            let selected = plan.selected_paths.as_ref().ok_or_else(|| {
                CheckPoError::Corruption("discard plan requires selected paths".to_string())
            })?;
            let selected_set = selected.iter().collect::<BTreeSet<_>>();
            if selected.is_empty()
                || selected_set.len() != selected.len()
                || !selected.iter().eq(selected_set)
            {
                return Err(CheckPoError::Corruption(
                    "discard plan selected paths are not normalized".to_string(),
                ));
            }
            if plan
                .operations
                .iter()
                .any(|operation| !selected.contains(&operation.path))
            {
                return Err(CheckPoError::Corruption(
                    "discard plan operations exceed the selected scope".to_string(),
                ));
            }
        }
        _ => {}
    }

    validate_journal_operations(project, &plan.checkpoint_id, &plan.operations)?;
    validate_journal_directory_topology(
        &plan.operations,
        &plan.directories_to_remove,
        &plan.directories_to_create,
    )
}

pub(super) fn validate_journal_operations(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
    operations: &[FileOperation],
) -> Result<()> {
    let mut sorted = operations.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    if sorted != operations {
        return Err(CheckPoError::Corruption(
            "transaction operations are not sorted".to_string(),
        ));
    }
    if sorted
        .windows(2)
        .any(|window| window[0].path == window[1].path)
    {
        return Err(CheckPoError::Corruption(
            "transaction operations contain duplicate paths".to_string(),
        ));
    }

    let snapshot = load_project_snapshot(project, checkpoint_id)?;
    let snapshot_files = snapshot
        .files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    for operation in operations {
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
                    || operation.before_modified_at_utc.is_some()
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
                    || operation.before_modified_at_utc.is_none()
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
                    || operation.before_modified_at_utc.is_none()
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
            FileOperationType::SetMetadata => {
                let file = snapshot_files.get(&operation.path).ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "metadata operation missing snapshot entry for {}",
                        operation.path
                    ))
                })?;
                if operation.before_hash.is_none()
                    || operation.before_size_bytes.is_none()
                    || operation.before_modified_at_utc.is_none()
                    || operation.before_hash.as_ref() != Some(file.content_hash())
                    || operation.before_size_bytes != Some(file.content_size_bytes())
                    || operation.after_hash.as_ref() != Some(file.content_hash())
                    || operation.after_size_bytes != Some(file.content_size_bytes())
                    || operation.after_modified_at_utc.as_deref()
                        != Some(file.modified_at_utc.as_str())
                    || operation.before_modified_at_utc == operation.after_modified_at_utc
                {
                    return Err(CheckPoError::Corruption(format!(
                        "invalid metadata operation for {}",
                        operation.path
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_journal_directory_topology(
    operations: &[FileOperation],
    directories_to_remove: &[TrackedUnityFilePath],
    directories_to_create: &[TrackedUnityFilePath],
) -> Result<()> {
    let after_files = operations
        .iter()
        .filter(|operation| operation.after_hash.is_some())
        .map(|operation| &operation.path)
        .collect::<Vec<_>>();
    let mut normalized_remove = directories_to_remove.to_vec();
    normalized_remove.sort_by(|left, right| {
        right
            .as_str()
            .matches('/')
            .count()
            .cmp(&left.as_str().matches('/').count())
            .then_with(|| left.cmp(right))
    });
    normalized_remove.dedup();
    let mut normalized_create = directories_to_create.to_vec();
    normalized_create.sort();
    normalized_create.dedup();
    if normalized_remove != directories_to_remove || normalized_create != directories_to_create {
        return Err(CheckPoError::Corruption(
            "transaction directory topology is not normalized".to_string(),
        ));
    }
    for directory in directories_to_remove {
        if !after_files.iter().any(|file| {
            directory == *file
                || directory
                    .as_str()
                    .strip_prefix(file.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(CheckPoError::Corruption(format!(
                "transaction removes an unrelated directory: {directory}"
            )));
        }
    }
    for directory in directories_to_create {
        if !after_files.iter().any(|file| {
            file.as_str()
                .strip_prefix(directory.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(CheckPoError::Corruption(format!(
                "transaction creates an unrelated directory: {directory}"
            )));
        }
    }
    Ok(())
}
