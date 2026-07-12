use crate::storage::{object_path_no_follow, open_file_fingerprint_db, replace_file};
use crate::{
    acquire_repository_lock, db_error, ensure_no_pending_transactions, load_snapshot,
    report_operation_progress, CancellationToken, CheckpointIndexState, CheckpointIndexStatus,
    CheckpointSummary, ObjectId, OperationProgress, ProjectContext, RebuildIndexResult, Result,
    SnapshotFile, SnapshotId, StorageSummary, TrackedUnityFilePath,
};
use rusqlite::{params, OpenFlags, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const INDEX_STATE_SNAPSHOT_DIR_FINGERPRINT: &str = "snapshot_dir_fingerprint";
const INDEX_STATE_PROJECT_ID: &str = "project_id";
const SNAPSHOT_INDEX_SCHEMA_VERSION: i64 = 3;
const MAX_REBUILD_OBJECTS_IN_MEMORY: usize = 1_000_000;

fn sqlite_i64_from_u64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        crate::CheckPoError::Corruption(format!(
            "{field} value {value} exceeds SQLite's signed integer range"
        ))
    })
}

fn sqlite_i64_from_usize(value: usize, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        crate::CheckPoError::Corruption(format!(
            "{field} value {value} exceeds SQLite's signed integer range"
        ))
    })
}

fn u64_from_sqlite_i64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        crate::CheckPoError::IndexUnavailable(format!(
            "SQLite index contains a negative {field} value: {value}"
        ))
    })
}

fn usize_from_sqlite_i64(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        crate::CheckPoError::IndexUnavailable(format!(
            "SQLite index contains an invalid {field} value: {value}"
        ))
    })
}

#[derive(Debug, Clone)]
pub struct CachedFileFingerprint {
    pub size_bytes: u64,
    pub fingerprint: String,
    pub object_id: ObjectId,
}

#[derive(Debug, Clone)]
pub struct FileFingerprintUpdate {
    pub path: TrackedUnityFilePath,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub fingerprint: String,
    pub object_id: ObjectId,
}

pub(crate) struct IndexConnection {
    conn: rusqlite::Connection,
    db_path: PathBuf,
}

pub(crate) fn open_index_connection(project: &ProjectContext) -> Result<IndexConnection> {
    let status = checkpoint_index_status(project)?;
    if status.state != CheckpointIndexState::Current {
        return Err(index_unavailable(&status));
    }
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_snapshot_index_read_write(&db_path)?;
    Ok(IndexConnection { conn, db_path })
}

pub fn checkpoint_index_status(project: &ProjectContext) -> Result<CheckpointIndexStatus> {
    let db_path = crate::db_path(&project.repo_root);
    if !db_path.exists() {
        return Ok(CheckpointIndexStatus {
            state: CheckpointIndexState::Missing,
            rebuildable: true,
            detail: Some("checkpoint index has not been built".to_string()),
        });
    }
    let metadata =
        fs::symlink_metadata(&db_path).map_err(|error| crate::io_error(&db_path, error))?;
    if !metadata.file_type().is_file() || crate::metadata_is_link_or_reparse(&metadata) {
        return Ok(CheckpointIndexStatus {
            state: CheckpointIndexState::Corrupt,
            rebuildable: true,
            detail: Some("checkpoint index is not a regular file".to_string()),
        });
    }
    let conn = match open_snapshot_index_read_only(&db_path) {
        Ok(conn) => conn,
        Err(error) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Corrupt,
                rebuildable: true,
                detail: Some(error.to_string()),
            })
        }
    };
    match schema_is_compatible(&conn, &db_path) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Incompatible,
                rebuildable: true,
                detail: Some(format!(
                    "checkpoint index schema is not version {SNAPSHOT_INDEX_SCHEMA_VERSION}"
                )),
            })
        }
        Err(error) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Corrupt,
                rebuildable: true,
                detail: Some(error.to_string()),
            })
        }
    }
    let indexed_project_id = conn
        .query_row(
            "SELECT value FROM index_state WHERE key = ?1",
            params![INDEX_STATE_PROJECT_ID],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| db_error(&db_path, error))?;
    if indexed_project_id.as_deref() != Some(project.project_id.as_str()) {
        return Ok(CheckpointIndexStatus {
            state: CheckpointIndexState::Stale,
            rebuildable: true,
            detail: Some("checkpoint index belongs to a different project".to_string()),
        });
    }
    let indexed = match read_snapshot_dir_fingerprint_with_connection(&conn, &db_path) {
        Ok(value) => value,
        Err(error) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Corrupt,
                rebuildable: true,
                detail: Some(error.to_string()),
            })
        }
    };
    let actual = match snapshot_dir_fingerprint(&project.repo_root) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Corrupt,
                rebuildable: true,
                detail: Some(error.to_string()),
            })
        }
    };
    if indexed.as_deref() != Some(actual.as_str()) {
        return Ok(CheckpointIndexStatus {
            state: CheckpointIndexState::Stale,
            rebuildable: true,
            detail: Some("checkpoint files changed after the index was built".to_string()),
        });
    }
    Ok(CheckpointIndexStatus::current())
}

fn index_unavailable(status: &CheckpointIndexStatus) -> crate::CheckPoError {
    crate::CheckPoError::IndexUnavailable(
        status
            .detail
            .clone()
            .unwrap_or_else(|| format!("checkpoint index is {:?}", status.state)),
    )
}

fn require_current_index(project: &ProjectContext) -> Result<()> {
    let status = checkpoint_index_status(project)?;
    if status.state == CheckpointIndexState::Current {
        Ok(())
    } else {
        Err(index_unavailable(&status))
    }
}

pub(crate) fn index_snapshot_with_index_connection(
    index: &IndexConnection,
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
) -> Result<()> {
    index_snapshot_with_connection(&index.conn, &index.db_path, snapshot_id, snapshot, None)?;
    write_snapshot_dir_fingerprint(
        &index.conn,
        &index.db_path,
        &snapshot_dir_fingerprint(&project.repo_root)?,
    )
}

pub fn list_checkpoint_summaries_from_index(
    project: &ProjectContext,
) -> Result<Vec<CheckpointSummary>> {
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_snapshot_index_read_only(&db_path).map_err(index_read_error)?;
    query_checkpoint_summaries(&conn, &db_path, project).map_err(index_read_error)
}

pub fn storage_summary_from_index(project: &ProjectContext) -> Result<StorageSummary> {
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_snapshot_index_read_only(&db_path).map_err(index_read_error)?;
    query_storage_summary(&conn, &db_path, project).map_err(index_read_error)
}

pub fn checkpoint_summaries_and_storage_summary_from_index(
    project: &ProjectContext,
) -> Result<(Vec<CheckpointSummary>, StorageSummary)> {
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_snapshot_index_read_only(&db_path).map_err(index_read_error)?;
    let mut checkpoints =
        query_checkpoint_summaries(&conn, &db_path, project).map_err(index_read_error)?;
    let _ = crate::apply_checkpoint_name_overrides(project, &mut checkpoints);
    let storage = query_storage_summary(&conn, &db_path, project).map_err(index_read_error)?;
    Ok((checkpoints, storage))
}

fn index_read_error(error: crate::CheckPoError) -> crate::CheckPoError {
    match error {
        crate::CheckPoError::IndexUnavailable(_) => error,
        other => crate::CheckPoError::IndexUnavailable(other.to_string()),
    }
}

fn query_storage_summary(
    conn: &rusqlite::Connection,
    db_path: &Path,
    project: &ProjectContext,
) -> Result<StorageSummary> {
    let (checkpoint_count, logical_size_bytes) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(logical_size_bytes), 0)
             FROM snapshots WHERE project_id = ?1",
            params![project.project_id.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|error| db_error(db_path, error))?;
    let checkpoint_count = usize_from_sqlite_i64(checkpoint_count, "checkpoint count")?;
    let logical_size_bytes = u64_from_sqlite_i64(logical_size_bytes, "logical size in bytes")?;
    let (unique_blob_count, stored_size_bytes) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(present_size_bytes), 0) FROM object_refs",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|error| db_error(db_path, error))?;
    let unique_blob_count = usize_from_sqlite_i64(unique_blob_count, "unique blob count")?;
    let stored_size_bytes = u64_from_sqlite_i64(stored_size_bytes, "stored size in bytes")?;

    Ok(StorageSummary {
        checkpoint_count,
        unique_blob_count,
        logical_size_bytes,
        stored_size_bytes,
    })
}

pub fn load_file_fingerprints(
    project: &ProjectContext,
) -> Result<BTreeMap<TrackedUnityFilePath, CachedFileFingerprint>> {
    let db_path = crate::file_fingerprint_db_path(&project.repo_root);
    if !db_path.exists() {
        return Ok(BTreeMap::new());
    }
    let conn = open_file_fingerprint_db(&project.repo_root)?;
    create_fingerprint_schema(&conn, &db_path)?;
    load_file_fingerprints_with_connection(&conn, &db_path, project)
}

fn load_file_fingerprints_with_connection(
    conn: &rusqlite::Connection,
    db_path: &Path,
    project: &ProjectContext,
) -> Result<BTreeMap<TrackedUnityFilePath, CachedFileFingerprint>> {
    let mut statement = conn
        .prepare(
            "SELECT path, size_bytes, fingerprint, object_id
             FROM file_fingerprints
             WHERE project_id = ?1",
        )
        .map_err(|error| db_error(db_path, error))?;
    let rows = statement
        .query_map(params![project.project_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| db_error(db_path, error))?;
    let mut records = BTreeMap::new();
    for row in rows {
        let (path, size_bytes, fingerprint, object_id) =
            row.map_err(|error| db_error(db_path, error))?;
        let size_bytes = u64_from_sqlite_i64(size_bytes, "file fingerprint size in bytes")?;
        let Ok(path) = TrackedUnityFilePath::parse(&path) else {
            continue;
        };
        let Ok(object_id) = ObjectId::parse(&object_id) else {
            continue;
        };
        records.insert(
            path,
            CachedFileFingerprint {
                size_bytes,
                fingerprint,
                object_id,
            },
        );
    }
    Ok(records)
}

pub(crate) fn refresh_file_fingerprints(
    project: &ProjectContext,
    updates: &[FileFingerprintUpdate],
    seen_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<()> {
    let db_path = crate::file_fingerprint_db_path(&project.repo_root);
    let conn = open_file_fingerprint_db(&project.repo_root)?;
    create_fingerprint_schema(&conn, &db_path)?;
    let existing = load_file_fingerprints_with_connection(&conn, &db_path, project)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(&db_path, error))?;
    {
        let mut statement = tx
            .prepare(
                "INSERT OR REPLACE INTO file_fingerprints
                 (project_id, path, size_bytes, modified_at_utc, fingerprint, object_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| db_error(&db_path, error))?;
        for update in updates {
            let size_bytes =
                sqlite_i64_from_u64(update.size_bytes, "file fingerprint size in bytes")?;
            statement
                .execute(params![
                    project.project_id.as_str(),
                    update.path.as_str(),
                    size_bytes,
                    update.modified_at_utc.as_str(),
                    update.fingerprint.as_str(),
                    update.object_id.as_str(),
                ])
                .map_err(|error| db_error(&db_path, error))?;
        }
    }
    for path in existing.keys().filter(|path| !seen_paths.contains(*path)) {
        tx.execute(
            "DELETE FROM file_fingerprints WHERE project_id = ?1 AND path = ?2",
            params![project.project_id.as_str(), path.as_str()],
        )
        .map_err(|error| db_error(&db_path, error))?;
    }
    tx.commit().map_err(|error| db_error(&db_path, error))
}

pub fn invalidate_file_fingerprints(
    project: &ProjectContext,
    paths: &[TrackedUnityFilePath],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    match delete_file_fingerprints(project, paths) {
        Ok(()) => Ok(()),
        Err(_) => remove_file_if_exists(&crate::file_fingerprint_db_path(&project.repo_root)),
    }
}

pub fn rebuild_index(project_path: impl AsRef<std::path::Path>) -> Result<RebuildIndexResult> {
    let project = crate::load_project(project_path)?;
    rebuild_index_for_project_with_progress_and_cancellation(&project, None, None)
}

pub fn rebuild_index_for_project_with_progress_and_cancellation(
    project: &ProjectContext,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<RebuildIndexResult> {
    crate::ensure_project_location_allows_mutation(project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "index-rebuild")?;
    ensure_no_pending_transactions(project)?;
    rebuild_index_for_project_unlocked(project, progress, cancellation)
}

pub(crate) fn rebuild_index_for_project_unlocked(
    project: &ProjectContext,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<RebuildIndexResult> {
    crate::ensure_not_cancelled(cancellation)?;
    let before = snapshot_dir_fingerprint(&project.repo_root)?;
    let live_path = crate::db_path(&project.repo_root);
    let indexes_dir = live_path
        .parent()
        .ok_or_else(|| crate::CheckPoError::Unexpected("index path has no parent".to_string()))?;
    crate::create_absolute_dir_all_no_follow(indexes_dir)?;
    cleanup_owned_index_temporary_files(indexes_dir)?;
    let temp_path = indexes_dir.join(format!("snapshot-index-{}.tmp", Uuid::new_v4().simple()));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|error| crate::io_error(&temp_path, error))?;
    let result = match build_snapshot_index(project, &temp_path, &before, progress, cancellation) {
        Ok(result) => result,
        Err(error) => {
            let _ = remove_file_if_exists(&temp_path);
            return Err(error);
        }
    };
    crate::ensure_not_cancelled(cancellation)?;
    if before != snapshot_dir_fingerprint(&project.repo_root)? {
        let _ = remove_file_if_exists(&temp_path);
        return Err(crate::CheckPoError::IndexUnavailable(
            "checkpoint files changed while the index was being rebuilt".to_string(),
        ));
    }
    report_operation_progress(progress, "committingIndex", 0, 0, None);
    if let Err(error) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp_path)
        .and_then(|file| file.sync_all())
    {
        let _ = remove_file_if_exists(&temp_path);
        return Err(crate::io_error(&temp_path, error));
    }
    if let Err(error) = replace_snapshot_index_with_retry(&temp_path, &live_path) {
        let _ = remove_file_if_exists(&temp_path);
        return Err(error);
    }
    crate::sync_parent_dir(&live_path)?;
    Ok(result)
}

fn build_snapshot_index(
    project: &ProjectContext,
    db_path: &Path,
    source_fingerprint: &str,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<RebuildIndexResult> {
    let conn = rusqlite::Connection::open(db_path).map_err(|error| db_error(db_path, error))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| db_error(db_path, error))?;
    create_schema(&conn, db_path)?;
    let mut snapshot_count = 0_usize;
    let mut missing_object_count = 0_usize;
    let snapshot_ids = crate::list_snapshot_ids(&project.repo_root)?;
    let total = snapshot_ids.len();
    let mut aggregate_objects = BTreeMap::<ObjectId, (u64, u64)>::new();
    let mut aggregated_object_count = 0_usize;
    for (index, snapshot_id) in snapshot_ids.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        let snapshot = load_snapshot(&project.repo_root, snapshot_id)?;
        if snapshot.project_id != project.project_id {
            return Err(crate::CheckPoError::Corruption(format!(
                "snapshot {snapshot_id} belongs to a different project"
            )));
        }
        insert_snapshot_summary(&conn, db_path, snapshot_id, &snapshot)?;
        let mut snapshot_objects = BTreeMap::<ObjectId, u64>::new();
        for (file_index, file) in snapshot.files.iter().enumerate() {
            if file_index % 256 == 0 {
                crate::ensure_not_cancelled(cancellation)?;
            }
            match snapshot_objects.insert(file.content_hash().clone(), file.size_bytes) {
                Some(existing) if existing != file.size_bytes => {
                    return Err(crate::CheckPoError::Corruption(format!(
                        "snapshot {snapshot_id} gives object {} conflicting sizes {existing} and {}",
                        file.content_hash(),
                        file.size_bytes
                    )));
                }
                _ => {}
            }
        }
        for (object_id, size) in snapshot_objects {
            match aggregate_objects.get_mut(&object_id) {
                Some((expected_size, _)) if *expected_size != size => {
                    return Err(crate::CheckPoError::Corruption(format!(
                        "object {object_id} has conflicting expected sizes {expected_size} and {size}"
                    )));
                }
                Some((_, reference_count)) => {
                    *reference_count = reference_count.checked_add(1).ok_or_else(|| {
                        crate::CheckPoError::Corruption(format!(
                            "object {object_id} reference count exceeds the supported range"
                        ))
                    })?;
                }
                None => {
                    aggregate_objects.insert(object_id, (size, 1));
                }
            }
        }
        if aggregate_objects.len() >= MAX_REBUILD_OBJECTS_IN_MEMORY {
            flush_aggregate_object_refs(
                &conn,
                db_path,
                &mut aggregate_objects,
                &mut aggregated_object_count,
                progress,
                cancellation,
            )?;
        }
        snapshot_count += 1;
        report_operation_progress(
            progress,
            "readingSnapshots",
            index + 1,
            total,
            Some(snapshot_id.to_string()),
        );
    }
    flush_aggregate_object_refs(
        &conn,
        db_path,
        &mut aggregate_objects,
        &mut aggregated_object_count,
        progress,
        cancellation,
    )?;

    let object_count = conn
        .query_row("SELECT COUNT(*) FROM object_refs", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| db_error(db_path, error))?;
    let object_count = usize_from_sqlite_i64(object_count, "referenced object count")?;
    let mut checked_object_count = 0_usize;
    let mut last_object_id = String::new();
    loop {
        let mut statement = conn
            .prepare(
                "SELECT object_id, expected_size_bytes FROM object_refs
                 WHERE object_id > ?1 ORDER BY object_id LIMIT 4096",
            )
            .map_err(|error| db_error(db_path, error))?;
        let rows = statement
            .query_map(params![last_object_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| db_error(db_path, error))?;
        let mut objects = Vec::with_capacity(4096);
        for row in rows {
            objects.push(row.map_err(|error| db_error(db_path, error))?);
        }
        drop(statement);
        let Some((page_last_id, _)) = objects.last() else {
            break;
        };
        last_object_id = page_last_id.clone();

        let tx = conn
            .unchecked_transaction()
            .map_err(|error| db_error(db_path, error))?;
        {
            let mut update = tx
                .prepare("UPDATE object_refs SET present_size_bytes = ?2 WHERE object_id = ?1")
                .map_err(|error| db_error(db_path, error))?;
            for (object_id, expected_size) in objects {
                if checked_object_count.is_multiple_of(256) {
                    crate::ensure_not_cancelled(cancellation)?;
                }
                let object_id = ObjectId::parse(&object_id)?;
                let present_size = object_path_no_follow(&project.repo_root, &object_id)
                    .ok()
                    .and_then(|path| fs::symlink_metadata(path).ok())
                    .filter(|metadata| {
                        metadata.file_type().is_file()
                            && !crate::metadata_is_link_or_reparse(metadata)
                            && i64::try_from(metadata.len()).ok() == Some(expected_size)
                    })
                    .map(|_| expected_size);
                if present_size.is_none() {
                    missing_object_count += 1;
                }
                update
                    .execute(params![object_id.as_str(), present_size])
                    .map_err(|error| db_error(db_path, error))?;
                checked_object_count += 1;
                report_operation_progress(
                    progress,
                    "checkingObjects",
                    checked_object_count,
                    object_count,
                    Some(object_id.to_string()),
                );
            }
        }
        tx.commit().map_err(|error| db_error(db_path, error))?;
    }

    write_snapshot_dir_fingerprint(&conn, db_path, source_fingerprint)?;
    conn.execute(
        "INSERT OR REPLACE INTO index_state(key, value) VALUES(?1, ?2)",
        params![INDEX_STATE_PROJECT_ID, project.project_id.as_str()],
    )
    .map_err(|error| db_error(db_path, error))?;
    let integrity = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .map_err(|error| db_error(db_path, error))?;
    if integrity != "ok" {
        return Err(crate::CheckPoError::IndexUnavailable(format!(
            "rebuilt index failed integrity_check: {integrity}"
        )));
    }
    drop(conn);
    Ok(RebuildIndexResult {
        snapshot_count,
        referenced_object_count: object_count,
        unavailable_referenced_object_count: missing_object_count,
        errors: Vec::new(),
    })
}

fn flush_aggregate_object_refs(
    conn: &rusqlite::Connection,
    db_path: &Path,
    aggregate_objects: &mut BTreeMap<ObjectId, (u64, u64)>,
    aggregated_object_count: &mut usize,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    if aggregate_objects.is_empty() {
        return Ok(());
    }
    let batch = std::mem::take(aggregate_objects);
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(db_path, error))?;
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO object_refs(
                   object_id, expected_size_bytes, reference_count, present_size_bytes
                 ) VALUES(?1, ?2, ?3, NULL)
                 ON CONFLICT(object_id) DO UPDATE SET
                   reference_count = reference_count + excluded.reference_count
                 WHERE expected_size_bytes = excluded.expected_size_bytes",
            )
            .map_err(|error| db_error(db_path, error))?;
        for (index, (object_id, (size, reference_count))) in batch.into_iter().enumerate() {
            if index % 256 == 0 {
                crate::ensure_not_cancelled(cancellation)?;
            }
            let changed = insert
                .execute(params![
                    object_id.as_str(),
                    sqlite_i64_from_u64(size, "object expected size in bytes")?,
                    sqlite_i64_from_u64(reference_count, "object reference count")?,
                ])
                .map_err(|error| db_error(db_path, error))?;
            if changed != 1 {
                return Err(crate::CheckPoError::Corruption(format!(
                    "object {object_id} has conflicting expected sizes across aggregate batches"
                )));
            }
            *aggregated_object_count = aggregated_object_count.checked_add(1).ok_or_else(|| {
                crate::CheckPoError::Corruption(
                    "aggregated object count exceeds the supported range".to_string(),
                )
            })?;
            report_operation_progress(
                progress,
                "aggregatingReferences",
                *aggregated_object_count,
                0,
                Some(object_id.to_string()),
            );
        }
    }
    tx.commit().map_err(|error| db_error(db_path, error))
}

fn insert_snapshot_summary(
    conn: &rusqlite::Connection,
    db_path: &Path,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
) -> Result<()> {
    let logical_size = snapshot.files.iter().try_fold(0_u64, |total, file| {
        total.checked_add(file.size_bytes).ok_or_else(|| {
            crate::CheckPoError::Corruption(format!(
                "snapshot {snapshot_id} logical size exceeds the supported range"
            ))
        })
    })?;
    conn.execute(
        "INSERT INTO snapshots(
           snapshot_id, project_id, created_at_utc, name, parent_snapshot_id,
           file_count, logical_size_bytes
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id.as_str(),
            snapshot.project_id.as_str(),
            snapshot.created_at_utc.as_str(),
            snapshot.name.as_str(),
            snapshot.parent_snapshot_id.as_ref().map(|id| id.as_str()),
            sqlite_i64_from_usize(snapshot.files.len(), "snapshot file count")?,
            sqlite_i64_from_u64(logical_size, "snapshot logical size in bytes")?,
        ],
    )
    .map_err(|error| db_error(db_path, error))?;
    Ok(())
}

fn index_snapshot_with_connection(
    conn: &rusqlite::Connection,
    db_path: &Path,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    let file_count = sqlite_i64_from_usize(snapshot.files.len(), "snapshot file count")?;
    let logical_size_bytes = snapshot.files.iter().try_fold(0_u64, |total, file| {
        total.checked_add(file.size_bytes).ok_or_else(|| {
            crate::CheckPoError::Corruption(format!(
                "snapshot {snapshot_id} logical size exceeds the supported range"
            ))
        })
    })?;
    let logical_size_bytes =
        sqlite_i64_from_u64(logical_size_bytes, "snapshot logical size in bytes")?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(db_path, error))?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO snapshots(snapshot_id, project_id, created_at_utc, name, parent_snapshot_id, file_count, logical_size_bytes)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id.as_str(),
            snapshot.project_id.as_str(),
            snapshot.created_at_utc.as_str(),
            snapshot.name.as_str(),
            snapshot.parent_snapshot_id.as_ref().map(|id| id.as_str()),
            file_count,
            logical_size_bytes,
        ],
    )
    .map_err(|error| db_error(db_path, error))?;
    if inserted == 0 {
        let expected = (
            snapshot.project_id.to_string(),
            snapshot.created_at_utc.clone(),
            snapshot.name.clone(),
            snapshot
                .parent_snapshot_id
                .as_ref()
                .map(ToString::to_string),
            file_count,
            logical_size_bytes,
        );
        let existing = tx
            .query_row(
                "SELECT project_id, created_at_utc, name, parent_snapshot_id, file_count, logical_size_bytes
                 FROM snapshots WHERE snapshot_id = ?1",
                params![snapshot_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .map_err(|error| db_error(db_path, error))?;
        if existing != expected {
            return Err(crate::CheckPoError::IndexUnavailable(format!(
                "indexed checkpoint {snapshot_id} summary does not match its snapshot"
            )));
        }
        tx.commit().map_err(|error| db_error(db_path, error))?;
        return Ok(());
    }
    let mut unique_objects = BTreeMap::<ObjectId, u64>::new();
    for (index, file) in snapshot.files.iter().enumerate() {
        if index % 256 == 0 {
            crate::ensure_not_cancelled(cancellation)?;
        }
        match unique_objects.insert(file.content_hash().clone(), file.size_bytes) {
            Some(existing) if existing != file.size_bytes => {
                return Err(crate::CheckPoError::Corruption(format!(
                    "snapshot {snapshot_id} gives object {} conflicting sizes {existing} and {}",
                    file.content_hash(),
                    file.size_bytes
                )));
            }
            _ => {}
        }
    }
    for (object_id, size) in unique_objects {
        increment_object_ref(&tx, db_path, &object_id, size)?;
    }
    tx.commit().map_err(|error| db_error(db_path, error))
}

fn increment_object_ref(
    tx: &rusqlite::Transaction<'_>,
    db_path: &Path,
    object_id: &ObjectId,
    size: u64,
) -> Result<()> {
    let size = sqlite_i64_from_u64(size, "object expected size in bytes")?;
    let existing = tx
        .query_row(
            "SELECT expected_size_bytes FROM object_refs WHERE object_id = ?1",
            params![object_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| db_error(db_path, error))?;
    match existing {
        Some(existing) if existing != size => Err(crate::CheckPoError::Corruption(format!(
            "object {object_id} has conflicting expected sizes {existing} and {size}"
        ))),
        Some(_) => {
            tx.execute(
                "UPDATE object_refs
                 SET reference_count = reference_count + 1, present_size_bytes = ?2
                 WHERE object_id = ?1",
                params![object_id.as_str(), size],
            )
            .map_err(|error| db_error(db_path, error))?;
            Ok(())
        }
        None => {
            tx.execute(
                "INSERT INTO object_refs(object_id, expected_size_bytes, reference_count, present_size_bytes)
                 VALUES(?1, ?2, 1, ?2)",
                params![object_id.as_str(), size],
            )
            .map_err(|error| db_error(db_path, error))?;
            Ok(())
        }
    }
}

pub(crate) fn delete_snapshot_from_index(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
) -> Result<()> {
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_snapshot_index_read_write(&db_path)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(&db_path, error))?;
    let mut unique_objects = BTreeMap::<ObjectId, u64>::new();
    for file in &snapshot.files {
        match unique_objects.insert(file.content_hash().clone(), file.size_bytes) {
            Some(existing) if existing != file.size_bytes => {
                return Err(crate::CheckPoError::Corruption(format!(
                    "snapshot {snapshot_id} contains conflicting sizes for object {}",
                    file.content_hash()
                )));
            }
            _ => {}
        }
    }
    for (object_id, size) in unique_objects {
        let expected = sqlite_i64_from_u64(size, "object expected size in bytes")?;
        let row = tx
            .query_row(
                "SELECT expected_size_bytes, reference_count FROM object_refs WHERE object_id = ?1",
                params![object_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| db_error(&db_path, error))?;
        let Some((indexed_size, reference_count)) = row else {
            return Err(crate::CheckPoError::IndexUnavailable(format!(
                "index is missing object reference {object_id}"
            )));
        };
        if indexed_size != expected || reference_count <= 0 {
            return Err(crate::CheckPoError::IndexUnavailable(format!(
                "index contains an invalid reference for object {object_id}"
            )));
        }
        if reference_count == 1 {
            tx.execute(
                "DELETE FROM object_refs WHERE object_id = ?1",
                params![object_id.as_str()],
            )
        } else {
            tx.execute(
                "UPDATE object_refs SET reference_count = reference_count - 1 WHERE object_id = ?1",
                params![object_id.as_str()],
            )
        }
        .map_err(|error| db_error(&db_path, error))?;
    }
    let deleted_snapshots = tx
        .execute(
            "DELETE FROM snapshots WHERE snapshot_id = ?1 AND project_id = ?2",
            params![snapshot_id.as_str(), project.project_id.as_str()],
        )
        .map_err(|error| db_error(&db_path, error))?;
    if deleted_snapshots != 1 {
        return Err(crate::CheckPoError::IndexUnavailable(format!(
            "SQLite index did not contain checkpoint {snapshot_id}"
        )));
    }
    tx.commit().map_err(|error| db_error(&db_path, error))?;
    write_snapshot_dir_fingerprint(
        &conn,
        &db_path,
        &snapshot_dir_fingerprint(&project.repo_root)?,
    )
}

fn query_checkpoint_summaries(
    conn: &rusqlite::Connection,
    db_path: &Path,
    project: &ProjectContext,
) -> Result<Vec<CheckpointSummary>> {
    let mut statement = conn
        .prepare(
            "SELECT snapshot_id, name, created_at_utc, file_count, logical_size_bytes
             FROM snapshots
             WHERE project_id = ?1
             ORDER BY created_at_utc DESC, snapshot_id DESC",
        )
        .map_err(|error| db_error(db_path, error))?;
    let rows = statement
        .query_map(params![project.project_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| db_error(db_path, error))?;
    let mut summaries = Vec::new();
    for row in rows {
        let (snapshot_id, name, created_at_utc, file_count, logical_size_bytes) =
            row.map_err(|error| db_error(db_path, error))?;
        let file_count = usize_from_sqlite_i64(file_count, "snapshot file count")?;
        let logical_size_bytes =
            u64_from_sqlite_i64(logical_size_bytes, "snapshot logical size in bytes")?;
        summaries.push(CheckpointSummary {
            checkpoint_id: SnapshotId::parse(&snapshot_id)?,
            name,
            created_at_utc,
            file_count,
            logical_size_bytes,
            newly_stored_bytes: 0,
            warnings: Vec::new(),
        });
    }
    Ok(summaries)
}

fn delete_file_fingerprints(
    project: &ProjectContext,
    paths: &[TrackedUnityFilePath],
) -> Result<()> {
    let db_path = crate::file_fingerprint_db_path(&project.repo_root);
    if !db_path.exists() {
        return Ok(());
    }
    let conn = open_file_fingerprint_db(&project.repo_root)?;
    create_fingerprint_schema(&conn, &db_path)?;
    for path in paths {
        conn.execute(
            "DELETE FROM file_fingerprints WHERE project_id = ?1 AND path = ?2",
            params![project.project_id.as_str(), path.as_str()],
        )
        .map_err(|error| db_error(&db_path, error))?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::io_error(path, error)),
    }
}

fn cleanup_owned_index_temporary_files(indexes_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(indexes_dir).map_err(|error| crate::io_error(indexes_dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(indexes_dir, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let base = name.strip_suffix("-journal").unwrap_or(name);
        let Some(id) = base
            .strip_prefix("snapshot-index-")
            .and_then(|value| value.strip_suffix(".tmp"))
        else {
            continue;
        };
        if id.len() != 32
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            continue;
        }
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| crate::io_error(&path, error))?;
        if metadata.file_type().is_file() && !crate::metadata_is_link_or_reparse(&metadata) {
            fs::remove_file(&path).map_err(|error| crate::io_error(&path, error))?;
        } else {
            crate::diagnostics::log_warning(
                "index-rebuild",
                &format!(
                    "unsafe index temporary path was not removed: {}",
                    path.display()
                ),
            );
        }
    }
    Ok(())
}

fn snapshot_dir_fingerprint(repo_root: &Path) -> Result<String> {
    let ids = crate::list_snapshot_ids(repo_root)?;
    let mut hasher = blake3::Hasher::new();
    for id in ids {
        let path = crate::snapshot_path(repo_root, &id);
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| crate::io_error(&path, error))?;
        if !metadata.file_type().is_file() || crate::metadata_is_link_or_reparse(&metadata) {
            return Err(crate::CheckPoError::Corruption(format!(
                "snapshot is not a regular file: {}",
                path.display()
            )));
        }
        let modified = metadata
            .modified()
            .map_err(|error| crate::io_error(&path, error))?;
        hasher.update(id.as_str().as_bytes());
        hasher.update(&metadata.len().to_le_bytes());
        hasher.update(crate::canonical_utc(modified).as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn read_snapshot_dir_fingerprint_with_connection(
    conn: &rusqlite::Connection,
    db_path: &Path,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM index_state WHERE key = ?1",
        params![INDEX_STATE_SNAPSHOT_DIR_FINGERPRINT],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| db_error(db_path, error))
}

fn write_snapshot_dir_fingerprint(
    conn: &rusqlite::Connection,
    db_path: &Path,
    fingerprint: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO index_state(key, value) VALUES(?1, ?2)",
        params![INDEX_STATE_SNAPSHOT_DIR_FINGERPRINT, fingerprint],
    )
    .map_err(|error| db_error(db_path, error))?;
    Ok(())
}

fn create_schema(conn: &rusqlite::Connection, db_path: &std::path::Path) -> Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = DELETE;
        CREATE TABLE snapshots(
          snapshot_id TEXT PRIMARY KEY,
          project_id TEXT NOT NULL,
          created_at_utc TEXT NOT NULL,
          name TEXT NOT NULL,
          parent_snapshot_id TEXT,
          file_count INTEGER NOT NULL,
          logical_size_bytes INTEGER NOT NULL CHECK(logical_size_bytes >= 0)
        ) WITHOUT ROWID;
        CREATE TABLE object_refs(
          object_id TEXT PRIMARY KEY,
          expected_size_bytes INTEGER NOT NULL CHECK(expected_size_bytes >= 0),
          reference_count INTEGER NOT NULL CHECK(reference_count > 0),
          present_size_bytes INTEGER
        ) WITHOUT ROWID;
        CREATE TABLE index_state(
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        ) WITHOUT ROWID;
        PRAGMA user_version = {SNAPSHOT_INDEX_SCHEMA_VERSION};"
    ))
    .map_err(|error| db_error(db_path, error))
}

fn create_fingerprint_schema(conn: &rusqlite::Connection, db_path: &std::path::Path) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_fingerprints(
          project_id TEXT NOT NULL,
          path TEXT NOT NULL,
          size_bytes INTEGER NOT NULL,
          modified_at_utc TEXT NOT NULL,
          fingerprint TEXT NOT NULL,
          object_id TEXT NOT NULL,
          PRIMARY KEY(project_id, path)
        ) WITHOUT ROWID;
        PRAGMA user_version = 1;",
    )
    .map_err(|error| db_error(db_path, error))
}

fn schema_is_compatible(conn: &rusqlite::Connection, db_path: &std::path::Path) -> Result<bool> {
    let version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|error| db_error(db_path, error))?;
    if version != SNAPSHOT_INDEX_SCHEMA_VERSION {
        return Ok(false);
    }
    let existing_tables = user_tables(conn, db_path)?;
    let expected = expected_schema();
    if existing_tables.len() != expected.len() {
        return Ok(false);
    }
    for (table, expected_columns) in expected {
        if !existing_tables.iter().any(|existing| existing == table) {
            return Ok(false);
        }
        let columns = table_columns(conn, db_path, table)?;
        if columns != expected_columns {
            return Ok(false);
        }
    }
    Ok(true)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn user_tables(conn: &rusqlite::Connection, db_path: &std::path::Path) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .map_err(|error| db_error(db_path, error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| db_error(db_path, error))?;
    let mut tables = Vec::new();
    for row in rows {
        tables.push(row.map_err(|error| db_error(db_path, error))?);
    }
    Ok(tables)
}

fn table_columns(
    conn: &rusqlite::Connection,
    db_path: &std::path::Path,
    table: &str,
) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({})", quote_identifier(table)))
        .map_err(|error| db_error(db_path, error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| db_error(db_path, error))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row.map_err(|error| db_error(db_path, error))?);
    }
    Ok(columns)
}

fn expected_schema() -> Vec<(&'static str, Vec<String>)> {
    vec![
        ("index_state", vec!["key", "value"]),
        (
            "object_refs",
            vec![
                "object_id",
                "expected_size_bytes",
                "reference_count",
                "present_size_bytes",
            ],
        ),
        (
            "snapshots",
            vec![
                "snapshot_id",
                "project_id",
                "created_at_utc",
                "name",
                "parent_snapshot_id",
                "file_count",
                "logical_size_bytes",
            ],
        ),
    ]
    .into_iter()
    .map(|(table, columns)| {
        (
            table,
            columns.into_iter().map(str::to_string).collect::<Vec<_>>(),
        )
    })
    .collect()
}

fn open_snapshot_index_read_only(path: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| db_error(path, error))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    Ok(conn)
}

fn open_snapshot_index_read_write(path: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| db_error(path, error))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    if !schema_is_compatible(&conn, path)? {
        return Err(crate::CheckPoError::IndexUnavailable(
            "checkpoint index schema is incompatible".to_string(),
        ));
    }
    Ok(conn)
}

fn replace_snapshot_index_with_retry(temp_path: &Path, destination: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0_u64..4 {
        match replace_file(temp_path, destination) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
    }
    Err(last_error.expect("index replacement was attempted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_columns_quotes_table_identifier() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE \"legacy-cache\"(value TEXT);")
            .unwrap();

        let columns = table_columns(&conn, Path::new(":memory:"), "legacy-cache").unwrap();

        assert_eq!(columns, vec!["value".to_string()]);
    }

    #[test]
    fn sqlite_integer_conversions_reject_negative_and_oversized_values() {
        assert!(matches!(
            u64_from_sqlite_i64(-1, "size"),
            Err(crate::CheckPoError::IndexUnavailable(_))
        ));
        assert!(matches!(
            usize_from_sqlite_i64(-1, "count"),
            Err(crate::CheckPoError::IndexUnavailable(_))
        ));
        assert!(matches!(
            sqlite_i64_from_u64(u64::MAX, "size"),
            Err(crate::CheckPoError::Corruption(_))
        ));
    }

    #[test]
    fn aggregate_batches_accumulate_counts_and_reject_size_conflicts() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_schema(&conn, Path::new(":memory:")).unwrap();
        let object_id = ObjectId::parse(&"0".repeat(64)).unwrap();
        let mut processed = 0;
        let mut first = BTreeMap::from([(object_id.clone(), (12, 2))]);
        flush_aggregate_object_refs(
            &conn,
            Path::new(":memory:"),
            &mut first,
            &mut processed,
            None,
            None,
        )
        .unwrap();
        let mut second = BTreeMap::from([(object_id.clone(), (12, 3))]);
        flush_aggregate_object_refs(
            &conn,
            Path::new(":memory:"),
            &mut second,
            &mut processed,
            None,
            None,
        )
        .unwrap();
        let count = conn
            .query_row(
                "SELECT reference_count FROM object_refs WHERE object_id = ?1",
                params![object_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(count, 5);

        let mut conflicting = BTreeMap::from([(object_id, (13, 1))]);
        assert!(matches!(
            flush_aggregate_object_refs(
                &conn,
                Path::new(":memory:"),
                &mut conflicting,
                &mut processed,
                None,
                None,
            ),
            Err(crate::CheckPoError::Corruption(_))
        ));
    }

    #[test]
    fn owned_index_temporary_cleanup_is_strict() {
        let temp = tempfile::tempdir().unwrap();
        let indexes = temp.path();
        let owned = indexes.join(format!("snapshot-index-{}.tmp", "a".repeat(32)));
        let journal = indexes.join(format!(
            "{}-journal",
            owned.file_name().unwrap().to_string_lossy()
        ));
        let near_match = indexes.join(format!("snapshot-index-{}.tmp", "g".repeat(32)));
        fs::write(&owned, b"temp").unwrap();
        fs::write(&journal, b"journal").unwrap();
        fs::write(&near_match, b"keep").unwrap();

        cleanup_owned_index_temporary_files(indexes).unwrap();

        assert!(!owned.exists());
        assert!(!journal.exists());
        assert!(near_match.exists());
    }
}
