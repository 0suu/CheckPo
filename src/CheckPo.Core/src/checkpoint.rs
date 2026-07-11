use crate::{
    acquire_repository_lock, ensure_no_pending_transactions, list_snapshot_ids, load_project,
    load_project_snapshot, load_snapshot, now_utc_string, object_path,
    put_object_from_file_with_known_hash, report_operation_progress, save_snapshot,
    scan_project_for_checkpoint, scan_project_for_checkpoint_with_baseline,
    write_latest_snapshot_id, CheckPoError, CheckpointDeleteResult, CheckpointListResult,
    CheckpointSummary, CreateCheckpointOptions, FileFingerprintUpdate, Result, ScannedFile,
    SnapshotContent, SnapshotEntry, SnapshotFile, SnapshotId, TrackedUnityFilePath,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

pub fn create_checkpoint(
    project_path: impl AsRef<Path>,
    name: &str,
    options: CreateCheckpointOptions,
) -> Result<CheckpointSummary> {
    if name.trim().is_empty() {
        return Err(crate::user_error(
            "checkpoint create requires --name <name>.",
        ));
    }
    let project = if options.init_if_needed {
        crate::init_project(&project_path)?;
        load_project(&project_path)?
    } else {
        load_project(&project_path)?
    };
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "checkpoint-create")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    crate::ensure_not_cancelled(options.cancellation.as_ref())?;
    let progress = options.progress.as_deref().map(|f| f as &dyn Fn(_));
    let (parent_snapshot_id, parent_snapshot, latest_warning) =
        latest_checkpoint_for_create(&project)?;
    let (scanned, scan_warnings, incomplete) = match parent_snapshot.as_ref() {
        Some(parent_snapshot) => scan_project_for_checkpoint_with_baseline(
            &project,
            Some(parent_snapshot),
            progress,
            options.cancellation.as_ref(),
        )?,
        None => scan_project_for_checkpoint(&project, progress, options.cancellation.as_ref())?,
    };
    if incomplete {
        return Err(crate::user_error(format!(
            "checkpoint was not created because some tracked files could not be read: {}",
            scan_warnings
                .iter()
                .map(crate::scanner::format_scan_warning)
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    report_operation_progress(progress, "storeCheckpoint", 0, scanned.len(), None);
    let mut newly_stored_bytes = 0_u64;
    let mut files = Vec::with_capacity(scanned.len());
    let previous_files = parent_snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .files
                .iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut available_previous_objects =
        preload_available_previous_objects(&project.repo_root, &previous_files)?;
    for (index, file) in scanned.iter().enumerate() {
        crate::ensure_not_cancelled(options.cancellation.as_ref())?;
        let previous = previous_files.get(&file.path).copied().filter(|entry| {
            entry.size_bytes == file.size_bytes
                && entry.content_size_bytes() == file.size_bytes
                && entry.content_hash() == &file.hash
        });
        let (object_id, created, size_bytes) = match previous {
            Some(entry) => reuse_previous_object_or_repair(
                &project.repo_root,
                file,
                entry,
                &mut available_previous_objects,
            )?,
            None => put_scanned_file(&project.repo_root, file)?,
        };
        if object_id != file.hash {
            return Err(crate::CheckPoError::WorkingTreeChanged(
                file.path.to_string(),
            ));
        }
        if created {
            newly_stored_bytes += size_bytes;
        }
        files.push(SnapshotEntry {
            path: file.path.clone(),
            size_bytes,
            modified_at_utc: file.modified_at_utc.clone(),
            content: SnapshotContent::Whole {
                hash: object_id,
                size_bytes,
            },
        });
        report_operation_progress(
            progress,
            "storeCheckpoint",
            index + 1,
            scanned.len(),
            Some(file.path.to_string()),
        );
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    crate::ensure_not_cancelled(options.cancellation.as_ref())?;
    report_operation_progress(progress, "finalizing", 1, 1, None);
    let created_at_utc = now_utc_string();
    let snapshot = SnapshotFile {
        schema_version: 1,
        project_id: project.project_id.clone(),
        parent_snapshot_id,
        created_at_utc: created_at_utc.clone(),
        name: name.trim().to_string(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files,
    };
    let snapshot_id = save_snapshot(&project.repo_root, &snapshot)?;
    write_latest_snapshot_id(&project.repo_root, &snapshot_id)?;
    let mut warnings = scan_warnings
        .iter()
        .map(crate::scanner::format_scan_warning)
        .collect::<Vec<_>>();
    if let Some(warning) = latest_warning {
        crate::diagnostics::log_warning("checkpoint-create", &warning);
        warnings.push(warning);
    }
    match crate::open_index_connection(&project) {
        Ok(index) => {
            if let Err(error) = crate::index_snapshot_with_index_connection(
                &index,
                &project,
                &snapshot_id,
                &snapshot,
            ) {
                warnings.push(format!("SQLite index update failed: {error}"));
            }
            if let Err(error) = refresh_fingerprints_after_checkpoint(&index, &project, &scanned) {
                warnings.push(format!("SQLite fingerprint update failed: {error}"));
            }
        }
        Err(error) => warnings.push(format!("SQLite index update failed: {error}")),
    }
    report_operation_progress(progress, "complete", 1, 1, None);
    Ok(CheckpointSummary {
        checkpoint_id: snapshot_id,
        name: snapshot.name,
        created_at_utc,
        file_count: snapshot.files.len(),
        logical_size_bytes: snapshot.files.iter().map(|file| file.size_bytes).sum(),
        newly_stored_bytes,
        warnings,
    })
}

fn preload_available_previous_objects(
    repo_root: &Path,
    previous_files: &BTreeMap<TrackedUnityFilePath, &SnapshotEntry>,
) -> Result<BTreeSet<crate::ObjectId>> {
    let mut expected_sizes = BTreeMap::new();
    for entry in previous_files.values() {
        match expected_sizes.insert(entry.content_hash().clone(), entry.content_size_bytes()) {
            Some(existing) if existing != entry.content_size_bytes() => {
                return Err(CheckPoError::Corruption(format!(
                    "snapshot object {} has conflicting sizes",
                    entry.content_hash()
                )))
            }
            _ => {}
        }
    }
    let expected_sizes = expected_sizes.into_iter().collect::<Vec<_>>();
    let loose_root = repo_root.join("objects").join("loose");
    crate::ensure_regular_directory_no_follow(&loose_root)?;
    let mut first_level_shards = BTreeSet::new();
    let mut second_level_shards = BTreeSet::new();
    for (object_id, _) in &expected_sizes {
        let object = object_path(repo_root, object_id);
        let shard = object.parent().ok_or_else(|| {
            CheckPoError::Corruption(format!("invalid object path: {}", object.display()))
        })?;
        let first_level = shard.parent().ok_or_else(|| {
            CheckPoError::Corruption(format!("invalid object shard: {}", shard.display()))
        })?;
        first_level_shards.insert(first_level.to_path_buf());
        second_level_shards.insert(shard.to_path_buf());
    }
    let existing_first_level_shards = first_level_shards
        .into_par_iter()
        .map(|path| regular_directory_exists_no_follow(&path).map(|exists| (path, exists)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(path, exists)| exists.then_some(path))
        .collect::<BTreeSet<_>>();
    let existing_second_level_shards = second_level_shards
        .into_par_iter()
        .filter(|path| {
            path.parent()
                .is_some_and(|parent| existing_first_level_shards.contains(parent))
        })
        .map(|path| regular_directory_exists_no_follow(&path).map(|exists| (path, exists)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(path, exists)| exists.then_some(path))
        .collect::<BTreeSet<_>>();
    expected_sizes
        .into_par_iter()
        .map(|(object_id, expected_size)| {
            let object = object_path(repo_root, &object_id);
            if !object
                .parent()
                .is_some_and(|parent| existing_second_level_shards.contains(parent))
            {
                return Ok(None);
            }
            match fs::symlink_metadata(&object) {
                Ok(metadata)
                    if !crate::metadata_is_link_or_reparse(&metadata)
                        && metadata.is_file()
                        && metadata.len() == expected_size =>
                {
                    Ok(Some(object_id))
                }
                Ok(_) => Ok(None),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(crate::io_error(&object, error)),
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(|objects| objects.into_iter().flatten().collect())
}

fn regular_directory_exists_no_follow(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
            Ok(true)
        }
        Ok(_) => Err(CheckPoError::Corruption(format!(
            "unsafe object shard directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(crate::io_error(path, error)),
    }
}

fn latest_checkpoint_for_create(
    project: &crate::ProjectContext,
) -> Result<(Option<SnapshotId>, Option<SnapshotFile>, Option<String>)> {
    let latest_path = crate::refs_latest_path(&project.repo_root);
    let text = match fs::read_to_string(&latest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((None, None, None)),
        Err(error) => return Err(crate::io_error(&latest_path, error)),
    };
    let snapshot_id = match SnapshotId::parse(text.trim()) {
        Ok(snapshot_id) => snapshot_id,
        Err(CheckPoError::InvalidId(message)) => {
            return Ok(latest_create_fallback(format!(
                "refs/latest is malformed: {message}"
            )))
        }
        Err(error) => return Err(error),
    };
    match load_project_snapshot(project, &snapshot_id) {
        Ok(snapshot) => Ok((Some(snapshot_id), Some(snapshot), None)),
        Err(error @ CheckPoError::SnapshotNotFound(_))
        | Err(error @ CheckPoError::Corruption(_))
        | Err(error @ CheckPoError::Json { .. }) => Ok(latest_create_fallback(error.to_string())),
        Err(error) => Err(error),
    }
}

fn latest_create_fallback(
    reason: String,
) -> (Option<SnapshotId>, Option<SnapshotFile>, Option<String>) {
    (
        None,
        None,
        Some(format!(
            "Latest checkpoint could not be used ({reason}). All tracked files were hashed and this checkpoint was created as a new history root."
        )),
    )
}

fn reuse_previous_object_or_repair(
    repo_root: &Path,
    file: &ScannedFile,
    previous: &SnapshotEntry,
    available_objects: &mut BTreeSet<crate::ObjectId>,
) -> Result<(crate::ObjectId, bool, u64)> {
    let object_id = previous.content_hash();
    if available_objects.contains(object_id) {
        return Ok((object_id.clone(), false, file.size_bytes));
    }

    let object = crate::storage::object_path_no_follow(repo_root, object_id)?;
    let available = match fs::symlink_metadata(&object) {
        Ok(metadata) => {
            !crate::metadata_is_link_or_reparse(&metadata)
                && metadata.is_file()
                && metadata.len() == previous.content_size_bytes()
        }
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(crate::io_error(&object, error)),
    };
    let created = if available {
        false
    } else {
        match put_object_from_file_with_known_hash(
            repo_root,
            &file.full_path,
            object_id,
            previous.content_size_bytes(),
        ) {
            Ok(created) => created,
            Err(crate::CheckPoError::ObjectHashMismatch(_)) => {
                return Err(crate::CheckPoError::WorkingTreeChanged(
                    file.path.to_string(),
                ));
            }
            Err(error) => return Err(error),
        }
    };
    available_objects.insert(object_id.clone());
    Ok((object_id.clone(), created, file.size_bytes))
}

fn put_scanned_file(repo_root: &Path, file: &ScannedFile) -> Result<(crate::ObjectId, bool, u64)> {
    let object = crate::storage::object_path_no_follow(repo_root, &file.hash)?;
    match fs::symlink_metadata(&object) {
        Ok(metadata)
            if metadata.is_file()
                && !crate::metadata_is_link_or_reparse(&metadata)
                && metadata.len() == file.size_bytes =>
        {
            ensure_scanned_file_still_matches(file)?;
            // Existing objects are content-addressed and verified when written.
            // Normal checkpoint creation keeps the hot path fast; full integrity checks belong to verify.
            return Ok((file.hash.clone(), false, file.size_bytes));
        }
        Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            return Err(CheckPoError::Corruption(format!(
                "object destination is not a no-follow regular file: {}",
                object.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(crate::io_error(&object, error)),
    }
    let created = match put_object_from_file_with_known_hash(
        repo_root,
        &file.full_path,
        &file.hash,
        file.size_bytes,
    ) {
        Ok(created) => created,
        Err(crate::CheckPoError::ObjectHashMismatch(_)) => {
            return Err(crate::CheckPoError::WorkingTreeChanged(
                file.path.to_string(),
            ));
        }
        Err(error) => return Err(error),
    };
    Ok((file.hash.clone(), created, file.size_bytes))
}

fn ensure_scanned_file_still_matches(file: &ScannedFile) -> Result<()> {
    let leaf_metadata = fs::symlink_metadata(&file.full_path)
        .map_err(|error| crate::io_error(&file.full_path, error))?;
    if leaf_metadata.file_type().is_symlink() {
        return Err(crate::CheckPoError::WorkingTreeChanged(
            file.path.to_string(),
        ));
    }
    let metadata =
        fs::metadata(&file.full_path).map_err(|error| crate::io_error(&file.full_path, error))?;
    let modified = metadata
        .modified()
        .map_err(|error| crate::io_error(&file.full_path, error))?;
    if let Some(expected) = file.fingerprint.as_deref() {
        let current = crate::scanner::file_fingerprint(&file.full_path, &metadata)?;
        if current.as_deref() != Some(expected) {
            return Err(crate::CheckPoError::WorkingTreeChanged(
                file.path.to_string(),
            ));
        }
    }
    if metadata.len() != file.size_bytes || crate::canonical_utc(modified) != file.modified_at_utc {
        return Err(crate::CheckPoError::WorkingTreeChanged(
            file.path.to_string(),
        ));
    }
    Ok(())
}

pub fn list_checkpoints(project_path: impl AsRef<Path>) -> Result<Vec<CheckpointSummary>> {
    let project = load_project(project_path)?;
    list_checkpoints_for_project(&project)
}

pub fn list_checkpoints_for_project(
    project: &crate::ProjectContext,
) -> Result<Vec<CheckpointSummary>> {
    Ok(list_checkpoints_with_warnings_for_project(project)?.checkpoints)
}

pub fn list_checkpoints_with_warnings_for_project(
    project: &crate::ProjectContext,
) -> Result<CheckpointListResult> {
    let (mut checkpoints, mut warnings) = match crate::list_checkpoint_summaries_from_index(project)
    {
        Ok(checkpoints) => (checkpoints, Vec::new()),
        Err(index_error) => {
            let mut direct = list_checkpoints_from_snapshots(project)?;
            direct
                .warnings
                .insert(0, format!("SQLite index was not used: {index_error}"));
            (direct.checkpoints, direct.warnings)
        }
    };
    warnings.extend(crate::apply_checkpoint_name_overrides(
        project,
        &mut checkpoints,
    ));
    warnings.sort();
    warnings.dedup();
    Ok(CheckpointListResult {
        checkpoints,
        warnings,
    })
}

pub fn rename_checkpoint(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    name: &str,
) -> Result<CheckpointSummary> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::user_error(
            "checkpoint rename requires --name <name>.",
        ));
    }
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "checkpoint-rename")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let id = SnapshotId::parse(checkpoint_id)?;
    let snapshot = load_project_snapshot(&project, &id)?;
    let (mut names, warnings) = crate::read_checkpoint_name_overrides(&project);
    if !warnings.is_empty() {
        return Err(crate::CheckPoError::Corruption(format!(
            "checkpoint display names cannot be modified until their metadata is repaired: {}",
            warnings.join("; ")
        )));
    }
    if name == snapshot.name {
        names.remove(id.as_str());
    } else {
        names.insert(id.to_string(), name.to_string());
    }
    crate::write_checkpoint_name_overrides(&project, &names)?;
    Ok(summary_from_snapshot(
        id,
        &snapshot,
        name.to_string(),
        Vec::new(),
    ))
}

pub fn delete_checkpoint(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
) -> Result<CheckpointDeleteResult> {
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "checkpoint-delete")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let id = SnapshotId::parse(checkpoint_id)?;
    let path = crate::snapshot_path(&project.repo_root, &id);
    if !path.is_file() {
        return Err(crate::CheckPoError::SnapshotNotFound(id.to_string()));
    }
    load_project_snapshot(&project, &id)?;
    let (_, name_warnings) = crate::read_checkpoint_name_overrides(&project);
    if !name_warnings.is_empty() {
        return Err(crate::CheckPoError::Corruption(format!(
            "checkpoint display names cannot be modified until their metadata is repaired: {}",
            name_warnings.join("; ")
        )));
    }
    let direct = list_checkpoints_from_snapshots(&project)?;
    if !direct.warnings.is_empty() {
        return Err(crate::user_error(format!(
            "checkpoint delete is blocked because not all snapshots are readable: {}",
            direct.warnings.join("; ")
        )));
    }
    let mut remaining_checkpoints = direct.checkpoints;
    remaining_checkpoints.retain(|summary| summary.checkpoint_id != id);
    let remaining_checkpoint_count = remaining_checkpoints.len();
    if crate::read_latest_snapshot_id(&project.repo_root)?.as_ref() == Some(&id) {
        if let Some(new_latest) = remaining_checkpoints
            .first()
            .map(|summary| summary.checkpoint_id.clone())
        {
            write_latest_snapshot_id(&project.repo_root, &new_latest)?;
        } else {
            let latest_path = crate::refs_latest_path(&project.repo_root);
            if latest_path.exists() {
                fs::remove_file(&latest_path)
                    .map_err(|error| crate::io_error(&latest_path, error))?;
                crate::sync_parent_dir(&latest_path)?;
            }
        }
    }
    fs::remove_file(&path).map_err(|error| crate::io_error(&path, error))?;
    crate::sync_parent_dir(&path)?;
    let mut warnings = Vec::new();
    if let Err(error) = crate::delete_snapshot_from_index(&project, &id) {
        warnings.push(format!(
            "SQLite index update failed after checkpoint delete: {error}"
        ));
        if let Err(rebuild_error) = crate::rebuild_index_for_project_unlocked(&project, None, None)
        {
            warnings.push(format!(
                "SQLite index rebuild failed after checkpoint delete: {rebuild_error}"
            ));
        }
    }
    match crate::remove_checkpoint_name_override(&project, &id) {
        Ok(name_warnings) => warnings.extend(name_warnings),
        Err(error) => warnings.push(format!(
            "checkpoint display name cleanup failed after delete: {error}"
        )),
    }
    Ok(CheckpointDeleteResult {
        deleted_checkpoint_id: id,
        deleted_snapshot_path: path,
        remaining_checkpoint_count,
        warnings,
    })
}

fn list_checkpoints_from_snapshots(
    project: &crate::ProjectContext,
) -> Result<CheckpointListResult> {
    let mut summaries = Vec::new();
    let mut warnings = Vec::new();
    for snapshot_id in list_snapshot_ids(&project.repo_root)? {
        let snapshot = match load_snapshot(&project.repo_root, &snapshot_id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warnings.push(format!("{snapshot_id}: {error}"));
                continue;
            }
        };
        if snapshot.project_id != project.project_id {
            warnings.push(format!(
                "{snapshot_id}: snapshot project id does not match this project"
            ));
            continue;
        }
        let name = snapshot.name.clone();
        summaries.push(summary_from_snapshot(
            snapshot_id,
            &snapshot,
            name,
            Vec::new(),
        ));
    }
    summaries.sort_by(|a, b| {
        b.created_at_utc
            .cmp(&a.created_at_utc)
            .then_with(|| b.checkpoint_id.cmp(&a.checkpoint_id))
    });
    Ok(CheckpointListResult {
        checkpoints: summaries,
        warnings,
    })
}

fn summary_from_snapshot(
    checkpoint_id: SnapshotId,
    snapshot: &SnapshotFile,
    name: String,
    warnings: Vec<String>,
) -> CheckpointSummary {
    CheckpointSummary {
        checkpoint_id,
        name,
        created_at_utc: snapshot.created_at_utc.clone(),
        file_count: snapshot.files.len(),
        logical_size_bytes: snapshot.files.iter().map(|file| file.size_bytes).sum(),
        newly_stored_bytes: 0,
        warnings,
    }
}

fn refresh_fingerprints_after_checkpoint(
    index: &crate::db::IndexConnection,
    project: &crate::ProjectContext,
    scanned: &[ScannedFile],
) -> Result<()> {
    let mut updates = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for file in scanned {
        seen_paths.insert(file.path.clone());
        let Some(fingerprint) = file.fingerprint.clone() else {
            continue;
        };
        updates.push(FileFingerprintUpdate {
            path: file.path.clone(),
            size_bytes: file.size_bytes,
            modified_at_utc: file.modified_at_utc.clone(),
            fingerprint,
            object_id: file.hash.clone(),
        });
    }
    crate::refresh_file_fingerprints_with_index_connection(index, project, &updates, &seen_paths)
}
