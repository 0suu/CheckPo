use crate::{
    acquire_repository_lock, db_error, ensure_no_pending_transactions, load_snapshot, object_path,
    open_db, report_operation_progress, CancellationToken, CheckpointSummary, ObjectId,
    OperationProgress, ProjectContext, RebuildIndexResult, Result, SnapshotFile, SnapshotId,
    StorageSummary, TrackedUnityFilePath,
};
use rusqlite::{params, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const INDEX_STATE_SNAPSHOT_DIR_FINGERPRINT: &str = "snapshot_dir_fingerprint";

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
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    Ok(IndexConnection { conn, db_path })
}

pub(crate) fn index_snapshot_with_index_connection(
    index: &IndexConnection,
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
) -> Result<()> {
    index_snapshot_with_connection(
        &index.conn,
        &index.db_path,
        &project.repo_root,
        snapshot_id,
        snapshot,
    )?;
    if indexed_snapshot_count(&index.conn, &index.db_path, project)?
        == crate::list_snapshot_ids(&project.repo_root)?.len()
    {
        write_snapshot_dir_fingerprint(
            &index.conn,
            &index.db_path,
            &snapshot_dir_fingerprint(&project.repo_root)?,
        )?;
    }
    Ok(())
}

pub fn list_checkpoint_summaries_from_index(
    project: &ProjectContext,
) -> Result<Vec<CheckpointSummary>> {
    ensure_index_current(project)?;
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    query_checkpoint_summaries(&conn, &db_path, project)
}

pub fn storage_summary_from_index(project: &ProjectContext) -> Result<StorageSummary> {
    ensure_index_current(project)?;
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    query_storage_summary(&conn, &db_path, project)
}

pub fn checkpoint_summaries_and_storage_summary_from_index(
    project: &ProjectContext,
) -> Result<(Vec<CheckpointSummary>, StorageSummary)> {
    ensure_index_current(project)?;
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    let checkpoints = query_checkpoint_summaries(&conn, &db_path, project)?;
    let storage = query_storage_summary(&conn, &db_path, project)?;
    Ok((checkpoints, storage))
}

fn query_storage_summary(
    conn: &rusqlite::Connection,
    db_path: &Path,
    project: &ProjectContext,
) -> Result<StorageSummary> {
    let checkpoint_count = conn
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE project_id = ?1",
            params![project.project_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))? as usize;
    let logical_size_bytes = conn
        .query_row(
            "SELECT COALESCE(SUM(se.size_bytes), 0)
             FROM snapshot_entries se
             INNER JOIN snapshots s ON s.snapshot_id = se.snapshot_id
             WHERE s.project_id = ?1",
            params![project.project_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))? as u64;
    let unique_blob_count = conn
        .query_row(
            "SELECT COUNT(DISTINCT se.object_id)
             FROM snapshot_entries se
             INNER JOIN snapshots s ON s.snapshot_id = se.snapshot_id
             WHERE s.project_id = ?1",
            params![project.project_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))? as usize;
    let stored_size_bytes = conn
        .query_row(
            "SELECT COALESCE(SUM(o.size_bytes), 0)
             FROM objects o
             INNER JOIN (
               SELECT DISTINCT se.object_id
               FROM snapshot_entries se
               INNER JOIN snapshots s ON s.snapshot_id = se.snapshot_id
               WHERE s.project_id = ?1
             ) referenced ON referenced.object_id = o.object_id",
            params![project.project_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))? as u64;

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
    let index = open_index_connection(project)?;
    load_file_fingerprints_with_connection(&index.conn, &index.db_path, project)
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
                row.get::<_, i64>(1)? as u64,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| db_error(db_path, error))?;
    let mut records = BTreeMap::new();
    for row in rows {
        let (path, size_bytes, fingerprint, object_id) =
            row.map_err(|error| db_error(db_path, error))?;
        let Ok(path) = TrackedUnityFilePath::parse(&path) else {
            continue;
        };
        let Ok(object_id) = ObjectId::parse(&object_id) else {
            continue;
        };
        if object_path(&project.repo_root, &object_id).is_file() {
            records.insert(
                path,
                CachedFileFingerprint {
                    size_bytes,
                    fingerprint,
                    object_id,
                },
            );
        }
    }
    Ok(records)
}

pub(crate) fn refresh_file_fingerprints_with_index_connection(
    index: &IndexConnection,
    project: &ProjectContext,
    updates: &[FileFingerprintUpdate],
    seen_paths: &BTreeSet<TrackedUnityFilePath>,
) -> Result<()> {
    let existing = load_file_fingerprints_with_connection(&index.conn, &index.db_path, project)?;
    let tx = index
        .conn
        .unchecked_transaction()
        .map_err(|error| db_error(&index.db_path, error))?;
    {
        let mut statement = tx
            .prepare(
                "INSERT OR REPLACE INTO file_fingerprints
                 (project_id, path, size_bytes, modified_at_utc, fingerprint, object_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| db_error(&index.db_path, error))?;
        for update in updates {
            statement
                .execute(params![
                    project.project_id.as_str(),
                    update.path.as_str(),
                    update.size_bytes as i64,
                    update.modified_at_utc.as_str(),
                    update.fingerprint.as_str(),
                    update.object_id.as_str(),
                ])
                .map_err(|error| db_error(&index.db_path, error))?;
        }
    }
    for path in existing.keys().filter(|path| !seen_paths.contains(*path)) {
        tx.execute(
            "DELETE FROM file_fingerprints WHERE project_id = ?1 AND path = ?2",
            params![project.project_id.as_str(), path.as_str()],
        )
        .map_err(|error| db_error(&index.db_path, error))?;
    }
    tx.commit().map_err(|error| db_error(&index.db_path, error))
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
        Err(_) => remove_index_db(&project.repo_root),
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
    let db_path = crate::db_path(&project.repo_root);
    remove_index_db(&project.repo_root)?;
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    let mut snapshot_count = 0_usize;
    let mut referenced_objects = BTreeSet::new();
    let mut missing_object_count = 0_usize;
    let mut errors = Vec::new();
    let snapshot_ids = crate::list_snapshot_ids(&project.repo_root)?;
    let total = snapshot_ids.len();
    for (index, snapshot_id) in snapshot_ids.into_iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        match load_snapshot(&project.repo_root, &snapshot_id) {
            Ok(snapshot) if snapshot.project_id == project.project_id => {
                for file in &snapshot.files {
                    referenced_objects.insert(file.content_hash().clone());
                    if !object_path(&project.repo_root, file.content_hash()).is_file() {
                        missing_object_count += 1;
                    }
                }
                if let Err(error) = index_snapshot_with_connection(
                    &conn,
                    &db_path,
                    &project.repo_root,
                    &snapshot_id,
                    &snapshot,
                ) {
                    errors.push(error.to_string());
                } else {
                    snapshot_count += 1;
                }
            }
            Ok(_) => {}
            Err(error) => errors.push(error.to_string()),
        }
        report_operation_progress(
            progress,
            "rebuildIndex",
            index + 1,
            total,
            Some(snapshot_id.to_string()),
        );
    }

    let object_count = index_existing_objects(&conn, &db_path, &project.repo_root)?;
    if errors.is_empty() {
        write_snapshot_dir_fingerprint(
            &conn,
            &db_path,
            &snapshot_dir_fingerprint(&project.repo_root)?,
        )?;
    }
    Ok(RebuildIndexResult {
        snapshot_count,
        object_count,
        missing_object_count,
        errors,
    })
}

fn index_snapshot_with_connection(
    conn: &rusqlite::Connection,
    db_path: &Path,
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(db_path, error))?;
    tx.execute(
        "INSERT OR REPLACE INTO snapshots(snapshot_id, project_id, created_at_utc, name, parent_snapshot_id, file_count, logical_size_bytes)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id.as_str(),
            snapshot.project_id.as_str(),
            snapshot.created_at_utc.as_str(),
            snapshot.name.as_str(),
            snapshot.parent_snapshot_id.as_ref().map(|id| id.as_str()),
            snapshot.files.len() as i64,
            snapshot.files.iter().map(|file| file.size_bytes).sum::<u64>() as i64,
        ],
    )
    .map_err(|error| db_error(db_path, error))?;
    tx.execute(
        "DELETE FROM snapshot_entries WHERE snapshot_id = ?1",
        params![snapshot_id.as_str()],
    )
    .map_err(|error| db_error(db_path, error))?;
    for file in &snapshot.files {
        tx.execute(
            "INSERT OR REPLACE INTO snapshot_entries(snapshot_id, path, object_id, size_bytes, modified_at_utc)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                snapshot_id.as_str(),
                file.path.as_str(),
                file.content_hash().as_str(),
                file.size_bytes as i64,
                file.modified_at_utc.as_str(),
            ],
        )
        .map_err(|error| db_error(db_path, error))?;
        if let Ok((relative_path, stored_size_bytes)) =
            object_index_entry(repo_root, file.content_hash())
        {
            tx.execute(
                "INSERT OR REPLACE INTO objects(object_id, size_bytes, relative_path, verified_at_utc)
                 VALUES(?1, ?2, ?3, NULL)",
                params![
                    file.content_hash().as_str(),
                    stored_size_bytes as i64,
                    relative_path.to_string_lossy().to_string(),
                ],
            )
            .map_err(|error| db_error(db_path, error))?;
        }
    }
    tx.commit().map_err(|error| db_error(db_path, error))
}

pub(crate) fn delete_snapshot_from_index(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
) -> Result<()> {
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(&db_path, error))?;
    tx.execute(
        "DELETE FROM snapshot_entries WHERE snapshot_id = ?1",
        params![snapshot_id.as_str()],
    )
    .map_err(|error| db_error(&db_path, error))?;
    let deleted_snapshots = tx
        .execute(
            "DELETE FROM snapshots WHERE snapshot_id = ?1 AND project_id = ?2",
            params![snapshot_id.as_str(), project.project_id.as_str()],
        )
        .map_err(|error| db_error(&db_path, error))?;
    if deleted_snapshots != 1 {
        return Err(crate::CheckPoError::IndexUnavailable(format!(
            "SQLite index did not contain checkpoint {}",
            snapshot_id
        )));
    }
    tx.commit().map_err(|error| db_error(&db_path, error))?;
    if indexed_snapshot_count(&conn, &db_path, project)?
        == crate::list_snapshot_ids(&project.repo_root)?.len()
    {
        write_snapshot_dir_fingerprint(
            &conn,
            &db_path,
            &snapshot_dir_fingerprint(&project.repo_root)?,
        )?;
    }
    Ok(())
}

fn ensure_index_current(project: &ProjectContext) -> Result<()> {
    let expected = snapshot_dir_fingerprint(&project.repo_root)?;
    let db_path = crate::db_path(&project.repo_root);
    let current = if db_path.is_file() {
        read_snapshot_dir_fingerprint(project).ok().flatten()
    } else {
        None
    };
    if current.as_deref() == Some(expected.as_str()) {
        return Ok(());
    }
    crate::ensure_project_location_allows_mutation(project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "index-rebuild")?;
    ensure_no_pending_transactions(project)?;
    rebuild_index_for_project_unlocked(project, None, None)?;
    Ok(())
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
                row.get::<_, i64>(3)? as usize,
                row.get::<_, i64>(4)? as u64,
            ))
        })
        .map_err(|error| db_error(db_path, error))?;
    let mut summaries = Vec::new();
    for row in rows {
        let (snapshot_id, name, created_at_utc, file_count, logical_size_bytes) =
            row.map_err(|error| db_error(db_path, error))?;
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

fn indexed_snapshot_count(
    conn: &rusqlite::Connection,
    db_path: &Path,
    project: &ProjectContext,
) -> Result<usize> {
    let count = conn
        .query_row(
            "SELECT COUNT(*) FROM snapshots WHERE project_id = ?1",
            params![project.project_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))?;
    Ok(count as usize)
}

fn delete_file_fingerprints(
    project: &ProjectContext,
    paths: &[TrackedUnityFilePath],
) -> Result<()> {
    let db_path = crate::db_path(&project.repo_root);
    if !db_path.exists() {
        return Ok(());
    }
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
    for path in paths {
        conn.execute(
            "DELETE FROM file_fingerprints WHERE project_id = ?1 AND path = ?2",
            params![project.project_id.as_str(), path.as_str()],
        )
        .map_err(|error| db_error(&db_path, error))?;
    }
    Ok(())
}

fn remove_index_db(repo_root: &Path) -> Result<()> {
    let path = crate::db_path(repo_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::io_error(&path, error)),
    }
}

fn object_index_entry(repo_root: &Path, object_id: &ObjectId) -> Result<(PathBuf, u64)> {
    let path = object_path(repo_root, object_id);
    let metadata = fs::metadata(&path).map_err(|error| crate::io_error(&path, error))?;
    let relative = path
        .strip_prefix(repo_root)
        .map_err(|_| crate::CheckPoError::OutsideTrackedScope(path.display().to_string()))?
        .to_path_buf();
    Ok((relative, metadata.len()))
}

fn index_existing_objects(
    conn: &rusqlite::Connection,
    db_path: &Path,
    repo_root: &Path,
) -> Result<usize> {
    let loose_root = repo_root.join("objects").join("loose");
    if !loose_root.exists() {
        return Ok(0);
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(db_path, error))?;
    let mut count = 0_usize;
    {
        let mut statement = tx
            .prepare(
                "INSERT OR REPLACE INTO objects(object_id, size_bytes, relative_path, verified_at_utc)
                 VALUES(?1, ?2, ?3, NULL)",
            )
            .map_err(|error| db_error(db_path, error))?;
        for entry in WalkDir::new(&loose_root).follow_links(false) {
            let entry = entry.map_err(|error| {
                let path = error
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| loose_root.clone());
                crate::io_error(path, std::io::Error::other(error))
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry.path().strip_prefix(repo_root).map_err(|_| {
                crate::CheckPoError::OutsideTrackedScope(entry.path().display().to_string())
            })?;
            let Ok(object_id) = crate::object_id_from_loose_relative_path(relative) else {
                continue;
            };
            let metadata =
                fs::metadata(entry.path()).map_err(|error| crate::io_error(entry.path(), error))?;
            statement
                .execute(params![
                    object_id.as_str(),
                    metadata.len() as i64,
                    relative.to_string_lossy().to_string(),
                ])
                .map_err(|error| db_error(db_path, error))?;
            count += 1;
        }
    }
    tx.commit().map_err(|error| db_error(db_path, error))?;
    Ok(count)
}

fn snapshot_dir_fingerprint(repo_root: &Path) -> Result<String> {
    let ids = crate::list_snapshot_ids(repo_root)?;
    let mut parts = Vec::with_capacity(ids.len());
    for id in ids {
        let path = crate::snapshot_path(repo_root, &id);
        let metadata = fs::metadata(&path).map_err(|error| crate::io_error(&path, error))?;
        let modified = metadata
            .modified()
            .map_err(|error| crate::io_error(&path, error))?;
        parts.push(format!(
            "{}:{}:{}",
            id.as_str(),
            metadata.len(),
            crate::canonical_utc(modified)
        ));
    }
    Ok(parts.join("|"))
}

fn read_snapshot_dir_fingerprint(project: &ProjectContext) -> Result<Option<String>> {
    let db_path = crate::db_path(&project.repo_root);
    let conn = open_db(&project.repo_root)?;
    create_schema(&conn, &db_path)?;
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
    if !schema_is_compatible(conn, db_path)? {
        recreate_schema(conn, db_path)?;
    }
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS snapshots(
          snapshot_id TEXT PRIMARY KEY,
          project_id TEXT NOT NULL,
          created_at_utc TEXT NOT NULL,
          name TEXT NOT NULL,
          parent_snapshot_id TEXT,
          file_count INTEGER NOT NULL,
          logical_size_bytes INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS snapshot_entries(
          snapshot_id TEXT NOT NULL,
          path TEXT NOT NULL,
          object_id TEXT NOT NULL,
          size_bytes INTEGER NOT NULL,
          modified_at_utc TEXT NOT NULL,
          PRIMARY KEY(snapshot_id, path)
        );
        CREATE TABLE IF NOT EXISTS objects(
          object_id TEXT PRIMARY KEY,
          size_bytes INTEGER NOT NULL,
          relative_path TEXT NOT NULL,
          verified_at_utc TEXT
        );
        CREATE TABLE IF NOT EXISTS file_fingerprints(
          project_id TEXT NOT NULL,
          path TEXT NOT NULL,
          size_bytes INTEGER NOT NULL,
          modified_at_utc TEXT NOT NULL,
          fingerprint TEXT NOT NULL,
          object_id TEXT NOT NULL,
          PRIMARY KEY(project_id, path)
        );
        CREATE TABLE IF NOT EXISTS index_state(
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        PRAGMA user_version = 2;
        ",
    )
    .map_err(|error| db_error(db_path, error))
}

fn schema_is_compatible(conn: &rusqlite::Connection, db_path: &std::path::Path) -> Result<bool> {
    let existing_tables = user_tables(conn, db_path)?;
    if existing_tables.is_empty() {
        return Ok(true);
    }
    let expected = expected_schema();
    if existing_tables
        .iter()
        .any(|table| !expected.iter().any(|(expected, _)| expected == table))
    {
        return Ok(false);
    }
    for (table, expected_columns) in expected {
        if existing_tables.iter().any(|existing| existing == table) {
            let columns = table_columns(conn, db_path, table)?;
            if columns != expected_columns {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn recreate_schema(conn: &rusqlite::Connection, db_path: &std::path::Path) -> Result<()> {
    let tables = user_tables(conn, db_path)?;
    let mut sql = String::new();
    for table in tables {
        sql.push_str("DROP TABLE IF EXISTS ");
        sql.push_str(&quote_identifier(&table));
        sql.push(';');
    }
    if !sql.is_empty() {
        conn.execute_batch(&sql)
            .map_err(|error| db_error(db_path, error))?;
    }
    Ok(())
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
        (
            "file_fingerprints",
            vec![
                "project_id",
                "path",
                "size_bytes",
                "modified_at_utc",
                "fingerprint",
                "object_id",
            ],
        ),
        ("index_state", vec!["key", "value"]),
        (
            "objects",
            vec![
                "object_id",
                "size_bytes",
                "relative_path",
                "verified_at_utc",
            ],
        ),
        (
            "snapshot_entries",
            vec![
                "snapshot_id",
                "path",
                "object_id",
                "size_bytes",
                "modified_at_utc",
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
}
