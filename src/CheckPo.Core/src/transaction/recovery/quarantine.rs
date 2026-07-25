use super::*;

pub fn unresolved_transaction_quarantines(
    project_path: impl AsRef<Path>,
) -> Result<Vec<UnresolvedTransactionQuarantine>> {
    let project = crate::load_project(project_path)?;
    let _lock =
        crate::acquire_project_repository_shared_lock(&project, "transaction-quarantine-status")?;
    unresolved_transaction_quarantines_for_project(&project)
}

pub(crate) fn unresolved_transaction_quarantines_for_project(
    project: &ProjectContext,
) -> Result<Vec<UnresolvedTransactionQuarantine>> {
    let Some((quarantine_root, record_paths)) = quarantine_record_paths(project)? else {
        return Ok(Vec::new());
    };
    let record_names = record_paths
        .iter()
        .filter_map(|path| path.file_stem().and_then(|value| value.to_str()))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let mut unresolved = Vec::new();
    for record_path in record_paths {
        let record_name = record_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string();
        let quarantine_path = quarantine_root.join(&record_name);
        let fallback_transaction_id = record_name
            .split('-')
            .next()
            .unwrap_or(&record_name)
            .to_string();
        let metadata = match fs::symlink_metadata(&record_path) {
            Ok(metadata) => metadata,
            Err(error) => return Err(crate::io_error(&record_path, error)),
        };
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: fallback_transaction_id,
                quarantined_at_utc: None,
                quarantine_path,
                reason: "quarantine record is not a regular file".to_string(),
            });
            continue;
        }
        if quarantine_record_has_valid_resolution(&record_path)? {
            continue;
        }
        match read_quarantine_record(&record_path) {
            Ok(record)
                if !record.project_was_verified_in_before_state
                    && record.resolved_at_utc.is_none() =>
            {
                unresolved.push(UnresolvedTransactionQuarantine {
                    transaction_id: record.transaction_id,
                    quarantined_at_utc: Some(record.quarantined_at_utc),
                    quarantine_path,
                    reason: "the Unity project could not be verified in its pre-transaction state"
                        .to_string(),
                });
            }
            Ok(_) => {}
            Err(error) => unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: fallback_transaction_id,
                quarantined_at_utc: None,
                quarantine_path,
                reason: format!("quarantine record could not be verified: {error}"),
            }),
        }
    }
    for entry in
        fs::read_dir(&quarantine_root).map_err(|error| crate::io_error(&quarantine_root, error))?
    {
        let entry = entry.map_err(|error| crate::io_error(&quarantine_root, error))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if metadata.is_dir() && !record_names.contains(&name) {
            unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: name.split('-').next().unwrap_or(&name).to_string(),
                quarantined_at_utc: None,
                quarantine_path: entry.path(),
                reason: "quarantined transaction payload has no matching record".to_string(),
            });
        } else if crate::metadata_is_link_or_reparse(&metadata) {
            unresolved.push(UnresolvedTransactionQuarantine {
                transaction_id: name,
                quarantined_at_utc: None,
                quarantine_path: entry.path(),
                reason: "quarantine entry is a symbolic link or reparse point".to_string(),
            });
        }
    }
    unresolved.sort_by(|left, right| {
        left.quarantined_at_utc
            .cmp(&right.quarantined_at_utc)
            .then_with(|| left.transaction_id.cmp(&right.transaction_id))
    });
    Ok(unresolved)
}

pub fn ensure_no_unresolved_transaction_quarantines(project: &ProjectContext) -> Result<()> {
    let unresolved = unresolved_transaction_quarantines_for_project(project)?;
    if unresolved.is_empty() {
        return Ok(());
    }
    Err(CheckPoError::UnresolvedTransactionQuarantine(format!(
        "{} unresolved quarantined transaction(s); restore a known good checkpoint before changing this project",
        unresolved.len()
    )))
}

pub fn quarantine_transaction(
    project_path: impl AsRef<Path>,
    transaction_id: &str,
    options: ApplyOptions,
) -> Result<TransactionQuarantineResult> {
    if !options.yes {
        return Err(crate::user_error("transaction quarantine requires --yes."));
    }
    if transaction_id.len() != 32
        || !transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(crate::user_error("invalid transaction id."));
    }

    let project = crate::load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = crate::acquire_project_repository_lock(&project, "transaction-quarantine")?;
    let tx_root = journals_dir(&project.repo_root).join(transaction_id);
    ensure_regular_transaction_directory(&tx_root)?;

    let journal_path = tx_root.join("journal.json");
    if let Ok(journal) = read_transaction_journal(&journal_path) {
        if validate_transaction_journal_identity(&tx_root, &journal).is_ok()
            && matches!(
                journal.state,
                JournalState::Committed | JournalState::Recovered
            )
        {
            return Err(crate::user_error(
                "completed or recovered transactions cannot be quarantined; run transaction cleanup instead.",
            ));
        }
    }
    let (project_was_verified_in_before_state, mut warnings) =
        inspect_project_before_state(&project, &tx_root, &journal_path);
    if !project_was_verified_in_before_state {
        warnings.push(
            "The Unity project may contain a partially applied transaction. Restore a known good checkpoint before creating a new checkpoint."
                .to_string(),
        );
    }
    let rescue_root = project
        .repo_root
        .join("recovery-rescues")
        .join(transaction_id);
    let transaction_bytes = match dir_size(&tx_root) {
        Ok(size) => size,
        Err(error) => {
            warnings.push(format!(
                "Preserved transaction byte count could not be calculated: {error}"
            ));
            0
        }
    };
    let rescue_bytes = match optional_regular_directory_size(&rescue_root) {
        Ok(size) => size,
        Err(error) => {
            warnings.push(format!(
                "Preserved recovery rescue byte count could not be calculated: {error}"
            ));
            0
        }
    };
    let preserved_bytes = transaction_bytes.saturating_add(rescue_bytes);

    let quarantine_root = project.repo_root.join("quarantined-journals");
    crate::create_dir_all_no_follow(&project.repo_root, &quarantine_root)?;
    crate::sync_parent_dir(&quarantine_root)?;

    let quarantine_name = format!("{transaction_id}-{}", Uuid::new_v4().simple());
    let quarantine_path = quarantine_root.join(&quarantine_name);
    let record_path = quarantine_root.join(format!("{quarantine_name}.json"));
    write_quarantine_json(
        &project.repo_root,
        &record_path,
        &QuarantineRecord {
            schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
            transaction_id: transaction_id.to_string(),
            quarantined_at_utc: crate::now_utc_string(),
            original_journal_path: journal_path,
            project_was_verified_in_before_state,
            resolved_at_utc: None,
            resolved_checkpoint_id: None,
        },
    )?;
    if let Err(error) = move_repo_directory_anchored(&project.repo_root, &tx_root, &quarantine_path)
    {
        let _ = remove_repo_file_if_exists_anchored(&project.repo_root, &record_path);
        return Err(error);
    }
    if let Err(error) =
        move_recovery_rescue_into_quarantine(&project, transaction_id, &quarantine_path)
    {
        warnings.push(format!(
            "Recovery rescue data remains in CheckPo storage because it could not be bundled into the quarantine: {error}"
        ));
    }

    crate::diagnostics::log_warning(
        "transaction-quarantine",
        &format!(
            "transaction {transaction_id} was preserved at {}",
            quarantine_path.display()
        ),
    );
    Ok(TransactionQuarantineResult {
        transaction_id: transaction_id.to_string(),
        quarantine_path,
        preserved_bytes,
        warnings,
    })
}

pub(in crate::transaction) fn resolve_unverified_transaction_quarantines_unlocked(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
) -> Result<usize> {
    let Some((quarantine_root, record_paths)) = quarantine_record_paths(project)? else {
        return Ok(0);
    };
    let resolved_at_utc = crate::now_utc_string();
    let mut resolved_count = 0;
    for record_path in record_paths {
        let metadata = fs::symlink_metadata(&record_path)
            .map_err(|error| crate::io_error(&record_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            continue;
        }
        if quarantine_record_has_valid_resolution(&record_path)? {
            continue;
        }
        write_quarantine_resolution(
            &project.repo_root,
            &record_path,
            checkpoint_id,
            &resolved_at_utc,
        )?;
        resolved_count += 1;
    }
    for entry in
        fs::read_dir(&quarantine_root).map_err(|error| crate::io_error(&quarantine_root, error))?
    {
        let entry = entry.map_err(|error| crate::io_error(&quarantine_root, error))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let record_path = quarantine_root.join(format!("{name}.json"));
        if fs::symlink_metadata(&record_path).is_ok() {
            continue;
        }
        write_quarantine_json(
            &project.repo_root,
            &record_path,
            &QuarantineRecord {
                schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
                transaction_id: name.split('-').next().unwrap_or(&name).to_string(),
                quarantined_at_utc: resolved_at_utc.clone(),
                original_journal_path: entry.path().join("journal.json"),
                project_was_verified_in_before_state: false,
                resolved_at_utc: None,
                resolved_checkpoint_id: None,
            },
        )?;
        write_quarantine_resolution(
            &project.repo_root,
            &record_path,
            checkpoint_id,
            &resolved_at_utc,
        )?;
        resolved_count += 1;
    }
    Ok(resolved_count)
}

fn write_quarantine_resolution(
    repo_root: &Path,
    record_path: &Path,
    checkpoint_id: &SnapshotId,
    resolved_at_utc: &str,
) -> Result<()> {
    let anchored_repo = crate::storage::AnchoredRoot::open(repo_root)?;
    let bytes = anchored_repo.read_bytes_bounded_path(record_path, MAX_QUARANTINE_RECORD_BYTES)?;
    write_quarantine_json(
        repo_root,
        &quarantine_resolution_path(record_path),
        &QuarantineResolutionRecord {
            schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
            resolved_at_utc: resolved_at_utc.to_string(),
            resolved_checkpoint_id: checkpoint_id.clone(),
            quarantine_record_digest: blake3::hash(&bytes).to_hex().to_string(),
        },
    )
}

fn write_quarantine_json<T: serde::Serialize>(
    repo_root: &Path,
    path: &Path,
    value: &T,
) -> Result<()> {
    crate::storage::AnchoredRoot::open(repo_root)?.write_json_atomic_path(path, value)
}

fn quarantine_resolution_path(record_path: &Path) -> PathBuf {
    record_path.with_extension("resolved")
}

fn quarantine_record_has_valid_resolution(record_path: &Path) -> Result<bool> {
    let resolution_path = quarantine_resolution_path(record_path);
    let metadata = match fs::symlink_metadata(&resolution_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(crate::io_error(&resolution_path, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Ok(false);
    }
    let repo_root = quarantine_repo_root_for_record_path(record_path)?;
    let anchored_repo = crate::storage::AnchoredRoot::open(repo_root)?;
    let resolution =
        match anchored_repo.read_json_path::<QuarantineResolutionRecord>(&resolution_path) {
            Ok(resolution) if resolution.schema_version == QUARANTINE_RECORD_SCHEMA_VERSION_V1 => {
                resolution
            }
            Ok(_) | Err(_) => return Ok(false),
        };
    let record_bytes =
        anchored_repo.read_bytes_bounded_path(record_path, MAX_QUARANTINE_RECORD_BYTES)?;
    Ok(resolution.quarantine_record_digest == blake3::hash(&record_bytes).to_hex().as_str())
}

fn quarantine_record_paths(project: &ProjectContext) -> Result<Option<(PathBuf, Vec<PathBuf>)>> {
    let quarantine_root = project.repo_root.join("quarantined-journals");
    let root_metadata = match fs::symlink_metadata(&quarantine_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(&quarantine_root, error)),
    };
    if crate::metadata_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "transaction quarantine root is not a regular directory: {}",
            quarantine_root.display()
        )));
    }
    let mut record_paths = Vec::new();
    for entry in
        fs::read_dir(&quarantine_root).map_err(|error| crate::io_error(&quarantine_root, error))?
    {
        let entry = entry.map_err(|error| crate::io_error(&quarantine_root, error))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            record_paths.push(path);
        }
    }
    record_paths.sort();
    Ok(Some((quarantine_root, record_paths)))
}

fn read_quarantine_record(path: &Path) -> Result<QuarantineRecord> {
    let repo_root = quarantine_repo_root_for_record_path(path)?;
    let bytes = crate::storage::AnchoredRoot::open(repo_root)?
        .read_bytes_bounded_path(path, MAX_QUARANTINE_RECORD_BYTES)?;
    let envelope: QuarantineRecordEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))?;
    if envelope.schema_version > QUARANTINE_RECORD_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction quarantine record schema".to_string(),
            found: envelope.schema_version,
            supported: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
        });
    }
    if envelope.schema_version != QUARANTINE_RECORD_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::Corruption(format!(
            "invalid transaction quarantine record schema: {}",
            envelope.schema_version
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))
}

fn quarantine_repo_root_for_record_path(path: &Path) -> Result<&Path> {
    let quarantine_root = path.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "quarantine record has no parent: {}",
            path.display()
        ))
    })?;
    if quarantine_root.file_name() != Some(std::ffi::OsStr::new("quarantined-journals")) {
        return Err(CheckPoError::Corruption(format!(
            "quarantine record is outside the canonical repository namespace: {}",
            path.display()
        )));
    }
    quarantine_root.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "quarantine record has no repository root: {}",
            path.display()
        ))
    })
}

fn inspect_project_before_state(
    project: &ProjectContext,
    tx_root: &Path,
    journal_path: &Path,
) -> (bool, Vec<String>) {
    let result = (|| -> Result<bool> {
        let metadata = fs::symlink_metadata(journal_path)
            .map_err(|error| crate::io_error(journal_path, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            return Err(CheckPoError::Corruption(format!(
                "transaction journal is not a regular file: {}",
                journal_path.display()
            )));
        }
        let journal = read_transaction_journal(journal_path)?;
        validate_transaction_journal_identity(tx_root, &journal)?;
        if journal.operations.is_empty() {
            return Err(CheckPoError::Corruption(
                "transaction journal contains no operations".to_string(),
            ));
        }
        for operation in &journal.operations {
            let current = current_file_state(project, &operation.path)?;
            if current.as_ref().map(|state| &state.hash) != operation.before_hash.as_ref()
                || current.as_ref().map(|state| state.size_bytes) != operation.before_size_bytes
                || current.as_ref().map(|state| state.modified_at_utc.as_str())
                    != operation.before_modified_at_utc.as_deref()
            {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    match result {
        Ok(is_before) => (is_before, Vec::new()),
        Err(error) => (
            false,
            vec![format!(
                "Transaction state could not be verified before quarantine: {error}"
            )],
        ),
    }
}

pub(super) fn quarantine_unknown_transaction_locked(
    project: &ProjectContext,
    tx_root: &Path,
    journal_path: &Path,
    transaction_id: &str,
    reason: &str,
) -> Result<PathBuf> {
    let quarantine_root = project.repo_root.join("quarantined-journals");
    crate::create_dir_all_no_follow(&project.repo_root, &quarantine_root)?;
    let quarantine_name = format!("unknown-{}", Uuid::new_v4().simple());
    let quarantine_path = quarantine_root.join(&quarantine_name);
    let record_path = quarantine_root.join(format!("{quarantine_name}.json"));
    write_quarantine_json(
        &project.repo_root,
        &record_path,
        &QuarantineRecord {
            schema_version: QUARANTINE_RECORD_SCHEMA_VERSION_V1,
            transaction_id: transaction_id.to_string(),
            quarantined_at_utc: crate::now_utc_string(),
            original_journal_path: journal_path.to_path_buf(),
            project_was_verified_in_before_state: false,
            resolved_at_utc: None,
            resolved_checkpoint_id: None,
        },
    )?;
    if let Err(error) = move_repo_directory_anchored(&project.repo_root, tx_root, &quarantine_path)
    {
        let _ = remove_repo_file_if_exists_anchored(&project.repo_root, &record_path);
        return Err(error);
    }
    if let Err(error) =
        move_recovery_rescue_into_quarantine(project, transaction_id, &quarantine_path)
    {
        crate::diagnostics::log_warning(
            "transaction-recovery",
            &format!(
                "transaction {transaction_id} was quarantined, but its recovery rescue data remains in CheckPo storage: {error}"
            ),
        );
    }
    crate::diagnostics::log_warning(
        "transaction-recovery",
        &format!(
            "{reason}; transaction {transaction_id} was quarantined at {}",
            quarantine_path.display()
        ),
    );
    Ok(quarantine_path)
}

fn optional_regular_directory_size(path: &Path) -> Result<u64> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(crate::io_error(path, error)),
        Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
            dir_size(path)
        }
        Ok(_) => Err(CheckPoError::Corruption(format!(
            "recovery rescue directory is unsafe: {}",
            path.display()
        ))),
    }
}

fn move_recovery_rescue_into_quarantine(
    project: &ProjectContext,
    transaction_id: &str,
    quarantine_path: &Path,
) -> Result<()> {
    let rescue_root = project
        .repo_root
        .join("recovery-rescues")
        .join(transaction_id);
    match fs::symlink_metadata(&rescue_root) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::io_error(&rescue_root, error)),
        Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(CheckPoError::Corruption(format!(
                "recovery rescue directory is unsafe: {}",
                rescue_root.display()
            )))
        }
    }
    move_repo_directory_anchored(
        &project.repo_root,
        &rescue_root,
        &quarantine_path.join("recovery-rescue"),
    )
}

fn move_repo_directory_anchored(repo_root: &Path, source: &Path, destination: &Path) -> Result<()> {
    let source_relative = source.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "transaction directory is outside repository: {}",
            source.display()
        ))
    })?;
    let destination_relative = destination.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "quarantine directory is outside repository: {}",
            destination.display()
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(repo_root)?;
    let (source_parent, source_leaf) = root.open_parent_for_mutation(source_relative, false)?;
    let source_directory = source_parent.open_directory(&source_leaf)?;
    let (destination_parent, destination_leaf) =
        root.open_parent_for_mutation(destination_relative, true)?;
    let source_parent_relative = source_relative.parent().unwrap_or_else(|| Path::new(""));
    let destination_parent_relative = destination_relative
        .parent()
        .unwrap_or_else(|| Path::new(""));
    root.verify_parent_binding(source_parent_relative, &source_parent)?;
    root.verify_parent_binding(destination_parent_relative, &destination_parent)?;
    source_parent.rename_directory_no_replace_to_owned(
        &source_leaf,
        source_directory,
        &destination_parent,
        &destination_leaf,
    )?;
    destination_parent.sync_all()?;
    source_parent.sync_all()?;
    root.verify_parent_binding(destination_parent_relative, &destination_parent)?;
    root.verify_root_binding()
}

fn remove_repo_file_if_exists_anchored(repo_root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption(format!(
            "quarantine record is outside repository: {}",
            path.display()
        ))
    })?;
    let root = crate::storage::AnchoredRoot::open(repo_root)?;
    let (parent, leaf) = root.open_parent_for_mutation(relative, false)?;
    let file = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    parent.unlink_file_if_bound(&leaf, file)?;
    parent.sync_all()?;
    root.verify_root_binding()
}
