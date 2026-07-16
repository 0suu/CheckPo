use crate::storage::chunk_store::RepositoryManifestSource;
use crate::storage::merkle_codec::{ChunkKind, Digest32, ManifestRef};
use crate::storage::snapshot_v2::{
    load_chunk, validate_manifest_cached, DecodedChunk, ManifestValidationCache,
};
use crate::storage::{object_path_no_follow, open_file_fingerprint_db};
use crate::{
    db_error, ensure_no_pending_transactions, report_operation_progress, CancellationToken,
    CheckpointIndexState, CheckpointIndexStatus, CheckpointSummary, ObjectId, OperationProgress,
    ProjectContext, RebuildIndexResult, Result, SnapshotFile, SnapshotId, StorageSummary,
    TrackedUnityFilePath,
};
use rusqlite::{params, OpenFlags, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const INDEX_STATE_SNAPSHOT_INVENTORY_HEAD: &str = "snapshot_inventory_head";
const INDEX_STATE_PROJECT_ID: &str = "project_id";
const SNAPSHOT_INDEX_SCHEMA_VERSION: i64 = 5;
const MAX_REBUILD_VALIDATION_CACHE_CHUNKS: usize = 100_000;
const MAX_REBUILD_VALIDATION_CACHE_BYTES: usize = 64 * 1024 * 1024;

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

#[derive(Debug, Clone)]
pub(crate) struct CachedObjectIntegrityFingerprint {
    pub size_bytes: u64,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ObjectIntegrityFingerprintUpdate {
    pub object_id: ObjectId,
    pub size_bytes: u64,
    pub fingerprint: String,
}

pub(crate) struct IndexConnection {
    conn: BoundSnapshotIndexConnection,
    db_path: PathBuf,
}

struct BoundSnapshotIndexConnection {
    connection: rusqlite::Connection,
    _database_directory: crate::storage::AnchoredRoot,
    _indexes: crate::storage::AnchoredParent,
}

impl std::ops::Deref for BoundSnapshotIndexConnection {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

pub(crate) fn open_index_connection(project: &ProjectContext) -> Result<IndexConnection> {
    let status = checkpoint_index_status(project)?;
    if status.state != CheckpointIndexState::Current {
        return Err(index_unavailable(&status));
    }
    let db_path = crate::db_path(&project.repo_root)?;
    let conn = open_snapshot_index_read_write(&db_path)?;
    Ok(IndexConnection { conn, db_path })
}

pub fn checkpoint_index_status(project: &ProjectContext) -> Result<CheckpointIndexStatus> {
    let db_path = crate::db_path(&project.repo_root)?;
    let metadata = match fs::symlink_metadata(&db_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Missing,
                rebuildable: true,
                detail: Some("checkpoint index has not been built".to_string()),
            })
        }
        Err(error) => return Err(crate::io_error(&db_path, error)),
    };
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
    let indexed = match read_snapshot_inventory_head_with_connection(&conn, &db_path) {
        Ok(value) => value,
        Err(error) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Corrupt,
                rebuildable: true,
                detail: Some(error.to_string()),
            })
        }
    };
    let actual = match crate::storage::inventory_head_id(&project.repo_root, &project.project_id) {
        Ok(head_id) => head_id,
        Err(error) => {
            return Ok(CheckpointIndexStatus {
                state: CheckpointIndexState::Corrupt,
                rebuildable: false,
                detail: Some(error.to_string()),
            })
        }
    };
    if indexed.as_deref() != Some(actual.as_str()) {
        return Ok(CheckpointIndexStatus {
            state: CheckpointIndexState::Stale,
            rebuildable: true,
            detail: Some("checkpoint inventory changed after the index was built".to_string()),
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
    write_snapshot_inventory_head(
        &index.conn,
        &index.db_path,
        &crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?,
    )
}

pub fn list_checkpoint_summaries_from_index(
    project: &ProjectContext,
) -> Result<Vec<CheckpointSummary>> {
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root)?;
    let conn = open_snapshot_index_read_only(&db_path).map_err(index_read_error)?;
    query_checkpoint_summaries(&conn, &db_path, project).map_err(index_read_error)
}

pub fn storage_summary_from_index(project: &ProjectContext) -> Result<StorageSummary> {
    let _lock = crate::acquire_project_repository_shared_lock(project, "storage-summary")?;
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root)?;
    let conn = open_snapshot_index_read_only(&db_path).map_err(index_read_error)?;
    query_storage_summary(&conn, &db_path, project).map_err(index_read_error)
}

pub fn storage_index_summary_from_index(
    project: &ProjectContext,
) -> Result<crate::StorageIndexSummary> {
    let _lock = crate::acquire_project_repository_shared_lock(project, "storage-index-summary")?;
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root)?;
    let conn = open_snapshot_index_read_only(&db_path).map_err(index_read_error)?;
    query_storage_index_summary(&conn, &db_path, project).map_err(index_read_error)
}

pub fn checkpoint_summaries_and_storage_summary_from_index(
    project: &ProjectContext,
) -> Result<(Vec<CheckpointSummary>, StorageSummary)> {
    let _lock =
        crate::acquire_project_repository_shared_lock(project, "checkpoint-storage-summary")?;
    require_current_index(project)?;
    let db_path = crate::db_path(&project.repo_root)?;
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
    let indexed = query_storage_index_summary(conn, db_path, project)?;
    Ok(StorageSummary {
        checkpoint_count: indexed.checkpoint_count,
        unique_blob_count: indexed.unique_blob_count,
        logical_size_bytes: indexed.logical_size_bytes,
        stored_size_bytes: repository_content_stored_bytes(&project.repo_root)?,
    })
}

fn query_storage_index_summary(
    conn: &rusqlite::Connection,
    db_path: &Path,
    project: &ProjectContext,
) -> Result<crate::StorageIndexSummary> {
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
    let unique_blob_count = conn
        .query_row("SELECT COUNT(*) FROM object_refs", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| db_error(db_path, error))?;
    let unique_blob_count = usize_from_sqlite_i64(unique_blob_count, "unique blob count")?;
    Ok(crate::StorageIndexSummary {
        checkpoint_count,
        unique_blob_count,
        logical_size_bytes,
    })
}

fn repository_content_stored_bytes(repo_root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    for root in [
        crate::snapshots_dir(repo_root),
        repo_root.join("manifests").join("v2"),
        repo_root.join("objects").join("loose"),
    ] {
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(crate::io_error(&root, error)),
            Ok(metadata) if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
                return Err(crate::CheckPoError::Corruption(format!(
                    "repository content directory is unsafe: {}",
                    root.display()
                )))
            }
            Ok(_) => {}
        }
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|error| crate::CheckPoError::Io {
                path: error
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone()),
                source: error
                    .into_io_error()
                    .unwrap_or_else(|| std::io::Error::other("repository walk failed")),
            })?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(path).map_err(|error| crate::io_error(path, error))?;
            if crate::metadata_is_link_or_reparse(&metadata) {
                return Err(crate::CheckPoError::Corruption(format!(
                    "repository content entry is a link or reparse point: {}",
                    path.display()
                )));
            }
            if metadata.is_file() {
                total = total.checked_add(metadata.len()).ok_or_else(|| {
                    crate::CheckPoError::Corruption(
                        "repository content size exceeds the supported range".to_string(),
                    )
                })?;
            }
        }
    }
    Ok(total)
}

pub fn load_file_fingerprints(
    project: &ProjectContext,
) -> Result<BTreeMap<TrackedUnityFilePath, CachedFileFingerprint>> {
    let db_path = crate::file_fingerprint_db_path(&project.repo_root)?;
    if !db_path.exists() {
        return Ok(BTreeMap::new());
    }
    let conn = open_file_fingerprint_db(&project.repo_root)?;
    create_fingerprint_schema(&conn, &db_path)?;
    load_file_fingerprints_with_connection(&conn, &db_path, project)
}

pub(crate) fn load_object_integrity_fingerprints(
    repo_root: &Path,
) -> Result<BTreeMap<ObjectId, CachedObjectIntegrityFingerprint>> {
    let db_path = crate::file_fingerprint_db_path(repo_root)?;
    if !db_path.exists() {
        return Ok(BTreeMap::new());
    }
    let conn = open_file_fingerprint_db(repo_root)?;
    create_fingerprint_schema(&conn, &db_path)?;
    let mut statement = conn
        .prepare("SELECT object_id, size_bytes, fingerprint FROM object_integrity_fingerprints")
        .map_err(|error| db_error(&db_path, error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| db_error(&db_path, error))?;
    let mut records = BTreeMap::new();
    for row in rows {
        let (object_id, size_bytes, fingerprint) =
            row.map_err(|error| db_error(&db_path, error))?;
        let Ok(object_id) = ObjectId::parse(&object_id) else {
            continue;
        };
        let size_bytes = u64_from_sqlite_i64(size_bytes, "object integrity size in bytes")?;
        records.insert(
            object_id,
            CachedObjectIntegrityFingerprint {
                size_bytes,
                fingerprint,
            },
        );
    }
    Ok(records)
}

pub(crate) fn refresh_object_integrity_fingerprints(
    repo_root: &Path,
    updates: &[ObjectIntegrityFingerprintUpdate],
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let db_path = crate::file_fingerprint_db_path(repo_root)?;
    let conn = open_file_fingerprint_db(repo_root)?;
    create_fingerprint_schema(&conn, &db_path)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(&db_path, error))?;
    {
        let mut statement = tx
            .prepare(
                "INSERT OR REPLACE INTO object_integrity_fingerprints
                 (object_id, size_bytes, fingerprint) VALUES(?1, ?2, ?3)",
            )
            .map_err(|error| db_error(&db_path, error))?;
        for update in updates {
            statement
                .execute(params![
                    update.object_id.as_str(),
                    sqlite_i64_from_u64(update.size_bytes, "object integrity size in bytes")?,
                    update.fingerprint.as_str(),
                ])
                .map_err(|error| db_error(&db_path, error))?;
        }
    }
    tx.commit().map_err(|error| db_error(&db_path, error))
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
    let db_path = crate::file_fingerprint_db_path(&project.repo_root)?;
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
        Err(_) => crate::storage::remove_file_fingerprint_db_if_exists(&project.repo_root),
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
    let _lock = crate::acquire_project_repository_lock(project, "index-rebuild")?;
    ensure_no_pending_transactions(project)?;
    rebuild_index_for_project_unlocked(project, progress, cancellation)
}

pub(crate) fn rebuild_index_for_project_unlocked(
    project: &ProjectContext,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<RebuildIndexResult> {
    crate::ensure_not_cancelled(cancellation)?;
    let before = crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    let snapshot_ids = crate::storage::validate_physical_snapshot_inventory(
        &project.repo_root,
        &project.project_id,
    )?;
    let live_path = crate::db_path(&project.repo_root)?;
    let indexes_dir = live_path
        .parent()
        .ok_or_else(|| crate::CheckPoError::Unexpected("index path has no parent".to_string()))?;
    crate::create_absolute_dir_all_no_follow(indexes_dir)?;
    let anchored_indexes = crate::storage::AnchoredRoot::open(indexes_dir)?;
    let indexes = anchored_indexes.open_directory_for_mutation(Path::new(""), false)?;
    anchored_indexes.verify_root_binding()?;
    cleanup_owned_index_temporary_files(&anchored_indexes, &indexes)?;
    let temp_leaf =
        std::ffi::OsString::from(format!("snapshot-index-{}.tmp", Uuid::new_v4().simple()));
    let temp_file = indexes.create_new_file(&temp_leaf)?;
    let temp_path = indexes_dir.join(&temp_leaf);
    let result = match build_snapshot_index(
        project,
        &temp_path,
        &before,
        &snapshot_ids,
        progress,
        cancellation,
    ) {
        Ok(result) => result,
        Err(error) => {
            let _ = remove_bound_index_temporary(&indexes, &temp_leaf, temp_file);
            return Err(error);
        }
    };
    crate::ensure_not_cancelled(cancellation)?;
    let after = crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    let after_snapshot_ids = crate::storage::validate_physical_snapshot_inventory(
        &project.repo_root,
        &project.project_id,
    )?;
    if before != after || snapshot_ids != after_snapshot_ids {
        let _ = remove_bound_index_temporary(&indexes, &temp_leaf, temp_file);
        return Err(crate::CheckPoError::IndexUnavailable(
            "checkpoint files changed while the index was being rebuilt".to_string(),
        ));
    }
    report_operation_progress(progress, "committingIndex", 0, 0, None);
    if let Err(error) = temp_file.sync_all().and_then(|()| {
        indexes.verify_file_binding(&temp_leaf, &temp_file)?;
        anchored_indexes.verify_root_binding()
    }) {
        let _ = remove_bound_index_temporary(&indexes, &temp_leaf, temp_file);
        return Err(error);
    }
    let live_leaf = live_path.file_name().ok_or_else(|| {
        crate::CheckPoError::Unexpected("index path has no file name".to_string())
    })?;
    if let Err(error) = publish_snapshot_index(&indexes, &temp_leaf, &temp_file, live_leaf) {
        let _ = remove_bound_index_temporary(&indexes, &temp_leaf, temp_file);
        return Err(error);
    }
    anchored_indexes.verify_root_binding()?;
    Ok(result)
}

fn build_snapshot_index(
    project: &ProjectContext,
    db_path: &Path,
    source_inventory_head: &str,
    snapshot_ids: &[SnapshotId],
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<RebuildIndexResult> {
    let conn = open_bound_snapshot_index(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| db_error(db_path, error))?;
    create_schema(&conn, db_path)?;
    let mut snapshot_count = 0_usize;
    let mut missing_object_count = 0_usize;
    let total = snapshot_ids.len();
    let source = RepositoryManifestSource::new(&project.repo_root)?;
    let mut validation_cache = ManifestValidationCache::default();
    prepare_rebuild_aggregation_tables(&conn, db_path)?;
    let mut aggregated_object_count = 0_usize;
    for (index, snapshot_id) in snapshot_ids.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        let root = crate::storage::load_snapshot_root_header(&project.repo_root, snapshot_id)?;
        if root.project_id != project.project_id {
            return Err(crate::CheckPoError::Corruption(format!(
                "snapshot {snapshot_id} belongs to a different project"
            )));
        }
        validate_manifest_cached(&source, root.manifest, root.summary, &mut validation_cache)
            .map_err(snapshot_v2_index_error)?;
        if validation_cache.len() >= MAX_REBUILD_VALIDATION_CACHE_CHUNKS
            || validation_cache.estimated_heap_bytes() >= MAX_REBUILD_VALIDATION_CACHE_BYTES
        {
            validation_cache = ManifestValidationCache::default();
        }
        insert_snapshot_root_summary(&conn, db_path, snapshot_id, &root)?;
        if let Some(manifest) = root.manifest {
            queue_rebuild_manifest_root(&conn, db_path, manifest)?;
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
    aggregate_manifest_object_references(
        &source,
        &conn,
        db_path,
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

    write_snapshot_inventory_head(&conn, db_path, source_inventory_head)?;
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
    let errors = if missing_object_count == 0 {
        Vec::new()
    } else {
        vec![format!(
            "{missing_object_count} referenced object(s) are unavailable; the index was rebuilt, but the repository requires a full verify"
        )]
    };
    Ok(RebuildIndexResult {
        snapshot_count,
        referenced_object_count: object_count,
        unavailable_referenced_object_count: missing_object_count,
        errors,
    })
}

fn aggregate_manifest_object_references(
    source: &RepositoryManifestSource<'_>,
    conn: &rusqlite::Connection,
    db_path: &Path,
    aggregated_object_count: &mut usize,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    discover_rebuild_manifest_graph(source, conn, db_path, cancellation)?;
    let total_chunks_i64 = conn
        .query_row("SELECT COUNT(*) FROM temp.rebuild_chunks", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| db_error(db_path, error))?;
    let total_chunks = usize_from_sqlite_i64(total_chunks_i64, "rebuild manifest chunk count")?;
    let total_object_rows_i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM temp.rebuild_leaf_objects",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))?;
    let total_object_rows =
        usize_from_sqlite_i64(total_object_rows_i64, "rebuild leaf object count")?;
    let mut processed_chunks = 0_usize;

    loop {
        crate::ensure_not_cancelled(cancellation)?;
        let ready = read_ready_rebuild_chunks(conn, db_path, 256)?;
        if ready.is_empty() {
            break;
        }
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| db_error(db_path, error))?;
        for reference in ready {
            crate::ensure_not_cancelled(cancellation)?;
            process_ready_rebuild_chunk(
                &tx,
                db_path,
                reference,
                aggregated_object_count,
                total_object_rows,
                progress,
                cancellation,
            )?;
            processed_chunks = processed_chunks.checked_add(1).ok_or_else(|| {
                crate::CheckPoError::Corruption(
                    "processed manifest chunk count exceeds the supported range".to_string(),
                )
            })?;
        }
        tx.commit().map_err(|error| db_error(db_path, error))?;
    }
    if processed_chunks != total_chunks {
        return Err(crate::CheckPoError::Corruption(
            "manifest graph contains a cycle".to_string(),
        ));
    }
    Ok(())
}

fn prepare_rebuild_aggregation_tables(conn: &rusqlite::Connection, db_path: &Path) -> Result<()> {
    conn.execute_batch(
        "PRAGMA temp_store = FILE;
         CREATE TEMP TABLE rebuild_chunks(
           kind INTEGER NOT NULL,
           chunk_id BLOB NOT NULL,
           discovered INTEGER NOT NULL DEFAULT 0,
           processed INTEGER NOT NULL DEFAULT 0,
           incoming_remaining INTEGER NOT NULL DEFAULT 0,
           multiplicity INTEGER NOT NULL DEFAULT 0,
           PRIMARY KEY(kind, chunk_id)
         ) WITHOUT ROWID;
         CREATE INDEX temp.rebuild_chunks_ready
           ON rebuild_chunks(discovered, processed, incoming_remaining);
         CREATE TEMP TABLE rebuild_pending(
           kind INTEGER NOT NULL,
           chunk_id BLOB NOT NULL,
           PRIMARY KEY(kind, chunk_id)
         ) WITHOUT ROWID;
         CREATE TEMP TABLE rebuild_edges(
           parent_kind INTEGER NOT NULL,
           parent_id BLOB NOT NULL,
           child_kind INTEGER NOT NULL,
           child_id BLOB NOT NULL,
           edge_count INTEGER NOT NULL,
           PRIMARY KEY(parent_kind, parent_id, child_kind, child_id)
         ) WITHOUT ROWID;
         CREATE TEMP TABLE rebuild_leaf_objects(
           chunk_kind INTEGER NOT NULL,
           chunk_id BLOB NOT NULL,
           object_id BLOB NOT NULL,
           expected_size_bytes INTEGER NOT NULL,
           occurrence_count INTEGER NOT NULL,
           PRIMARY KEY(chunk_kind, chunk_id, object_id)
         ) WITHOUT ROWID;",
    )
    .map_err(|error| db_error(db_path, error))
}

fn queue_rebuild_manifest_root(
    conn: &rusqlite::Connection,
    db_path: &Path,
    reference: ManifestRef,
) -> Result<()> {
    let (kind, id) = manifest_ref_sql_parts(reference);
    let existing = conn
        .query_row(
            "SELECT multiplicity FROM temp.rebuild_chunks WHERE kind = ?1 AND chunk_id = ?2",
            params![kind, id.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| db_error(db_path, error))?;
    match existing {
        Some(existing) => {
            let next = rebuild_u64(existing, "manifest root multiplicity")?
                .checked_add(1)
                .ok_or_else(|| {
                    crate::CheckPoError::Corruption(
                        "manifest root multiplicity exceeds the supported range".to_string(),
                    )
                })?;
            conn.execute(
                "UPDATE temp.rebuild_chunks SET multiplicity = ?3 WHERE kind = ?1 AND chunk_id = ?2",
                params![
                    kind,
                    id.as_slice(),
                    sqlite_i64_from_u64(next, "manifest root multiplicity")?
                ],
            )
            .map_err(|error| db_error(db_path, error))?;
        }
        None => {
            conn.execute(
                "INSERT INTO temp.rebuild_chunks(kind, chunk_id, multiplicity) VALUES(?1, ?2, 1)",
                params![kind, id.as_slice()],
            )
            .map_err(|error| db_error(db_path, error))?;
            conn.execute(
                "INSERT INTO temp.rebuild_pending(kind, chunk_id) VALUES(?1, ?2)",
                params![kind, id.as_slice()],
            )
            .map_err(|error| db_error(db_path, error))?;
        }
    }
    Ok(())
}

fn discover_rebuild_manifest_graph(
    source: &RepositoryManifestSource<'_>,
    conn: &rusqlite::Connection,
    db_path: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    loop {
        crate::ensure_not_cancelled(cancellation)?;
        let next = conn
            .query_row(
                "SELECT kind, chunk_id FROM temp.rebuild_pending ORDER BY kind, chunk_id LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|error| db_error(db_path, error))?;
        let Some((kind, id)) = next else {
            break;
        };
        let reference = manifest_ref_from_sql(kind, &id)?;
        let chunk = load_chunk(source, reference).map_err(snapshot_v2_index_error)?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| db_error(db_path, error))?;
        tx.execute(
            "DELETE FROM temp.rebuild_pending WHERE kind = ?1 AND chunk_id = ?2",
            params![kind, id.as_slice()],
        )
        .map_err(|error| db_error(db_path, error))?;
        match chunk {
            DecodedChunk::Node(node, _) => {
                for child in node.children {
                    discover_rebuild_edge(&tx, db_path, reference, child.child)?;
                }
            }
            DecodedChunk::Leaf(leaf, _) => {
                for entry in leaf.entries {
                    discover_rebuild_leaf_object(
                        &tx,
                        db_path,
                        reference,
                        entry.object_id,
                        entry.size_bytes,
                    )?;
                }
            }
        }
        tx.execute(
            "UPDATE temp.rebuild_chunks SET discovered = 1 WHERE kind = ?1 AND chunk_id = ?2",
            params![kind, id.as_slice()],
        )
        .map_err(|error| db_error(db_path, error))?;
        tx.commit().map_err(|error| db_error(db_path, error))?;
    }
    Ok(())
}

fn discover_rebuild_edge(
    tx: &rusqlite::Transaction<'_>,
    db_path: &Path,
    parent: ManifestRef,
    child: ManifestRef,
) -> Result<()> {
    let (parent_kind, parent_id) = manifest_ref_sql_parts(parent);
    let (child_kind, child_id) = manifest_ref_sql_parts(child);
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO temp.rebuild_chunks(kind, chunk_id) VALUES(?1, ?2)",
            params![child_kind, child_id.as_slice()],
        )
        .map_err(|error| db_error(db_path, error))?;
    if inserted == 1 {
        tx.execute(
            "INSERT INTO temp.rebuild_pending(kind, chunk_id) VALUES(?1, ?2)",
            params![child_kind, child_id.as_slice()],
        )
        .map_err(|error| db_error(db_path, error))?;
    }
    let edge_count = tx
        .query_row(
            "SELECT edge_count FROM temp.rebuild_edges
             WHERE parent_kind = ?1 AND parent_id = ?2 AND child_kind = ?3 AND child_id = ?4",
            params![
                parent_kind,
                parent_id.as_slice(),
                child_kind,
                child_id.as_slice()
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| db_error(db_path, error))?;
    let next_edge_count = match edge_count {
        Some(value) => rebuild_u64(value, "manifest edge count")?
            .checked_add(1)
            .ok_or_else(|| {
                crate::CheckPoError::Corruption(
                    "manifest edge count exceeds the supported range".to_string(),
                )
            })?,
        None => 1,
    };
    tx.execute(
        "INSERT OR REPLACE INTO temp.rebuild_edges(
           parent_kind, parent_id, child_kind, child_id, edge_count
         ) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![
            parent_kind,
            parent_id.as_slice(),
            child_kind,
            child_id.as_slice(),
            sqlite_i64_from_u64(next_edge_count, "manifest edge count")?
        ],
    )
    .map_err(|error| db_error(db_path, error))?;
    let incoming = tx
        .query_row(
            "SELECT incoming_remaining FROM temp.rebuild_chunks WHERE kind = ?1 AND chunk_id = ?2",
            params![child_kind, child_id.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))?;
    let incoming = rebuild_u64(incoming, "manifest incoming edge count")?
        .checked_add(1)
        .ok_or_else(|| {
            crate::CheckPoError::Corruption(
                "manifest incoming edge count exceeds the supported range".to_string(),
            )
        })?;
    tx.execute(
        "UPDATE temp.rebuild_chunks SET incoming_remaining = ?3 WHERE kind = ?1 AND chunk_id = ?2",
        params![
            child_kind,
            child_id.as_slice(),
            sqlite_i64_from_u64(incoming, "manifest incoming edge count")?
        ],
    )
    .map_err(|error| db_error(db_path, error))?;
    Ok(())
}

fn discover_rebuild_leaf_object(
    tx: &rusqlite::Transaction<'_>,
    db_path: &Path,
    leaf: ManifestRef,
    object_id: Digest32,
    size_bytes: u64,
) -> Result<()> {
    let (kind, id) = manifest_ref_sql_parts(leaf);
    let existing = tx
        .query_row(
            "SELECT expected_size_bytes, occurrence_count FROM temp.rebuild_leaf_objects
             WHERE chunk_kind = ?1 AND chunk_id = ?2 AND object_id = ?3",
            params![kind, id.as_slice(), object_id.as_bytes().as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|error| db_error(db_path, error))?;
    match existing {
        Some((existing_size, _))
            if rebuild_u64(existing_size, "leaf object size")? != size_bytes =>
        {
            return Err(crate::CheckPoError::Corruption(format!(
                "object {object_id} has conflicting expected sizes in a manifest leaf"
            )))
        }
        Some((_, count)) => {
            let count = rebuild_u64(count, "leaf object occurrence count")?
                .checked_add(1)
                .ok_or_else(|| {
                    crate::CheckPoError::Corruption(
                        "leaf object occurrence count exceeds the supported range".to_string(),
                    )
                })?;
            tx.execute(
                "UPDATE temp.rebuild_leaf_objects SET occurrence_count = ?4
                 WHERE chunk_kind = ?1 AND chunk_id = ?2 AND object_id = ?3",
                params![
                    kind,
                    id.as_slice(),
                    object_id.as_bytes().as_slice(),
                    sqlite_i64_from_u64(count, "leaf object occurrence count")?
                ],
            )
            .map_err(|error| db_error(db_path, error))?;
        }
        None => {
            tx.execute(
                "INSERT INTO temp.rebuild_leaf_objects(
                   chunk_kind, chunk_id, object_id, expected_size_bytes, occurrence_count
                 ) VALUES(?1, ?2, ?3, ?4, 1)",
                params![
                    kind,
                    id.as_slice(),
                    object_id.as_bytes().as_slice(),
                    sqlite_i64_from_u64(size_bytes, "leaf object size")?
                ],
            )
            .map_err(|error| db_error(db_path, error))?;
        }
    }
    Ok(())
}

fn read_ready_rebuild_chunks(
    conn: &rusqlite::Connection,
    db_path: &Path,
    limit: usize,
) -> Result<Vec<ManifestRef>> {
    let limit = sqlite_i64_from_usize(limit, "ready manifest chunk page size")?;
    let mut statement = conn
        .prepare(
            "SELECT kind, chunk_id FROM temp.rebuild_chunks
             WHERE discovered = 1 AND processed = 0 AND incoming_remaining = 0
             ORDER BY kind, chunk_id LIMIT ?1",
        )
        .map_err(|error| db_error(db_path, error))?;
    let rows = statement
        .query_map(params![limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| db_error(db_path, error))?;
    let mut ready = Vec::new();
    for row in rows {
        let (kind, id) = row.map_err(|error| db_error(db_path, error))?;
        ready.push(manifest_ref_from_sql(kind, &id)?);
    }
    Ok(ready)
}

fn process_ready_rebuild_chunk(
    tx: &rusqlite::Transaction<'_>,
    db_path: &Path,
    reference: ManifestRef,
    aggregated_object_count: &mut usize,
    total_object_rows: usize,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<()> {
    let (kind, id) = manifest_ref_sql_parts(reference);
    let multiplicity = tx
        .query_row(
            "SELECT multiplicity FROM temp.rebuild_chunks WHERE kind = ?1 AND chunk_id = ?2",
            params![kind, id.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_error(db_path, error))?;
    let multiplicity = rebuild_u64(multiplicity, "manifest multiplicity")?;
    match reference.kind {
        ChunkKind::Node => {
            let edges = {
                let mut statement = tx
                    .prepare(
                        "SELECT child_kind, child_id, edge_count FROM temp.rebuild_edges
                         WHERE parent_kind = ?1 AND parent_id = ?2
                         ORDER BY child_kind, child_id",
                    )
                    .map_err(|error| db_error(db_path, error))?;
                let rows = statement
                    .query_map(params![kind, id.as_slice()], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(|error| db_error(db_path, error))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| db_error(db_path, error))?
            };
            for (child_kind, child_id, edge_count) in edges {
                let edge_count = rebuild_u64(edge_count, "manifest edge count")?;
                let (child_multiplicity, incoming) = tx
                    .query_row(
                        "SELECT multiplicity, incoming_remaining FROM temp.rebuild_chunks
                         WHERE kind = ?1 AND chunk_id = ?2",
                        params![child_kind, child_id.as_slice()],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .map_err(|error| db_error(db_path, error))?;
                let contribution = multiplicity.checked_mul(edge_count).ok_or_else(|| {
                    crate::CheckPoError::Corruption(
                        "manifest multiplicity exceeds the supported range".to_string(),
                    )
                })?;
                let child_multiplicity =
                    rebuild_u64(child_multiplicity, "child manifest multiplicity")?
                        .checked_add(contribution)
                        .ok_or_else(|| {
                            crate::CheckPoError::Corruption(
                                "manifest multiplicity exceeds the supported range".to_string(),
                            )
                        })?;
                let incoming = rebuild_u64(incoming, "manifest incoming edge count")?
                    .checked_sub(edge_count)
                    .ok_or_else(|| {
                        crate::CheckPoError::Corruption(
                            "manifest incoming edge count underflow".to_string(),
                        )
                    })?;
                tx.execute(
                    "UPDATE temp.rebuild_chunks
                     SET multiplicity = ?3, incoming_remaining = ?4
                     WHERE kind = ?1 AND chunk_id = ?2",
                    params![
                        child_kind,
                        child_id.as_slice(),
                        sqlite_i64_from_u64(child_multiplicity, "child manifest multiplicity")?,
                        sqlite_i64_from_u64(incoming, "manifest incoming edge count")?
                    ],
                )
                .map_err(|error| db_error(db_path, error))?;
            }
        }
        ChunkKind::Leaf => {
            let objects = {
                let mut statement = tx
                    .prepare(
                        "SELECT object_id, expected_size_bytes, occurrence_count
                         FROM temp.rebuild_leaf_objects
                         WHERE chunk_kind = ?1 AND chunk_id = ?2 ORDER BY object_id",
                    )
                    .map_err(|error| db_error(db_path, error))?;
                let rows = statement
                    .query_map(params![kind, id.as_slice()], |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(|error| db_error(db_path, error))?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| db_error(db_path, error))?
            };
            for (index, (object_id, size, occurrence_count)) in objects.into_iter().enumerate() {
                if index % 256 == 0 {
                    crate::ensure_not_cancelled(cancellation)?;
                }
                let object_id = object_id_from_sql(&object_id)?;
                let size = rebuild_u64(size, "object expected size")?;
                let occurrence_count = rebuild_u64(occurrence_count, "object occurrence count")?;
                let reference_count =
                    multiplicity.checked_mul(occurrence_count).ok_or_else(|| {
                        crate::CheckPoError::Corruption(format!(
                            "object {object_id} reference count exceeds the supported range"
                        ))
                    })?;
                increment_object_ref(tx, db_path, &object_id, size, reference_count)?;
                *aggregated_object_count =
                    aggregated_object_count.checked_add(1).ok_or_else(|| {
                        crate::CheckPoError::Corruption(
                            "aggregated object count exceeds the supported range".to_string(),
                        )
                    })?;
                report_operation_progress(
                    progress,
                    "aggregatingReferences",
                    *aggregated_object_count,
                    total_object_rows,
                    Some(object_id.to_string()),
                );
            }
        }
        ChunkKind::Root => {
            return Err(crate::CheckPoError::Corruption(
                "manifest aggregation encountered a snapshot root".to_string(),
            ))
        }
    }
    tx.execute(
        "UPDATE temp.rebuild_chunks SET processed = 1 WHERE kind = ?1 AND chunk_id = ?2",
        params![kind, id.as_slice()],
    )
    .map_err(|error| db_error(db_path, error))?;
    Ok(())
}

fn manifest_ref_sql_parts(reference: ManifestRef) -> (i64, [u8; 32]) {
    let kind = match reference.kind {
        ChunkKind::Node => 2,
        ChunkKind::Leaf => 3,
        ChunkKind::Root => 1,
    };
    (kind, *reference.id.as_bytes())
}

fn manifest_ref_from_sql(kind: i64, id: &[u8]) -> Result<ManifestRef> {
    let id: [u8; 32] = id.try_into().map_err(|_| {
        crate::CheckPoError::Corruption("manifest chunk id in rebuild table is invalid".to_string())
    })?;
    let kind = match kind {
        2 => ChunkKind::Node,
        3 => ChunkKind::Leaf,
        _ => {
            return Err(crate::CheckPoError::Corruption(
                "manifest chunk kind in rebuild table is invalid".to_string(),
            ))
        }
    };
    ManifestRef::new(kind, Digest32::from_bytes(id)).map_err(|error| {
        crate::CheckPoError::Corruption(format!(
            "invalid manifest reference in rebuild table: {error}"
        ))
    })
}

fn object_id_from_sql(id: &[u8]) -> Result<ObjectId> {
    let id: [u8; 32] = id.try_into().map_err(|_| {
        crate::CheckPoError::Corruption("object id in rebuild table is invalid".to_string())
    })?;
    Ok(ObjectId::from_digest_bytes(id))
}

fn rebuild_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        crate::CheckPoError::Corruption(format!(
            "temporary rebuild table contains an invalid {field}: {value}"
        ))
    })
}

fn snapshot_v2_index_error(
    error: crate::storage::snapshot_v2::SnapshotV2Error,
) -> crate::CheckPoError {
    crate::CheckPoError::Corruption(error.to_string())
}

fn insert_snapshot_root_summary(
    conn: &rusqlite::Connection,
    db_path: &Path,
    snapshot_id: &SnapshotId,
    root: &crate::storage::SnapshotRootHeader,
) -> Result<()> {
    conn.execute(
        "INSERT INTO snapshots(
           snapshot_id, project_id, created_at_utc, name, parent_snapshot_id,
           file_count, logical_size_bytes
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id.as_str(),
            root.project_id.as_str(),
            root.created_at_utc.as_str(),
            root.name.as_str(),
            root.parent_snapshot_id.as_ref().map(|id| id.as_str()),
            sqlite_i64_from_u64(root.summary.entry_count, "snapshot file count")?,
            sqlite_i64_from_u64(
                root.summary.logical_size_bytes,
                "snapshot logical size in bytes"
            )?,
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
    let mut object_occurrences = BTreeMap::<ObjectId, (u64, u64)>::new();
    for (index, file) in snapshot.files.iter().enumerate() {
        if index % 256 == 0 {
            crate::ensure_not_cancelled(cancellation)?;
        }
        match object_occurrences.get_mut(file.content_hash()) {
            Some((existing, _)) if *existing != file.size_bytes => {
                return Err(crate::CheckPoError::Corruption(format!(
                    "snapshot {snapshot_id} gives object {} conflicting sizes {existing} and {}",
                    file.content_hash(),
                    file.size_bytes
                )));
            }
            Some((_, count)) => {
                *count = count.checked_add(1).ok_or_else(|| {
                    crate::CheckPoError::Corruption(
                        "snapshot object occurrence count exceeds the supported range".to_string(),
                    )
                })?;
            }
            None => {
                object_occurrences.insert(file.content_hash().clone(), (file.size_bytes, 1));
            }
        }
    }
    for (object_id, (size, count)) in object_occurrences {
        increment_object_ref(&tx, db_path, &object_id, size, count)?;
    }
    tx.commit().map_err(|error| db_error(db_path, error))
}

fn increment_object_ref(
    tx: &rusqlite::Transaction<'_>,
    db_path: &Path,
    object_id: &ObjectId,
    size: u64,
    count: u64,
) -> Result<()> {
    let size = sqlite_i64_from_u64(size, "object expected size in bytes")?;
    let existing = tx
        .query_row(
            "SELECT expected_size_bytes, reference_count FROM object_refs WHERE object_id = ?1",
            params![object_id.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|error| db_error(db_path, error))?;
    match existing {
        Some((existing_size, _)) if existing_size != size => Err(crate::CheckPoError::Corruption(
            format!("object {object_id} has conflicting expected sizes {existing_size} and {size}"),
        )),
        Some((_, existing_count)) => {
            let existing_count = rebuild_u64(existing_count, "object reference count")?;
            let next_count = existing_count.checked_add(count).ok_or_else(|| {
                crate::CheckPoError::Corruption(format!(
                    "object {object_id} reference count exceeds the supported range"
                ))
            })?;
            tx.execute(
                "UPDATE object_refs
                 SET reference_count = ?3, present_size_bytes = ?2
                 WHERE object_id = ?1",
                params![
                    object_id.as_str(),
                    size,
                    sqlite_i64_from_u64(next_count, "object reference count")?
                ],
            )
            .map_err(|error| db_error(db_path, error))?;
            Ok(())
        }
        None => {
            let count = sqlite_i64_from_u64(count, "object reference count")?;
            tx.execute(
                "INSERT INTO object_refs(object_id, expected_size_bytes, reference_count, present_size_bytes)
                 VALUES(?1, ?2, ?3, ?2)",
                params![object_id.as_str(), size, count],
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
    let db_path = crate::db_path(&project.repo_root)?;
    let conn = open_snapshot_index_read_write(&db_path)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| db_error(&db_path, error))?;
    let mut object_occurrences = BTreeMap::<ObjectId, (u64, i64)>::new();
    for file in &snapshot.files {
        match object_occurrences.get_mut(file.content_hash()) {
            Some((existing, _)) if *existing != file.size_bytes => {
                return Err(crate::CheckPoError::Corruption(format!(
                    "snapshot {snapshot_id} contains conflicting sizes for object {}",
                    file.content_hash()
                )));
            }
            Some((_, count)) => {
                *count = count.checked_add(1).ok_or_else(|| {
                    crate::CheckPoError::Corruption(
                        "snapshot object occurrence count exceeds the supported range".to_string(),
                    )
                })?;
            }
            None => {
                object_occurrences.insert(file.content_hash().clone(), (file.size_bytes, 1));
            }
        }
    }
    for (object_id, (size, removed_count)) in object_occurrences {
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
        if indexed_size != expected || reference_count < removed_count {
            return Err(crate::CheckPoError::IndexUnavailable(format!(
                "index contains an invalid reference for object {object_id}"
            )));
        }
        if reference_count == removed_count {
            tx.execute(
                "DELETE FROM object_refs WHERE object_id = ?1",
                params![object_id.as_str()],
            )
        } else {
            tx.execute(
                "UPDATE object_refs SET reference_count = reference_count - ?2 WHERE object_id = ?1",
                params![object_id.as_str(), removed_count],
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
    write_snapshot_inventory_head(
        &conn,
        &db_path,
        &crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?,
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
    let db_path = crate::file_fingerprint_db_path(&project.repo_root)?;
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

fn cleanup_owned_index_temporary_files(
    anchored_indexes: &crate::storage::AnchoredRoot,
    indexes: &crate::storage::AnchoredParent,
) -> Result<()> {
    let indexes_dir = indexes.display_path();
    let names = fs::read_dir(indexes_dir)
        .map_err(|error| crate::io_error(indexes_dir, error))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|error| crate::io_error(indexes_dir, error))
        })
        .collect::<Result<Vec<_>>>()?;
    anchored_indexes.verify_parent_binding(Path::new(""), indexes)?;
    let mut removed_any = false;
    for name in names {
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
        let leaf = std::ffi::OsStr::new(name);
        match indexes.open_file(leaf) {
            Ok(file) => {
                indexes.unlink_file_if_bound(leaf, file)?;
                removed_any = true;
            }
            Err(crate::CheckPoError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                crate::diagnostics::log_warning(
                    "index-rebuild",
                    &format!(
                        "unsafe index temporary path was not removed: {}",
                        indexes_dir.join(leaf).display()
                    ),
                );
            }
        }
    }
    if removed_any {
        indexes.sync_all()?;
    }
    anchored_indexes.verify_parent_binding(Path::new(""), indexes)?;
    Ok(())
}

fn remove_bound_index_temporary(
    indexes: &crate::storage::AnchoredParent,
    temp_leaf: &std::ffi::OsStr,
    temp_file: crate::storage::AnchoredFile,
) -> Result<()> {
    match indexes.unlink_file_if_bound(temp_leaf, temp_file) {
        Ok(()) => indexes.sync_all(),
        Err(crate::CheckPoError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn publish_snapshot_index(
    indexes: &crate::storage::AnchoredParent,
    temp_leaf: &std::ffi::OsStr,
    temp_file: &crate::storage::AnchoredFile,
    live_leaf: &std::ffi::OsStr,
) -> Result<()> {
    match indexes.open_file(live_leaf) {
        Ok(live_file) => {
            let mut sync_batch = crate::storage::AnchoredParentSyncBatch::new();
            indexes.replace_from_temporary_batched(
                temp_leaf,
                temp_file,
                live_leaf,
                &live_file,
                &mut sync_batch,
            )?;
            sync_batch.flush()?;
        }
        Err(crate::CheckPoError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            indexes.rename_no_replace_to(temp_leaf, temp_file, indexes, live_leaf)?;
            indexes.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    indexes.verify_file_binding(live_leaf, temp_file)
}

fn read_snapshot_inventory_head_with_connection(
    conn: &rusqlite::Connection,
    db_path: &Path,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM index_state WHERE key = ?1",
        params![INDEX_STATE_SNAPSHOT_INVENTORY_HEAD],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| db_error(db_path, error))
}

fn write_snapshot_inventory_head(
    conn: &rusqlite::Connection,
    db_path: &Path,
    head_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO index_state(key, value) VALUES(?1, ?2)",
        params![INDEX_STATE_SNAPSHOT_INVENTORY_HEAD, head_id],
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
        CREATE TABLE IF NOT EXISTS object_integrity_fingerprints(
          object_id TEXT NOT NULL PRIMARY KEY,
          size_bytes INTEGER NOT NULL,
          fingerprint TEXT NOT NULL
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

fn open_snapshot_index_read_only(path: &Path) -> Result<BoundSnapshotIndexConnection> {
    let conn = open_bound_snapshot_index(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    Ok(conn)
}

fn open_snapshot_index_read_write(path: &Path) -> Result<BoundSnapshotIndexConnection> {
    let conn = open_bound_snapshot_index(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    if !schema_is_compatible(&conn, path)? {
        return Err(crate::CheckPoError::IndexUnavailable(
            "checkpoint index schema is incompatible".to_string(),
        ));
    }
    Ok(conn)
}

fn open_bound_snapshot_index(
    path: &Path,
    flags: OpenFlags,
) -> Result<BoundSnapshotIndexConnection> {
    let indexes_path = path.parent().ok_or_else(|| {
        crate::CheckPoError::Corruption(format!(
            "snapshot index path has no parent: {}",
            path.display()
        ))
    })?;
    let anchored_indexes = crate::storage::AnchoredRoot::open(indexes_path)?;
    let indexes = anchored_indexes.open_directory(Path::new(""), false)?;
    anchored_indexes.verify_root_binding()?;
    let leaf = path.file_name().ok_or_else(|| {
        crate::CheckPoError::Corruption(format!(
            "snapshot index path has no file name: {}",
            path.display()
        ))
    })?;
    let opened = indexes.open_file(leaf)?;
    indexes.verify_file_binding(leaf, &opened)?;
    let sqlite_path = indexes_path
        .canonicalize()
        .map_err(|error| crate::io_error(indexes_path, error))?
        .join(leaf);
    let connection = rusqlite::Connection::open_with_flags(&sqlite_path, flags)
        .map_err(|error| db_error(path, error))?;
    let reopened = indexes.open_file(leaf)?;
    indexes.verify_file_binding(leaf, &reopened)?;
    anchored_indexes.verify_root_binding()?;
    Ok(BoundSnapshotIndexConnection {
        connection,
        _database_directory: anchored_indexes,
        _indexes: indexes,
    })
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
    fn object_reference_updates_accumulate_counts_and_reject_size_conflicts() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_schema(&conn, Path::new(":memory:")).unwrap();
        let object_id = ObjectId::parse(&"0".repeat(64)).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        increment_object_ref(&tx, Path::new(":memory:"), &object_id, 12, 2).unwrap();
        increment_object_ref(&tx, Path::new(":memory:"), &object_id, 12, 3).unwrap();
        tx.commit().unwrap();
        let count = conn
            .query_row(
                "SELECT reference_count FROM object_refs WHERE object_id = ?1",
                params![object_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(count, 5);

        let tx = conn.unchecked_transaction().unwrap();
        assert!(matches!(
            increment_object_ref(&tx, Path::new(":memory:"), &object_id, 13, 1),
            Err(crate::CheckPoError::Corruption(_))
        ));
    }

    #[test]
    fn owned_index_temporary_cleanup_is_strict() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let indexes = repo.join("indexes");
        fs::create_dir_all(&indexes).unwrap();
        let owned = indexes.join(format!("snapshot-index-{}.tmp", "a".repeat(32)));
        let journal = indexes.join(format!(
            "{}-journal",
            owned.file_name().unwrap().to_string_lossy()
        ));
        let near_match = indexes.join(format!("snapshot-index-{}.tmp", "g".repeat(32)));
        fs::write(&owned, b"temp").unwrap();
        fs::write(&journal, b"journal").unwrap();
        fs::write(&near_match, b"keep").unwrap();

        let anchored_indexes_root = crate::storage::AnchoredRoot::open(&indexes).unwrap();
        let anchored_indexes = anchored_indexes_root
            .open_directory_for_mutation(Path::new(""), false)
            .unwrap();
        cleanup_owned_index_temporary_files(&anchored_indexes_root, &anchored_indexes).unwrap();

        assert!(!owned.exists());
        assert!(!journal.exists());
        assert!(near_match.exists());
    }
}
