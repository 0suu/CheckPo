use crate::{
    acquire_repository_lock, ensure_no_pending_transactions, list_snapshot_ids, load_project,
    load_project_snapshot, load_snapshot, now_utc_string, object_path,
    put_object_from_file_with_known_hash, read_latest_snapshot_id, report_operation_progress,
    save_snapshot, scan_project_for_checkpoint, write_latest_snapshot_id, CheckpointDeleteResult,
    CheckpointSummary, CreateCheckpointOptions, FileFingerprintUpdate, Result, ScannedFile,
    SnapshotContent, SnapshotEntry, SnapshotFile, SnapshotId,
};
use std::collections::BTreeSet;
use std::fs;
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
    crate::ensure_not_cancelled(options.cancellation.as_ref())?;
    let progress = options.progress.as_deref().map(|f| f as &dyn Fn(_));
    let (scanned, scan_warnings, incomplete) =
        scan_project_for_checkpoint(&project, progress, options.cancellation.as_ref())?;
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
    for (index, file) in scanned.iter().enumerate() {
        crate::ensure_not_cancelled(options.cancellation.as_ref())?;
        let (object_id, created, size_bytes) = put_scanned_file(&project.repo_root, file)?;
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
    let parent_snapshot_id = read_latest_snapshot_id(&project.repo_root)?;
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

fn put_scanned_file(repo_root: &Path, file: &ScannedFile) -> Result<(crate::ObjectId, bool, u64)> {
    let object = object_path(repo_root, &file.hash);
    if object.is_file() {
        ensure_scanned_file_still_matches(file)?;
        let metadata = fs::metadata(&object).map_err(|error| crate::io_error(&object, error))?;
        // Existing objects are content-addressed and verified when written.
        // Normal checkpoint creation keeps the hot path fast; full integrity checks belong to verify.
        if metadata.is_file() && metadata.len() == file.size_bytes {
            return Ok((file.hash.clone(), false, file.size_bytes));
        }
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
    let mut checkpoints = match crate::list_checkpoint_summaries_from_index(project) {
        Ok(checkpoints) => checkpoints,
        Err(_) if project.location_status == crate::ProjectLocationStatus::CopiedSuspected => {
            list_checkpoints_from_snapshots(project)?
        }
        Err(error) => return Err(error),
    };
    crate::apply_checkpoint_name_overrides(project, &mut checkpoints);
    Ok(checkpoints)
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
    let id = SnapshotId::parse(checkpoint_id)?;
    let snapshot = load_project_snapshot(&project, &id)?;
    let (mut names, warnings) = crate::read_checkpoint_name_overrides(&project);
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
        warnings,
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
    let id = SnapshotId::parse(checkpoint_id)?;
    let path = crate::snapshot_path(&project.repo_root, &id);
    if !path.is_file() {
        return Err(crate::CheckPoError::SnapshotNotFound(id.to_string()));
    }
    load_project_snapshot(&project, &id)?;
    let mut remaining_checkpoints = list_checkpoints_from_snapshots(&project)?;
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
) -> Result<Vec<CheckpointSummary>> {
    let mut summaries = Vec::new();
    for snapshot_id in list_snapshot_ids(&project.repo_root)? {
        let snapshot = match load_snapshot(&project.repo_root, &snapshot_id) {
            Ok(snapshot) => snapshot,
            Err(_) => continue,
        };
        if snapshot.project_id != project.project_id {
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
    Ok(summaries)
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
