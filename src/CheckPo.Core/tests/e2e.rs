use checkpo_core as core;
use core::{
    FileOperationType, OperationPlanKind, ProjectLocationStatus, SnapshotContent, SnapshotEntry,
    SnapshotFile, TrackedUnityFilePath,
};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

fn setup() -> (
    MutexGuard<'static, ()>,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::env::set_var("CHECKPO_DATA_DIR", &data);
    let project = temp.path().join("UnityProject");
    fs::create_dir_all(project.join("Assets/Avatar")).unwrap();
    fs::create_dir_all(project.join("Packages")).unwrap();
    fs::create_dir_all(project.join("ProjectSettings")).unwrap();
    fs::write(
        project.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    )
    .unwrap();
    (guard, temp, project, data)
}

fn repo_path(view: &core::ProjectView) -> std::path::PathBuf {
    view.storage_root_path.join("repos").join(&view.project_id)
}

fn first_regular_file_below(root: &std::path::Path) -> std::path::PathBuf {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .find(|entry| entry.file_type().is_file())
        .expect("expected at least one regular file")
        .into_path()
}

fn publish_raw_snapshot_root(repo: &std::path::Path, bytes: &[u8]) -> core::SnapshotId {
    let id = core::snapshot_id_from_bytes(bytes);
    let path = core::snapshot_path(repo, &id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    id
}

fn write_checkpoint_create_journal(
    repo: &std::path::Path,
    transaction_id: &str,
    checkpoint_id: &core::SnapshotId,
    expected_old_latest: Option<&core::SnapshotId>,
    state: &str,
) -> std::path::PathBuf {
    let root = repo.join("journals/checkpoint-create").join(transaction_id);
    let inventory_head_before = fs::read_to_string(repo.join("inventory/snapshots/head"))
        .unwrap()
        .trim()
        .to_string();
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 3,
            "transactionId": transaction_id,
            "state": state,
            "checkpointId": checkpoint_id,
            "expectedOldLatest": expected_old_latest,
            "inventoryHeadBefore": inventory_head_before,
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    root
}

fn init_project_for_test(project: &std::path::Path) -> core::Result<core::ProjectView> {
    core::init_project(project)
}

fn cleanup_journals_for_test(
    project: &std::path::Path,
    yes: bool,
) -> core::Result<core::TransactionCleanupResult> {
    let plan = core::analyze_transaction_cleanup(project)?;
    core::cleanup_journals_with_expected_plan(project, &plan, core::ApplyOptions { yes })
}

fn fingerprint_count(repo: &std::path::Path, path: &str) -> i64 {
    let conn = rusqlite::Connection::open(core::file_fingerprint_db_path(repo).unwrap()).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM file_fingerprints WHERE path = ?1",
        [path],
        |row| row.get(0),
    )
    .unwrap()
}

fn open_test_index(repo: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(core::db_path(repo).unwrap()).unwrap()
}

#[test]
fn derived_sqlite_databases_live_outside_the_repository() {
    let (_guard, _temp, project, data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);

    let index = core::db_path(&repo).unwrap();
    let fingerprints = core::file_fingerprint_db_path(&repo).unwrap();

    assert!(index.starts_with(data.join("derived-indexes")));
    assert!(fingerprints.starts_with(data.join("derived-indexes")));
    assert!(!index.starts_with(&repo));
    assert!(!fingerprints.starts_with(&repo));
    assert!(index.is_file());
    assert!(fingerprints.is_file());
    assert!(!repo.join("indexes/local.db").exists());
    assert!(!repo.join("indexes/working-tree-cache.db").exists());
}

fn assert_copied_project_error(error: &core::CheckPoError) {
    assert!(matches!(
        error,
        core::CheckPoError::CopiedProjectSuspected(_)
    ));
    assert!(error.to_string().contains("appears to be a copy"));
}

#[test]
fn checkpoint_create_metrics_classify_initial_reused_and_changed_work() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/One.asset"), b"one").unwrap();
    fs::write(project.join("Assets/Avatar/Two.asset"), b"two").unwrap();
    init_project_for_test(&project).unwrap();

    let first = core::create_checkpoint_profiled(&project, "first", Default::default()).unwrap();
    assert_eq!(first.summary.file_count, 3);
    assert_eq!(first.create_metrics.scan.hashed_file_count, 3);
    assert_eq!(first.create_metrics.scan.reused_file_count, 0);
    assert!(first.create_metrics.object_store_parallelism >= 1);
    assert_eq!(first.create_metrics.io.loose_objects.written_count, 3);
    assert_eq!(
        first
            .create_metrics
            .io
            .loose_objects
            .post_write_readback_count,
        3
    );
    assert!(
        first.create_metrics.io.loose_objects.directory_fsync_count
            <= first.create_metrics.io.loose_objects.written_count * 2 + 2
    );
    assert!(first.create_metrics.io.loose_objects.directory_create_count > 0);
    assert!(first.create_metrics.io.loose_objects.hash_operation_count > 0);
    assert!(first.create_metrics.io.manifest_chunks.written_count > 0);
    assert!(first.create_metrics.io.manifest_chunks.hash_operation_count > 0);
    assert!(
        first
            .create_metrics
            .io
            .manifest_chunks
            .directory_fsync_count
            <= first.create_metrics.io.manifest_chunks.written_count * 2 + 1,
        "manifest chunk metrics: {:?}",
        first.create_metrics.io.manifest_chunks
    );
    assert_eq!(first.create_metrics.io.snapshot_root.written_count, 1);
    let measured_total = first
        .create_metrics
        .setup_micros
        .saturating_add(first.create_metrics.baseline_load_micros)
        .saturating_add(first.create_metrics.scan_total_micros)
        .saturating_add(first.create_metrics.object_preload_micros)
        .saturating_add(first.create_metrics.object_store_micros)
        .saturating_add(first.create_metrics.object_integrity_cache_update_micros)
        .saturating_add(first.create_metrics.manifest_build_micros)
        .saturating_add(first.create_metrics.manifest_store_micros)
        .saturating_add(first.create_metrics.durability_barrier_micros)
        .saturating_add(first.create_metrics.object_readback_micros)
        .saturating_add(first.create_metrics.root_journal_ref_commit_micros)
        .saturating_add(first.create_metrics.snapshot_index_update_micros)
        .saturating_add(first.create_metrics.file_fingerprint_update_micros)
        .saturating_add(first.create_metrics.unattributed_micros);
    assert_eq!(measured_total, first.create_metrics.total_micros);

    let second = core::create_checkpoint_profiled(&project, "second", Default::default()).unwrap();
    assert_eq!(second.create_metrics.scan.hashed_file_count, 0);
    assert_eq!(second.create_metrics.scan.reused_file_count, 3);
    assert_eq!(second.create_metrics.io.loose_objects.written_count, 0);
    assert!(second.create_metrics.io.manifest_chunks.existing_count > 0);
    assert_eq!(
        second
            .create_metrics
            .io
            .manifest_chunks
            .post_write_readback_micros,
        0
    );
    assert_eq!(second.create_metrics.io.snapshot_root.written_count, 1);

    fs::write(project.join("Assets/Avatar/One.asset"), b"one changed").unwrap();
    let third = core::create_checkpoint_profiled(&project, "third", Default::default()).unwrap();
    assert_eq!(third.create_metrics.scan.hashed_file_count, 1);
    assert_eq!(third.create_metrics.scan.reused_file_count, 2);
    assert_eq!(third.create_metrics.io.loose_objects.written_count, 1);
    assert!(third.create_metrics.io.manifest_chunks.written_count > 0);
}

#[test]
fn checkpoint_deduplicates_new_objects_before_copy_and_fsync() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/One.asset"), b"same payload").unwrap();
    fs::write(project.join("Assets/Avatar/Two.asset"), b"same payload").unwrap();
    init_project_for_test(&project).unwrap();

    let result =
        core::create_checkpoint_profiled(&project, "deduplicated", Default::default()).unwrap();

    assert_eq!(result.summary.file_count, 3);
    assert_eq!(result.create_metrics.io.loose_objects.written_count, 2);
    assert_eq!(result.create_metrics.io.loose_objects.file_fsync_count, 2);
    assert_eq!(result.create_metrics.io.loose_objects.checked_count, 2);
    assert_eq!(
        result
            .create_metrics
            .io
            .loose_objects
            .post_write_readback_count,
        2
    );
}

#[test]
fn tracked_unity_file_path_validation() {
    for path in [
        "Assets/Foo.prefab",
        "Assets/ Avatar/Foo.prefab",
        "Assets/Foo.prefab.meta",
        "Packages/manifest.json",
        "ProjectSettings/ProjectSettings.asset",
    ] {
        assert!(TrackedUnityFilePath::parse(path).is_ok(), "{path}");
    }

    for path in [
        "",
        "README.md",
        ".git/config",
        "UserSettings/foo",
        "Library/foo",
        "Assets",
        "Assets/",
        "Packages",
        "ProjectSettings",
        "Assets/../README.md",
        "../README.md",
        "/tmp/foo",
        "C:\\foo",
        "Assets\\Foo.prefab",
        "Assets//Foo.prefab",
        "Assets/./Foo.prefab",
        "Assets/Foo.prefab ",
        "Assets/Foo.prefab.",
        "Assets/CON.txt",
        "Assets/LPT1.asset",
        ".checkpo/project.json",
    ] {
        assert!(TrackedUnityFilePath::parse(path).is_err(), "{path}");
    }
}

#[test]
fn validated_ids_and_paths_reject_invalid_json_values() {
    assert!(serde_json::from_str::<core::TrackedUnityFilePath>(r#""README.md""#).is_err());
    assert!(serde_json::from_str::<core::TrackedUnityFilePath>(r#""Assets/Foo.prefab""#).is_ok());
    assert!(serde_json::from_str::<core::ObjectId>(r#""short""#).is_err());
    assert!(
        serde_json::from_str::<core::ProjectId>(r#""11111111111111111111111111111111""#).is_ok()
    );
    assert!(serde_json::from_str::<core::ProjectId>(r#""../escape""#).is_err());
    assert!(serde_json::from_str::<core::SnapshotId>(
        r#""ABCDEFabcdefABCDEFabcdefABCDEFabcdefABCDEFabcdefABCDEFabcdefAB""#
    )
    .is_err());
}

#[test]
fn project_marker_rejects_invalid_project_id() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    fs::write(
        project.join(".checkpo/project.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "projectId": "../escape",
            "createdAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let error = core::load_project(&project).unwrap_err();

    assert!(matches!(error, core::CheckPoError::InvalidId(_)));
}

#[test]
fn init_rejects_checkpoint_repository_inside_tracked_folder() {
    let (_guard, _temp, project, _data) = setup();
    std::env::set_var("CHECKPO_DATA_DIR", project.join("Assets/CheckpointStorage"));

    let error = core::init_project(&project).unwrap_err();

    assert!(error.to_string().contains("inside the Unity project"));
}

#[test]
fn canonical_snapshot_v2_root_is_binary_and_stable() {
    let snapshot = SnapshotFile {
        schema_version: 2,
        project_id: core::ProjectId::parse("11111111111111111111111111111111").unwrap(),
        parent_snapshot_id: None,
        created_at_utc: "2026-01-01T00:00:00Z".to_string(),
        name: "Initial".to_string(),
        tool_version: "0.1.0".to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files: vec![SnapshotEntry {
            path: TrackedUnityFilePath::parse("Assets/Avatar/Foo.prefab").unwrap(),
            size_bytes: 3,
            modified_at_utc: "2026-01-01T00:00:01Z".to_string(),
            content: SnapshotContent::Whole {
                hash: core::ObjectId::parse(&"0".repeat(64)).unwrap(),
                size_bytes: 3,
            },
        }],
    };
    let bytes = core::canonical_snapshot_bytes(&snapshot).unwrap();

    assert_eq!(&bytes[..8], b"CPMRKL2\0");
    assert_eq!(bytes, core::canonical_snapshot_bytes(&snapshot).unwrap());
    assert_eq!(core::snapshot_id_from_bytes(&bytes).as_str().len(), 64);
}

#[test]
fn checkpoint_creates_snapshot_objects_and_latest_ref() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let summary = core::create_checkpoint(
        &project,
        "Initial",
        core::CreateCheckpointOptions::default(),
    )
    .unwrap();

    let repo = repo_path(&view);
    assert!(core::snapshot_path(&repo, &summary.checkpoint_id).is_file());
    assert_eq!(
        fs::read_to_string(repo.join("refs/latest")).unwrap(),
        summary.checkpoint_id.to_string()
    );
    let snapshot = core::load_snapshot(&repo, &summary.checkpoint_id).unwrap();
    assert_eq!(snapshot.files[0].path.as_str(), "Assets/Avatar/Foo.prefab");
    assert!(core::object_path(&repo, snapshot.files[0].content_hash()).is_file());
}

#[test]
fn checkpoint_rejects_file_changed_after_scan_before_store() {
    let (_guard, _temp, project, _data) = setup();
    let file_path = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file_path, "one").unwrap();
    init_project_for_test(&project).unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_for_progress = Arc::clone(&changed);
    let file_for_progress = file_path.clone();

    let error = core::create_checkpoint(
        &project,
        "Initial",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                if progress.phase == "storeCheckpoint"
                    && progress.completed == 0
                    && !changed_for_progress.swap(true, Ordering::SeqCst)
                {
                    fs::write(&file_for_progress, "two").unwrap();
                }
            })),
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(
        matches!(error, core::CheckPoError::WorkingTreeChanged(_)),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn checkpoint_does_not_follow_source_symlink_swapped_after_scan() {
    let (_guard, temp, project, _data) = setup();
    let file_path = project.join("Assets/Avatar/Foo.prefab");
    let outside = temp.path().join("outside.prefab");
    fs::write(&file_path, "same payload").unwrap();
    fs::write(&outside, "same payload").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let swapped = Arc::new(AtomicBool::new(false));
    let swapped_for_progress = Arc::clone(&swapped);
    let file_for_progress = file_path.clone();
    let outside_for_progress = outside.clone();

    let error = core::create_checkpoint(
        &project,
        "Reject swapped source",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                if progress.phase == "storeCheckpoint"
                    && progress.completed == 0
                    && !swapped_for_progress.swap(true, Ordering::SeqCst)
                {
                    fs::remove_file(&file_for_progress).unwrap();
                    std::os::unix::fs::symlink(&outside_for_progress, &file_for_progress).unwrap();
                }
            })),
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(swapped.load(Ordering::SeqCst));
    assert!(
        matches!(error, core::CheckPoError::WorkingTreeChanged(_)),
        "{error:?}"
    );
    assert!(core::list_checkpoints(&project).unwrap().is_empty());
    assert!(!repo_path(&view).join("refs/latest").exists());
}

#[test]
fn checkpoint_final_object_readback_failure_does_not_publish_root_or_latest() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), b"one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let object = core::object_path(&repo, &core::hash_bytes(b"one"));
    let corrupted = Arc::new(AtomicBool::new(false));
    let corrupted_for_progress = Arc::clone(&corrupted);

    let error = core::create_checkpoint(
        &project,
        "must-not-publish",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                if progress.phase == "readbackCheckpoint"
                    && progress.completed == 0
                    && !corrupted_for_progress.swap(true, Ordering::SeqCst)
                {
                    fs::write(&object, b"bad").unwrap();
                }
            })),
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(corrupted.load(Ordering::SeqCst));
    assert!(matches!(error, core::CheckPoError::ObjectHashMismatch(_)));
    assert!(core::list_checkpoints(&project).unwrap().is_empty());
    assert!(!repo.join("refs/latest").exists());
    assert!(fs::read_dir(repo.join("snapshots/v2"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn checkpoint_reports_write_sync_readback_and_commit_in_order() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), b"one").unwrap();
    init_project_for_test(&project).unwrap();
    let phases = Arc::new(Mutex::new(Vec::new()));
    let phases_for_progress = Arc::clone(&phases);

    core::create_checkpoint(
        &project,
        "phases",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                phases_for_progress.lock().unwrap().push(progress.phase);
            })),
            ..Default::default()
        },
    )
    .unwrap();

    let phases = phases.lock().unwrap();
    let first = |phase: &str| phases.iter().position(|item| item == phase).unwrap();
    assert!(first("storeCheckpoint") < first("writeCheckpointMetadata"));
    assert!(first("writeCheckpointMetadata") < first("syncCheckpoint"));
    assert!(first("syncCheckpoint") < first("readbackCheckpoint"));
    assert!(first("readbackCheckpoint") < first("commitCheckpoint"));
    assert!(first("commitCheckpoint") < first("complete"));
}

#[test]
fn checkpoint_cancel_during_directory_barrier_keeps_root_unpublished() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), b"one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let cancellation = core::CancellationToken::new();
    let cancellation_for_progress = cancellation.clone();

    let error = core::create_checkpoint(
        &project,
        "cancel-before-root",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                if progress.phase == "syncCheckpoint" && progress.completed == 1 {
                    cancellation_for_progress.cancel();
                }
            })),
            cancellation: Some(cancellation),
            ..Default::default()
        },
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
    assert!(core::list_checkpoints(&project).unwrap().is_empty());
    assert!(!repo.join("refs/latest").exists());
    assert!(fs::read_dir(repo.join("snapshots/v2"))
        .unwrap()
        .next()
        .is_none());
}

#[cfg(windows)]
#[test]
fn checkpoint_rejects_unreadable_tracked_file_without_creating_snapshot() {
    use std::os::windows::fs::OpenOptionsExt;

    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let _exclusive = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&file)
        .unwrap();

    let error = core::create_checkpoint(&project, "Incomplete", Default::default()).unwrap_err();

    assert!(matches!(error, core::CheckPoError::User(_)));
    assert!(error.to_string().contains("could not be read"));
    assert!(core::list_checkpoints(&project).unwrap().is_empty());
    assert!(!repo_path(&view).join("refs/latest").exists());
}

#[test]
fn checkpoint_repairs_changed_object_even_when_size_is_unchanged() {
    let (_guard, _temp, project, _data) = setup();
    let source = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&source, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::write(&object, "two").unwrap();

    let second = core::create_checkpoint(&project, "Repair", Default::default()).unwrap();

    assert_eq!(second.newly_stored_bytes, 3);
    assert_eq!(fs::read_to_string(&object).unwrap(), "one");
    assert!(core::verify_project(&project, true).unwrap().is_valid);
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_symlinked_object_shard_without_touching_outside() {
    let (_guard, temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "content").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let object_id = core::hash_bytes(b"content");
    let first = &object_id.as_str()[..2];
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let outside_object = outside.join(object_id.as_str());
    fs::write(&outside_object, "changed").unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("objects/loose").join(first)).unwrap();

    let error = core::create_checkpoint(&project, "unsafe", Default::default()).unwrap_err();

    assert!(matches!(error, core::CheckPoError::Corruption(_)));
    assert_eq!(fs::read_to_string(outside_object).unwrap(), "changed");
    assert!(core::list_checkpoints(&project).unwrap().is_empty());
}

#[test]
fn checkpoint_does_not_trust_cached_object_id_without_latest_snapshot_anchor() {
    let (_guard, _temp, project, _data) = setup();
    let foo = project.join("Assets/Avatar/Foo.prefab");
    let bar = project.join("Assets/Avatar/Bar.prefab");
    fs::write(&foo, "one").unwrap();
    fs::write(&bar, "two").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let first_snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let foo_hash = first_snapshot
        .files
        .iter()
        .find(|entry| entry.path.as_str() == "Assets/Avatar/Foo.prefab")
        .unwrap()
        .content_hash()
        .clone();
    let bar_hash = first_snapshot
        .files
        .iter()
        .find(|entry| entry.path.as_str() == "Assets/Avatar/Bar.prefab")
        .unwrap()
        .content_hash()
        .clone();
    let conn = rusqlite::Connection::open(core::file_fingerprint_db_path(&repo).unwrap()).unwrap();
    conn.execute(
        "UPDATE file_fingerprints SET object_id = ?1 WHERE path = ?2",
        rusqlite::params![bar_hash.as_str(), "Assets/Avatar/Foo.prefab"],
    )
    .unwrap();
    drop(conn);

    let second = core::create_checkpoint(&project, "Anchored", Default::default()).unwrap();
    let second_snapshot = core::load_snapshot(&repo, &second.checkpoint_id).unwrap();
    let second_foo_hash = second_snapshot
        .files
        .iter()
        .find(|entry| entry.path.as_str() == "Assets/Avatar/Foo.prefab")
        .unwrap()
        .content_hash();

    assert_eq!(second_foo_hash, &foo_hash);
    assert_ne!(second_foo_hash, &bar_hash);
}

#[test]
fn cached_checkpoint_captures_scan_state_when_file_changes_before_store() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let expected = core::load_snapshot(&repo, &first.checkpoint_id)
        .unwrap()
        .files[0]
        .content_hash()
        .clone();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_for_progress = Arc::clone(&changed);
    let file_for_progress = file.clone();

    let second = core::create_checkpoint(
        &project,
        "Fuzzy scan",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                if progress.phase == "storeCheckpoint"
                    && progress.completed == 0
                    && !changed_for_progress.swap(true, Ordering::SeqCst)
                {
                    fs::write(&file_for_progress, "two").unwrap();
                }
            })),
            ..Default::default()
        },
    )
    .unwrap();

    let captured = core::load_snapshot(&repo, &second.checkpoint_id)
        .unwrap()
        .files[0]
        .content_hash()
        .clone();
    assert_eq!(captured, expected);
    assert!(
        core::diff_checkpoint(&project, second.checkpoint_id.as_str())
            .unwrap()
            .modified
            .iter()
            .any(|path| path == "Assets/Avatar/Foo.prefab")
    );
}

#[test]
fn cached_checkpoint_preserves_scan_state_when_file_changes_before_publish() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let expected = core::load_snapshot(&repo, &first.checkpoint_id)
        .unwrap()
        .files[0]
        .content_hash()
        .clone();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_for_progress = Arc::clone(&changed);
    let file_for_progress = file.clone();

    let second = core::create_checkpoint(
        &project,
        "Late change",
        core::CreateCheckpointOptions {
            progress: Some(Arc::new(move |progress| {
                if progress.phase == "readbackCheckpoint"
                    && !changed_for_progress.swap(true, Ordering::SeqCst)
                {
                    fs::write(&file_for_progress, "two").unwrap();
                }
            })),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(changed.load(Ordering::SeqCst));
    let captured = core::load_snapshot(&repo, &second.checkpoint_id)
        .unwrap()
        .files[0]
        .content_hash()
        .clone();
    assert_eq!(captured, expected);
    assert!(
        core::diff_checkpoint(&project, second.checkpoint_id.as_str())
            .unwrap()
            .modified
            .iter()
            .any(|path| path == "Assets/Avatar/Foo.prefab")
    );
}

#[test]
fn full_verify_detects_same_size_object_tampering() {
    let (_guard, _temp, project, _data) = setup();
    let source = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&source, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::write(&object, "two").unwrap();

    let quick = core::verify_project(&project, false).unwrap();
    let full = core::verify_project(&project, true).unwrap();

    assert!(quick.is_valid);
    assert!(!full.is_valid);
    assert!(full
        .errors
        .iter()
        .any(|error| error.contains("expected") && error.contains("got")));
}

#[test]
fn project_verify_reports_shared_checkpoint_objects_once() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    let second = core::create_checkpoint(&project, "Second", Default::default()).unwrap();
    assert_ne!(first.checkpoint_id, second.checkpoint_id);

    for full in [false, true] {
        let verified_object_count = AtomicUsize::new(0);
        let progress = |progress: core::OperationProgress| {
            if progress.phase == "verifyObjects" {
                verified_object_count.fetch_add(1, Ordering::SeqCst);
                assert_eq!(progress.total, first.file_count);
            }
        };

        let result = core::verify_project_with_progress_and_cancellation(
            &project,
            full,
            Some(&progress),
            None,
        )
        .unwrap();

        assert!(result.is_valid, "{:?}", result.errors);
        assert_eq!(
            verified_object_count.load(Ordering::SeqCst),
            first.file_count
        );
    }
}

#[test]
fn project_verify_rejects_conflicting_expected_sizes_across_manifests() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let mut conflicting = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    let object_id = conflicting.files[0].content_hash().clone();
    conflicting.parent_snapshot_id = Some(checkpoint.checkpoint_id);
    conflicting.name = "conflicting size".to_string();
    conflicting.files[0].size_bytes = 4;
    conflicting.files[0].content = core::SnapshotContent::Whole {
        hash: object_id,
        size_bytes: 4,
    };
    let conflicting_id = core::__debug_test_save_snapshot(&repo, &conflicting).unwrap();
    core::__debug_test_add_snapshot_to_inventory(
        &repo,
        &core::ProjectId::parse(&view.project_id).unwrap(),
        &conflicting_id,
    )
    .unwrap();

    let result = core::verify_project(&project, false).unwrap();

    assert!(!result.is_valid);
    assert!(result
        .errors
        .iter()
        .any(|error| error.contains("conflicting expected sizes")));
}

#[test]
fn project_verify_honors_pre_cancelled_token() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let cancellation = core::CancellationToken::new();
    cancellation.cancel();

    let error = core::verify_project_with_progress_and_cancellation(
        &project,
        false,
        None,
        Some(&cancellation),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
}

#[test]
fn project_verify_honors_cancellation_after_last_object_progress() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let cancellation = core::CancellationToken::new();
    let progress_cancellation = cancellation.clone();
    let progress = move |progress: core::OperationProgress| {
        if progress.phase == "verifyObjects" && progress.completed == progress.total {
            progress_cancellation.cancel();
        }
    };

    let error = core::verify_project_with_progress_and_cancellation(
        &project,
        true,
        Some(&progress),
        Some(&cancellation),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
}

#[test]
fn checkpoint_verify_honors_cancellation_after_last_object_progress() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let cancellation = core::CancellationToken::new();
    let progress_cancellation = cancellation.clone();
    let progress = move |progress: core::OperationProgress| {
        if progress.phase == "verifyObjects" && progress.completed == progress.total {
            progress_cancellation.cancel();
        }
    };

    let error = core::verify_checkpoint_with_progress_and_cancellation(
        &project,
        checkpoint.checkpoint_id.as_str(),
        true,
        Some(&progress),
        Some(&cancellation),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
}

#[test]
fn next_checkpoint_repairs_same_size_object_tampering() {
    let (_guard, _temp, project, _data) = setup();
    let source = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&source, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::write(&object, "two").unwrap();
    assert!(!core::verify_project(&project, true).unwrap().is_valid);

    core::create_checkpoint(&project, "Repair", Default::default()).unwrap();

    assert_eq!(fs::read_to_string(&object).unwrap(), "one");
    assert!(core::verify_project(&project, true).unwrap().is_valid);
}

#[test]
fn unchanged_checkpoint_does_not_rewrite_object_integrity_cache() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    let conn = rusqlite::Connection::open(core::file_fingerprint_db_path(&repo).unwrap()).unwrap();
    let cached_object_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM object_integrity_fingerprints",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(cached_object_count > 0);
    conn.execute_batch(
        "CREATE TRIGGER reject_redundant_object_integrity_cache_write
         BEFORE INSERT ON object_integrity_fingerprints
         BEGIN
           SELECT RAISE(ABORT, 'object integrity cache was rewritten');
         END;",
    )
    .unwrap();
    drop(conn);

    let checkpoint = core::create_checkpoint(&project, "Unchanged", Default::default()).unwrap();

    assert!(!checkpoint
        .warnings
        .iter()
        .any(|warning| warning.contains("Object integrity fingerprint update failed")));
}

#[cfg(unix)]
#[test]
fn restore_rejects_symlink_parent_without_writing_outside_project() {
    let (_guard, temp, project, _data) = setup();
    let avatar_dir = project.join("Assets/Avatar");
    let file = avatar_dir.join("Foo.prefab");
    fs::write(&file, "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_file(&file).unwrap();
    fs::remove_dir_all(&avatar_dir).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, &avatar_dir).unwrap();

    let error = core::preview_restore(&project, checkpoint.as_str()).unwrap_err();

    assert!(matches!(error, core::CheckPoError::InvalidTrackedPath(_)));
    assert!(!outside.join("Foo.prefab").exists());
}

#[cfg(unix)]
#[test]
fn discard_rejects_symlink_parent_without_touching_outside_project() {
    let (_guard, temp, project, _data) = setup();
    let avatar_dir = project.join("Assets/Avatar");
    let file = avatar_dir.join("Foo.prefab");
    fs::write(&file, "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_file(&file).unwrap();
    fs::remove_dir_all(&avatar_dir).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("Foo.prefab"), "two").unwrap();
    std::os::unix::fs::symlink(&outside, &avatar_dir).unwrap();
    let error = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::InvalidTrackedPath(_)));
    assert_eq!(
        fs::read_to_string(outside.join("Foo.prefab")).unwrap(),
        "two"
    );
}

#[cfg(unix)]
#[test]
fn recovery_rejects_symlink_parent_without_touching_outside_project() {
    let (_guard, temp, project, _data) = setup();
    let avatar_dir = project.join("Assets/Avatar");
    let file = avatar_dir.join("Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(&file, "two").unwrap();
    let plan = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    let repo = repo_path(&view);
    let tx = repo.join("journals/transactions/symlinktx");
    fs::create_dir_all(tx.join("backup/Assets/Avatar")).unwrap();
    fs::write(tx.join("backup/Assets/Avatar/Foo.prefab"), "two").unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "symlinktx",
            "state": "applying",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "discard",
            "selectedPaths": null,
            "operations": plan.operations,
            "directoriesToRemove": plan.directories_to_remove,
            "directoriesToCreate": plan.directories_to_create,
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(&file).unwrap();
    fs::remove_dir_all(&avatar_dir).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("Foo.prefab"), "outside").unwrap();
    std::os::unix::fs::symlink(&outside, &avatar_dir).unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.failed_transaction_count, 1);
    assert_eq!(
        fs::read_to_string(outside.join("Foo.prefab")).unwrap(),
        "outside"
    );
}

#[cfg(unix)]
#[test]
fn recovery_rejects_symlink_backup_without_touching_project_file() {
    let (_guard, temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(&file, "two").unwrap();
    let plan = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    fs::write(&file, "one").unwrap();
    let repo = repo_path(&view);
    let tx = repo.join("journals/transactions/symlinkbackuptx");
    fs::create_dir_all(tx.join("backup/Assets/Avatar")).unwrap();
    let outside = temp.path().join("outside.prefab");
    fs::write(&outside, "two").unwrap();
    std::os::unix::fs::symlink(&outside, tx.join("backup/Assets/Avatar/Foo.prefab")).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "symlinkbackuptx",
            "state": "applying",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "discard",
            "selectedPaths": null,
            "operations": plan.operations,
            "directoriesToRemove": plan.directories_to_remove,
            "directoriesToCreate": plan.directories_to_create,
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "two");
}

#[cfg(unix)]
#[test]
fn checkpoint_and_apply_reject_tracked_symlink_file() {
    let (_guard, temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    let link = project.join("Assets/Avatar/Linked.prefab");
    let outside = temp.path().join("outside.prefab");
    fs::write(&file, "one").unwrap();
    fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    init_project_for_test(&project).unwrap();

    let error = core::create_checkpoint(&project, "Initial", Default::default()).unwrap_err();
    assert!(error.to_string().contains("symbolic links"));

    fs::remove_file(&link).unwrap();
    fs::write(&link, "tracked").unwrap();
    let checkpoint_with_link_as_file =
        core::create_checkpoint(&project, "Link as file", Default::default()).unwrap();
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let error = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Linked.prefab".to_string()],
        Some(checkpoint_with_link_as_file.checkpoint_id.as_str()),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::InvalidTrackedPath(_)));
}

#[cfg(unix)]
#[test]
fn quick_verify_rejects_symlink_object_without_following_it() {
    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    let outside = temp.path().join("outside-object");
    fs::write(&outside, fs::read(&object).unwrap()).unwrap();
    fs::remove_file(&object).unwrap();
    std::os::unix::fs::symlink(&outside, &object).unwrap();

    let quick = core::verify_project(&project, false).unwrap();

    assert!(!quick.is_valid);
    assert!(quick.errors.iter().any(|error| error.contains("no-follow")));
    assert!(outside.is_file());
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_tracked_root_symlink() {
    let (_guard, temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let outside = temp.path().join("outside-assets");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("Outside.asset"), "outside").unwrap();
    fs::remove_dir_all(project.join("Assets")).unwrap();
    std::os::unix::fs::symlink(&outside, project.join("Assets")).unwrap();

    let error = core::create_checkpoint(&project, "Unsafe", Default::default()).unwrap_err();

    assert!(error.to_string().contains("unsafe Assets"));
    assert_eq!(
        fs::read_to_string(outside.join("Outside.asset")).unwrap(),
        "outside"
    );
}

#[cfg(unix)]
#[test]
fn journal_cleanup_rejects_linked_repository_directory() {
    let (_guard, temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let journals = repo.join("journals/transactions");
    let outside = temp.path().join("outside-journals");
    fs::remove_dir(&journals).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::create_dir(outside.join("important")).unwrap();
    fs::write(outside.join("important/data.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, &journals).unwrap();

    assert!(cleanup_journals_for_test(&project, true).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("important/data.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn incompatible_sqlite_index_requires_explicit_rebuild_without_dropping_live_db_on_read() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let index_path = core::db_path(&repo).unwrap();
    fs::remove_file(&index_path).unwrap();
    let conn = open_test_index(&repo);
    conn.execute_batch(
        "
        CREATE TABLE snapshots(old_snapshot_id TEXT PRIMARY KEY);
        CREATE TABLE legacy_cache(value TEXT);
        CREATE TABLE \"legacy-cache\"(value TEXT);
        ",
    )
    .unwrap();
    drop(conn);

    let context = core::load_project(&project).unwrap();
    let status = core::checkpoint_index_status(&context).unwrap();
    assert_eq!(status.state, core::CheckpointIndexState::Incompatible);
    assert!(matches!(
        core::list_checkpoints(&project),
        Err(core::CheckPoError::IndexUnavailable(_))
    ));

    let conn = open_test_index(&repo);
    let legacy_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'legacy_cache'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy_count, 1);
    drop(conn);

    core::rebuild_index(&project).unwrap();

    let status = core::checkpoint_index_status(&context).unwrap();
    assert_eq!(status.state, core::CheckpointIndexState::Current);
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    let conn = open_test_index(&repo);
    let schema_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(schema_version, 5);
    let snapshot_entries_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'snapshot_entries'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(snapshot_entries_count, 0);
    let invalid_ref_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM object_refs WHERE reference_count != 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invalid_ref_count, 0);
    let legacy_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'legacy_cache'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy_count, 0);
    let dashed_legacy_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'legacy-cache'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dashed_legacy_count, 0);
}

#[test]
fn list_and_storage_summary_do_not_rebuild_missing_sqlite_index() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    core::create_checkpoint(&project, "two", Default::default()).unwrap();
    let repo = repo_path(&view);
    fs::remove_file(core::db_path(&repo).unwrap()).unwrap();

    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Missing
    );
    assert!(matches!(
        core::list_checkpoints(&project),
        Err(core::CheckPoError::IndexUnavailable(_))
    ));
    assert!(matches!(
        core::storage_summary(&project),
        Err(core::CheckPoError::IndexUnavailable(_))
    ));
    assert!(!core::db_path(&repo).unwrap().exists());

    core::rebuild_index(&project).unwrap();
    let checkpoints = core::list_checkpoints(&project).unwrap();
    let summary = core::storage_summary(&project).unwrap();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(summary.checkpoint_count, 2);
    assert_eq!(summary.unique_blob_count, 3);
    assert!(core::db_path(&repo).unwrap().is_file());
}

#[test]
fn storage_summary_counts_roots_manifests_and_objects_on_disk() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let expected = [
        repo.join("snapshots/v2"),
        repo.join("manifests/v2"),
        repo.join("objects/loose"),
    ]
    .into_iter()
    .flat_map(|root| walkdir::WalkDir::new(root).follow_links(false))
    .filter_map(std::result::Result::ok)
    .filter(|entry| entry.file_type().is_file())
    .map(|entry| entry.metadata().unwrap().len())
    .sum::<u64>();

    let summary = core::storage_summary(&project).unwrap();
    assert_eq!(summary.stored_size_bytes, expected);
    assert!(summary.stored_size_bytes > 3);
}

#[test]
fn storage_index_summary_uses_only_indexed_metrics() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let context = core::load_project(&project).unwrap();

    let exact = core::storage_summary_from_index(&context).unwrap();
    let indexed = core::storage_index_summary_from_index(&context).unwrap();

    assert_eq!(indexed.checkpoint_count, exact.checkpoint_count);
    assert_eq!(indexed.logical_size_bytes, exact.logical_size_bytes);
    assert_eq!(indexed.unique_blob_count, exact.unique_blob_count);
    assert!(exact.stored_size_bytes > 0);
}

#[test]
fn storage_summary_rejects_an_exclusive_repository_operation() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let reported = Arc::new(AtomicBool::new(false));
    let project_for_create = project.clone();
    let create_worker = {
        let entered = entered.clone();
        let release = release.clone();
        std::thread::spawn(move || {
            core::create_checkpoint(
                project_for_create,
                "one",
                core::CreateCheckpointOptions {
                    progress: Some(Arc::new(move |_| {
                        if !reported.swap(true, Ordering::SeqCst) {
                            entered.wait();
                            release.wait();
                        }
                    })),
                    ..Default::default()
                },
            )
        })
    };
    entered.wait();
    assert!(matches!(
        core::storage_summary(&project),
        Err(core::CheckPoError::RepositoryLocked(_))
    ));
    release.wait();
    assert!(create_worker.join().unwrap().is_ok());
    assert!(core::storage_summary(&project).is_ok());
}

#[test]
fn rename_checkpoint_preserves_id_and_applies_to_all_summary_paths() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let created = core::create_checkpoint(&project, "before", Default::default()).unwrap();

    let renamed =
        core::rename_checkpoint(&project, created.checkpoint_id.as_str(), "after").unwrap();

    assert_eq!(renamed.checkpoint_id, created.checkpoint_id);
    assert_eq!(renamed.name, "after");

    let checkpoints = core::list_checkpoints(&project).unwrap();
    assert_eq!(checkpoints[0].checkpoint_id, created.checkpoint_id);
    assert_eq!(checkpoints[0].name, "after");

    let context = core::load_project(&project).unwrap();
    let (indexed_checkpoints, storage) =
        core::checkpoint_summaries_and_storage_summary_from_index(&context).unwrap();
    assert_eq!(indexed_checkpoints[0].checkpoint_id, created.checkpoint_id);
    assert_eq!(indexed_checkpoints[0].name, "after");
    assert_eq!(storage.checkpoint_count, 1);

    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &created.checkpoint_id).unwrap();
    assert_eq!(snapshot.name, "before");
}

#[test]
fn corrupt_checkpoint_display_names_do_not_block_checkpoint_list() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let created = core::create_checkpoint(&project, "before", Default::default()).unwrap();
    let names_path = repo_path(&view).join("refs").join("checkpoint_names.json");
    fs::write(&names_path, "not json").unwrap();

    let checkpoints = core::list_checkpoints(&project).unwrap();

    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_id, created.checkpoint_id);
    assert_eq!(checkpoints[0].name, "before");
    assert!(!checkpoints[0].warnings.is_empty());

    let context = core::load_project(&project).unwrap();
    let list_result = core::list_checkpoints_with_warnings_for_project(&context).unwrap();
    assert!(!list_result.warnings.is_empty());
    let (indexed_checkpoints, _) =
        core::checkpoint_summaries_and_storage_summary_from_index(&context).unwrap();
    assert_eq!(indexed_checkpoints[0].name, "before");
    assert!(!indexed_checkpoints[0].warnings.is_empty());
}

#[test]
fn corrupt_checkpoint_display_names_block_rename_and_delete_without_overwriting_metadata() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let created = core::create_checkpoint(&project, "before", Default::default()).unwrap();
    let repo = repo_path(&view);
    let names_path = repo.join("refs").join("checkpoint_names.json");
    let corrupt_bytes = b"not json";
    fs::write(&names_path, corrupt_bytes).unwrap();

    let rename_error =
        core::rename_checkpoint(&project, created.checkpoint_id.as_str(), "after").unwrap_err();
    assert!(matches!(rename_error, core::CheckPoError::Corruption(_)));
    assert_eq!(fs::read(&names_path).unwrap(), corrupt_bytes);

    let delete_error =
        core::delete_checkpoint(&project, created.checkpoint_id.as_str()).unwrap_err();
    assert!(matches!(delete_error, core::CheckPoError::Corruption(_)));
    assert_eq!(fs::read(&names_path).unwrap(), corrupt_bytes);
    assert!(core::snapshot_path(&repo, &created.checkpoint_id).is_file());

    let checkpoints = core::list_checkpoints(&project).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].name, "before");
    assert!(!checkpoints[0].warnings.is_empty());
}

#[test]
fn negative_sqlite_logical_sizes_are_rejected_instead_of_wrapping() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let conn = open_test_index(&repo);
    conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    conn.execute("UPDATE snapshots SET logical_size_bytes = -1", [])
        .unwrap();

    let error = core::storage_summary(&project).unwrap_err();
    assert!(matches!(error, core::CheckPoError::IndexUnavailable(_)));
}

#[test]
fn invalid_index_row_is_reported_as_index_unavailable() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let conn = open_test_index(&repo);
    conn.execute("UPDATE snapshots SET snapshot_id = 'invalid'", [])
        .unwrap();

    let error = core::list_checkpoints(&project).unwrap_err();
    assert!(matches!(error, core::CheckPoError::IndexUnavailable(_)));
}

#[test]
fn unreadable_sqlite_index_is_reported_without_snapshot_fallback() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let db_path = core::db_path(&repo).unwrap();
    fs::remove_file(&db_path).unwrap();
    fs::create_dir_all(&db_path).unwrap();

    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Corrupt
    );
    assert!(matches!(
        core::list_checkpoints_with_warnings_for_project(&context),
        Err(core::CheckPoError::IndexUnavailable(_))
    ));
    assert!(db_path.is_dir());
}

#[cfg(unix)]
#[test]
fn snapshot_index_symlink_is_corrupt_and_rebuild_never_follows_it() {
    use std::os::unix::fs::symlink;

    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let db_path = core::db_path(&repo).unwrap();
    let outside = temp.path().join("outside.db");
    fs::write(&outside, b"outside-must-not-change").unwrap();
    let before = fs::read(&outside).unwrap();
    fs::remove_file(&db_path).unwrap();
    symlink(&outside, &db_path).unwrap();

    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Corrupt
    );
    assert!(core::rebuild_index(&project).is_err());
    assert_eq!(fs::read(&outside).unwrap(), before);
    assert!(fs::symlink_metadata(&db_path)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn corrupt_snapshot_inventory_head_marks_the_index_corrupt() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let context = core::load_project(&project).unwrap();
    let repo = repo_path(&view);
    fs::write(repo.join("inventory/snapshots/head"), "not-a-digest\n").unwrap();

    let status = core::checkpoint_index_status(&context).unwrap();

    assert_eq!(status.state, core::CheckpointIndexState::Corrupt);
    assert!(!status.rebuildable);
    assert!(status
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("inventory")));
}

#[test]
fn cancelled_rebuild_index_does_not_remove_existing_index_db() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let index_db = core::db_path(&repo).unwrap();
    core::rebuild_index(&project).unwrap();
    assert!(index_db.is_file());
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    let before = fs::read(&index_db).unwrap();

    let token = core::CancellationToken::new();
    let cancel_from_progress = token.clone();
    let progress = move |event: core::OperationProgress| {
        if event.phase == "readingSnapshots" && event.completed >= 1 {
            cancel_from_progress.cancel();
        }
    };
    let context = core::load_project(&project).unwrap();
    let error = core::rebuild_index_for_project_with_progress_and_cancellation(
        &context,
        Some(&progress),
        Some(&token),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
    assert!(index_db.is_file());
    assert_eq!(fs::read(&index_db).unwrap(), before);
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
}

#[test]
fn rebuild_index_streams_a_low_sharing_manifest_graph_with_exact_reference_counts() {
    let (_guard, _temp, project, _data) = setup();
    const FILE_COUNT: usize = 256;
    const CHECKPOINT_COUNT: usize = 4;
    for file_index in 0..FILE_COUNT {
        fs::write(
            project.join(format!("Assets/Avatar/File{file_index:04}.asset")),
            format!("checkpoint-0-file-{file_index}"),
        )
        .unwrap();
    }
    let view = init_project_for_test(&project).unwrap();
    for checkpoint_index in 0..CHECKPOINT_COUNT {
        if checkpoint_index > 0 {
            for file_index in 0..FILE_COUNT {
                fs::write(
                    project.join(format!("Assets/Avatar/File{file_index:04}.asset")),
                    format!("checkpoint-{checkpoint_index}-file-{file_index}"),
                )
                .unwrap();
            }
        }
        core::create_checkpoint(
            &project,
            &format!("checkpoint-{checkpoint_index}"),
            Default::default(),
        )
        .unwrap();
    }

    let rebuilt = core::rebuild_index(&project).unwrap();

    assert_eq!(rebuilt.snapshot_count, CHECKPOINT_COUNT);
    let repo = repo_path(&view);
    let conn = open_test_index(&repo);
    let (unique_objects, total_references, singly_referenced): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), SUM(reference_count),
                    SUM(CASE WHEN reference_count = 1 THEN 1 ELSE 0 END)
             FROM object_refs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(unique_objects, (FILE_COUNT * CHECKPOINT_COUNT + 1) as i64);
    assert_eq!(
        total_references,
        ((FILE_COUNT + 1) * CHECKPOINT_COUNT) as i64
    );
    assert_eq!(singly_referenced, (FILE_COUNT * CHECKPOINT_COUNT) as i64);
}

#[test]
fn rebuilding_snapshot_index_does_not_replace_fingerprint_cache() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let cache_path = core::file_fingerprint_db_path(&repo).unwrap();
    assert!(cache_path.is_file());
    assert_eq!(fingerprint_count(&repo, "Assets/Avatar/Foo.prefab"), 1);
    let before = fs::read(&cache_path).unwrap();

    core::rebuild_index(&project).unwrap();

    assert_eq!(fs::read(&cache_path).unwrap(), before);
    assert_eq!(fingerprint_count(&repo, "Assets/Avatar/Foo.prefab"), 1);
}

#[test]
fn corrupt_snapshot_rebuild_preserves_existing_live_index() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    core::create_checkpoint(&project, "two", Default::default()).unwrap();
    let repo = repo_path(&view);
    let index_path = core::db_path(&repo).unwrap();
    let before = fs::read(&index_path).unwrap();
    fs::write(
        core::snapshot_path(&repo, &first.checkpoint_id),
        "corrupt root",
    )
    .unwrap();

    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Current
    );
    assert!(core::rebuild_index(&project).is_err());
    assert_eq!(fs::read(&index_path).unwrap(), before);
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Current
    );
}

#[test]
fn snapshot_index_status_is_head_only_and_full_verify_detects_root_change() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    core::rebuild_index(&project).unwrap();
    let repo = repo_path(&view);
    let snapshot_path = core::snapshot_path(&repo, &checkpoint.checkpoint_id);
    let original = fs::read(&snapshot_path).unwrap();
    let original_mtime =
        filetime::FileTime::from_last_modification_time(&fs::metadata(&snapshot_path).unwrap());
    let mut changed = original.clone();
    let name_offset = changed
        .windows(3)
        .position(|window| window == b"one")
        .expect("checkpoint name is encoded in the root");
    changed[name_offset..name_offset + 3].copy_from_slice(b"two");
    assert_eq!(changed.len(), original.len());
    assert_ne!(changed, original);
    fs::write(&snapshot_path, changed).unwrap();
    filetime::set_file_mtime(&snapshot_path, original_mtime).unwrap();

    let context = core::load_project(&project).unwrap();
    let status = core::checkpoint_index_status(&context).unwrap();

    assert_eq!(status.state, core::CheckpointIndexState::Current);
    let verification = core::verify_project(&project, true).unwrap();
    assert!(!verification.is_valid);
    assert!(verification
        .errors
        .iter()
        .any(|error| error.contains("digest mismatch")));
}

#[test]
fn object_size_mismatch_does_not_block_checkpoint_index_rebuild() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::write(&object, "wrong-size").unwrap();

    let rebuilt = core::rebuild_index(&project).unwrap();
    assert!(rebuilt.referenced_object_count >= 1);
    assert_eq!(rebuilt.unavailable_referenced_object_count, 1);
    assert_eq!(rebuilt.errors.len(), 1);
    assert!(rebuilt.errors[0].contains("full verify"));
    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Current
    );
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    let conn = open_test_index(&repo);
    let present_size: Option<i64> = conn
        .query_row(
            "SELECT present_size_bytes FROM object_refs WHERE object_id = ?1",
            [snapshot.files[0].content_hash().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(present_size, None);
}

#[test]
fn missing_cached_object_is_rehashed_and_restored() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::remove_file(&object).unwrap();

    core::create_checkpoint(&project, "two", Default::default()).unwrap();

    assert!(object.is_file());
}

#[test]
fn missing_cached_object_shard_is_recreated() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    let shard = object.parent().unwrap();
    fs::remove_dir_all(shard).unwrap();

    core::create_checkpoint(&project, "two", Default::default()).unwrap();

    assert!(object.is_file());
}

#[test]
fn checkpoint_scan_skips_checkpo_temporary_files() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    fs::write(
        project.join("Assets/Avatar/.checkpo-1234567890abcdef1234567890abcdef.tmp"),
        "temp",
    )
    .unwrap();
    fs::write(
        project
            .join("Assets/Avatar/.checkpo-r-1234567890abcdef-1234567890abcdef1234567890abcdef.tmp"),
        "temp",
    )
    .unwrap();
    fs::write(
        project.join("Assets/Avatar/.checkpo-Foo.prefab-1234567890abcdef1234567890abcdef.tmp"),
        "user",
    )
    .unwrap();
    fs::write(
        project.join("Assets/Avatar/.Foo.prefab.1234567890abcdef1234567890abcdef.tmp"),
        "user",
    )
    .unwrap();

    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let diff = core::diff_checkpoint(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert_eq!(checkpoint.file_count, 4);
    assert!(checkpoint
        .warnings
        .iter()
        .any(|warning| warning.contains(".checkpo-1234567890abcdef")));
    assert!(checkpoint
        .warnings
        .iter()
        .any(|warning| warning.contains(".checkpo-r-1234567890abcdef")));
    assert!(diff.added.is_empty());
    assert_eq!(diff.unchanged_count, 4);
}

#[test]
fn full_diff_and_restore_plan_report_scan_warnings_for_temporary_files() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    fs::write(
        project
            .join("Assets/Avatar/.checkpo-r-1234567890abcdef-1234567890abcdef1234567890abcdef.tmp"),
        "temp",
    )
    .unwrap();

    let diff = core::diff_checkpoint(&project, checkpoint.checkpoint_id.as_str()).unwrap();
    let plan = core::preview_restore(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert!(diff
        .warnings
        .iter()
        .any(|warning| warning.contains("temporary CheckPo file was skipped")));
    assert!(plan
        .warnings
        .iter()
        .any(|warning| warning.contains("temporary CheckPo file was skipped")));
    let error = core::apply_restore_plan(
        &project,
        checkpoint.checkpoint_id.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();
    assert!(error.to_string().contains("scan warnings"));
}

#[test]
fn orphan_checkpo_temporary_files_can_be_cleaned_up_explicitly() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let temp_file = project
        .join("Assets/Avatar/.checkpo-r-1234567890abcdef-1234567890abcdef1234567890abcdef.tmp");
    fs::write(&temp_file, "temp").unwrap();

    let plan = core::analyze_orphan_temp_files(&project).unwrap();
    assert_eq!(plan.file_count, 1);
    assert_eq!(plan.total_bytes, 4);

    let result = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(result.deleted_file_count, 1);
    assert_eq!(result.deleted_bytes, 4);
    assert!(!temp_file.exists());
}

#[test]
fn temporary_cleanup_expected_plan_rejects_new_candidate_without_deleting_previewed_files() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let first = project.join("Assets/.checkpo-11111111111111111111111111111111.tmp");
    let second = project.join("Assets/.checkpo-22222222222222222222222222222222.tmp");
    fs::write(&first, "first").unwrap();
    let plan = core::analyze_orphan_temp_files(&project).unwrap();
    fs::write(&second, "second").unwrap();

    let error = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert!(first.is_file());
    assert!(second.is_file());
}

#[test]
fn temporary_cleanup_expected_plan_rejects_same_size_replacement() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let temporary = project.join("Assets/.checkpo-33333333333333333333333333333333.tmp");
    fs::write(&temporary, "first").unwrap();
    let plan = core::analyze_orphan_temp_files(&project).unwrap();
    fs::remove_file(&temporary).unwrap();
    fs::write(&temporary, "other").unwrap();

    let error = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(temporary).unwrap(), "other");
}

#[test]
fn temporary_cleanup_expected_plan_is_bound_to_checkpoint_inventory_head() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let temporary = project.join("Assets/.checkpo-44444444444444444444444444444444.tmp");
    fs::write(&temporary, "temporary").unwrap();
    let plan = core::analyze_orphan_temp_files(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "checkpoint").unwrap();
    core::create_checkpoint(&project, "changes inventory", Default::default()).unwrap();

    let error = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert!(temporary.is_file());
}

#[test]
fn repository_object_temporary_files_are_included_in_cleanup() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo_tmp = repo_path(&view).join("tmp");
    let project_temp = project.join("Assets/Avatar/.checkpo-1234567890abcdef1234567890abcdef.tmp");
    let repository_temp = repo_tmp.join("object-0123456789abcdef0123456789abcdef.tmp");
    let uppercase_name = repo_tmp.join("object-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.tmp");
    let invalid_hex = repo_tmp.join("object-gggggggggggggggggggggggggggggggg.tmp");
    let matching_directory = repo_tmp.join("object-11111111111111111111111111111111.tmp");
    fs::write(&project_temp, "temp").unwrap();
    fs::write(&repository_temp, "repository").unwrap();
    fs::write(&uppercase_name, "uppercase").unwrap();
    fs::write(&invalid_hex, "invalid").unwrap();
    fs::create_dir(&matching_directory).unwrap();

    let plan = core::analyze_orphan_temp_files(&project).unwrap();

    assert_eq!(plan.file_count, 2);
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.repository_files.len(), 1);
    assert_eq!(plan.files[0].size_bytes, 4);
    assert_eq!(plan.repository_files[0].size_bytes, 10);
    assert_eq!(plan.total_bytes, 14);
    assert_eq!(
        plan.repository_files[0].file_name,
        "object-0123456789abcdef0123456789abcdef.tmp"
    );

    let result = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(result.deleted_file_count, 2);
    assert_eq!(result.deleted_bytes, 14);
    assert!(!project_temp.exists());
    assert!(!repository_temp.exists());
    assert!(uppercase_name.exists());
    assert!(invalid_hex.exists());
    assert!(matching_directory.is_dir());
}

#[cfg(unix)]
#[test]
fn repository_temporary_cleanup_does_not_follow_symlinks() {
    let (_guard, temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo_tmp = repo_path(&view).join("tmp");
    let outside = temp.path().join("outside-object-temp");
    let linked_temp = repo_tmp.join("object-0123456789abcdef0123456789abcdef.tmp");
    fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, &linked_temp).unwrap();

    let plan = core::analyze_orphan_temp_files(&project).unwrap();
    assert!(plan.repository_files.is_empty());

    let result = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(result.deleted_file_count, 0);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    assert!(fs::symlink_metadata(&linked_temp)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn project_temporary_cleanup_does_not_follow_symlinks_or_descend_into_them() {
    let (_guard, temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let outside_file = temp.path().join("outside-temp");
    let linked_temp = project.join("Assets/Avatar/.checkpo-1234567890abcdef1234567890abcdef.tmp");
    fs::write(&outside_file, "outside").unwrap();
    std::os::unix::fs::symlink(&outside_file, &linked_temp).unwrap();

    let checkpoint_error =
        core::create_checkpoint(&project, "unsafe", Default::default()).unwrap_err();
    assert!(checkpoint_error
        .to_string()
        .contains("symbolic links and reparse points"));

    let outside_dir = temp.path().join("outside-directory");
    fs::create_dir(&outside_dir).unwrap();
    let outside_nested_temp = outside_dir.join(".checkpo-abcdef0123456789abcdef0123456789.tmp");
    fs::write(&outside_nested_temp, "nested").unwrap();
    std::os::unix::fs::symlink(&outside_dir, project.join("Assets/LinkedOutside")).unwrap();

    let plan = core::analyze_orphan_temp_files(&project).unwrap();
    assert!(plan.files.is_empty());
    assert!(plan
        .warnings
        .iter()
        .any(|warning| warning.contains("symbolic links and reparse points")));

    let result = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(result.deleted_file_count, 0);
    assert_eq!(fs::read_to_string(&outside_file).unwrap(), "outside");
    assert_eq!(fs::read_to_string(&outside_nested_temp).unwrap(), "nested");
    assert!(fs::symlink_metadata(&linked_temp)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn temporary_cleanup_only_deletes_checkpo_owned_temporary_files() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let generic_copy_temp =
        project.join("Assets/Avatar/.Foo.prefab.1234567890abcdef1234567890abcdef.tmp");
    let prefixed_checkpo_temp =
        project.join("Assets/Avatar/.checkpo-Foo.prefab-1234567890abcdef1234567890abcdef.tmp");
    let user_temp = project.join("Assets/Avatar/.checkpo-notes.tmp");
    let atomic_temp = project.join("Assets/Avatar/.checkpo-1234567890abcdef1234567890abcdef.tmp");
    let recovery_temp = project
        .join("Assets/Avatar/.checkpo-r-1234567890abcdef-1234567890abcdef1234567890abcdef.tmp");
    fs::write(&generic_copy_temp, "copy").unwrap();
    fs::write(&prefixed_checkpo_temp, "prefixed").unwrap();
    fs::write(&user_temp, "user").unwrap();
    fs::write(&atomic_temp, "atomic").unwrap();
    fs::write(&recovery_temp, "recovery").unwrap();

    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let plan = core::analyze_orphan_temp_files(&project).unwrap();

    assert!(checkpoint
        .warnings
        .iter()
        .any(|warning| warning.contains("temporary CheckPo file was skipped")));
    assert_eq!(plan.file_count, 2);
    assert_eq!(plan.total_bytes, 14);
    let result = core::cleanup_orphan_temp_files_with_expected_plan(
        &project,
        &plan.plan_id,
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert_eq!(result.deleted_file_count, 2);
    assert_eq!(result.deleted_bytes, 14);
    assert!(generic_copy_temp.exists());
    assert!(prefixed_checkpo_temp.exists());
    assert!(user_temp.exists());
    assert!(!atomic_temp.exists());
    assert!(!recovery_temp.exists());
    let snapshot = core::load_snapshot(
        &repo_path(&core::load_project_view(&project).unwrap()),
        &checkpoint.checkpoint_id,
    )
    .unwrap();
    assert!(snapshot
        .files
        .iter()
        .any(|file| file.path.as_str() == "Assets/Avatar/.checkpo-notes.tmp"));
    assert!(snapshot.files.iter().any(|file| file.path.as_str()
        == "Assets/Avatar/.Foo.prefab.1234567890abcdef1234567890abcdef.tmp"));
    assert!(snapshot.files.iter().any(|file| file.path.as_str()
        == "Assets/Avatar/.checkpo-Foo.prefab-1234567890abcdef1234567890abcdef.tmp"));
}

#[test]
fn same_size_same_mtime_change_is_not_hidden_by_fingerprint_cache() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, "two").unwrap();
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_mtime)).unwrap();

    let second = core::create_checkpoint(&project, "two", Default::default()).unwrap();

    let repo = repo_path(&view);
    let first_snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let second_snapshot = core::load_snapshot(&repo, &second.checkpoint_id).unwrap();
    assert_ne!(
        first_snapshot.files[0].content_hash(),
        second_snapshot.files[0].content_hash()
    );
}

#[test]
fn metadata_diff_can_miss_same_size_same_mtime_change() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, "two").unwrap();
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_mtime)).unwrap();

    let diff = core::diff_checkpoint_metadata(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert!(!diff
        .modified
        .contains(&"Assets/Avatar/Foo.prefab".to_string()));
    assert_eq!(diff.unchanged_count, 2);
}

#[test]
fn full_diff_detects_same_size_same_mtime_change_with_fingerprint_cache() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, "two").unwrap();
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_mtime)).unwrap();

    let diff = core::diff_checkpoint(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert!(diff
        .modified
        .contains(&"Assets/Avatar/Foo.prefab".to_string()));
}

#[test]
fn metadata_diff_detects_added_deleted_and_metadata_changed_files() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    let deleted_file = project.join("Packages/locked.json");
    fs::write(&file, "one").unwrap();
    fs::write(&deleted_file, "{}").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();

    fs::write(project.join("Assets/Avatar/Added.prefab"), "added").unwrap();
    fs::write(&file, "changed size").unwrap();
    fs::remove_file(&deleted_file).unwrap();

    let diff = core::diff_checkpoint_metadata(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert!(diff
        .added
        .contains(&"Assets/Avatar/Added.prefab".to_string()));
    assert!(diff
        .modified
        .contains(&"Assets/Avatar/Foo.prefab".to_string()));
    assert!(diff.deleted.contains(&"Packages/locked.json".to_string()));
    assert!(diff.unknown.is_empty());
    assert!(diff.complete);
    assert!(diff.warnings.is_empty());
}

#[test]
fn metadata_diff_honors_pre_cancelled_token() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let cancellation = core::CancellationToken::new();
    cancellation.cancel();

    let error = core::diff_checkpoint_metadata_with_cancellation(
        &project,
        checkpoint.checkpoint_id.as_str(),
        Some(&cancellation),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
}

#[test]
fn metadata_diff_warns_when_tracked_root_is_not_directory() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    fs::write(project.join("Packages/locked.json"), "{}").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::remove_file(project.join("Packages/locked.json")).unwrap();
    fs::remove_dir(project.join("Packages")).unwrap();
    fs::write(project.join("Packages"), "not a directory").unwrap();

    let diff = core::diff_checkpoint_metadata(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert!(diff
        .warnings
        .iter()
        .any(|warning| warning.contains("Packages: tracked root is not a directory")));
    assert!(!diff.complete);
    assert!(!diff.deleted.contains(&"Packages/locked.json".to_string()));
    assert!(diff.unknown.contains(&"Packages/locked.json".to_string()));
}

#[test]
fn tampered_snapshot_path_cannot_reach_restore_operation() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "safe-object").unwrap();
    fs::write(project.join("README.md"), "keep-readme").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = repo_path(&view);
    let leaf = first_regular_file_below(&repo.join("manifests/v2/leaves"));
    let mut tampered = fs::read(&leaf).unwrap();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    fs::write(&leaf, tampered).unwrap();

    let error = core::preview_restore(&project, checkpoint.as_str()).unwrap_err();
    assert!(
        error.to_string().contains("tracked path") || error.to_string().contains("digest mismatch")
    );
    assert_eq!(
        fs::read_to_string(project.join("README.md")).unwrap(),
        "keep-readme"
    );
}

#[test]
fn tampered_snapshot_object_id_is_reported_without_panic() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "safe-object").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let _checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = repo_path(&view);
    let leaf = first_regular_file_below(&repo.join("manifests/v2/leaves"));
    let mut tampered = fs::read(&leaf).unwrap();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    fs::write(&leaf, tampered).unwrap();

    let result = core::verify_project(&project, false).unwrap();
    assert!(!result.is_valid);
    assert!(result
        .errors
        .iter()
        .any(|error| error.contains("digest mismatch")));
}

#[test]
fn snapshot_validation_rejects_duplicate_paths_invalid_roots_and_times() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let entry = SnapshotEntry {
        path: TrackedUnityFilePath::parse("Assets/Avatar/Foo.prefab").unwrap(),
        size_bytes: 3,
        modified_at_utc: "2026-01-01T00:00:01Z".to_string(),
        content: SnapshotContent::Whole {
            hash: core::ObjectId::parse(&"0".repeat(64)).unwrap(),
            size_bytes: 3,
        },
    };
    let repo = repo_path(&view);
    let duplicate = SnapshotFile {
        schema_version: 2,
        project_id: core::ProjectId::parse(&view.project_id).unwrap(),
        parent_snapshot_id: None,
        created_at_utc: "2026-01-01T00:00:00Z".to_string(),
        name: "duplicate".to_string(),
        tool_version: "0.1.0".to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files: vec![entry.clone(), entry.clone()],
    };
    let duplicate_error = core::__debug_test_save_snapshot(&repo, &duplicate).unwrap_err();
    assert!(duplicate_error.to_string().contains("duplicate"));

    let mut invalid_roots = duplicate;
    invalid_roots.files = vec![entry.clone()];
    invalid_roots.tracked_roots = vec!["Assets".to_string()];
    let invalid_roots_error = core::__debug_test_save_snapshot(&repo, &invalid_roots).unwrap_err();
    assert!(invalid_roots_error.to_string().contains("tracked roots"));

    let mut invalid_time = invalid_roots;
    invalid_time.tracked_roots = vec![
        "Assets".to_string(),
        "Packages".to_string(),
        "ProjectSettings".to_string(),
    ];
    invalid_time.files[0].modified_at_utc = "not-a-time".to_string();
    let invalid_time_error = core::__debug_test_save_snapshot(&repo, &invalid_time).unwrap_err();
    assert!(invalid_time_error.to_string().contains("modifiedAtUtc"));
}

#[test]
fn snapshot_load_rejects_trailing_bytes_even_when_root_id_matches() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let original_path = core::snapshot_path(&repo, &checkpoint.checkpoint_id);
    let mut noncanonical = fs::read(&original_path).unwrap();
    noncanonical.push(0);
    let noncanonical_id = publish_raw_snapshot_root(&repo, &noncanonical);

    let error = core::load_snapshot(&repo, &noncanonical_id).unwrap_err();

    assert!(error.to_string().contains("payload length mismatch"));
}

#[test]
fn snapshot_load_rejects_unknown_flags_even_when_root_id_matches() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let original_path = core::snapshot_path(&repo, &checkpoint.checkpoint_id);
    let mut with_unknown = fs::read(&original_path).unwrap();
    with_unknown[10] = 1;
    let unknown_id = publish_raw_snapshot_root(&repo, &with_unknown);

    let error = core::load_snapshot(&repo, &unknown_id).unwrap_err();

    assert!(error.to_string().contains("unsupported envelope flags"));
}

#[test]
fn whitespace_checkpoint_name_is_rejected_by_load_index_and_gc() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint = core::create_checkpoint(&project, "good", Default::default()).unwrap();
    let repo = repo_path(&view);
    let mut bytes = fs::read(core::snapshot_path(&repo, &checkpoint.checkpoint_id)).unwrap();
    let offset = bytes
        .windows(b"good".len())
        .position(|window| window == b"good")
        .expect("checkpoint name is encoded in the root");
    bytes[offset..offset + 4].copy_from_slice(b"    ");
    let invalid_id = publish_raw_snapshot_root(&repo, &bytes);

    assert!(core::load_snapshot(&repo, &invalid_id)
        .unwrap_err()
        .to_string()
        .contains("checkpoint name"));
    let rebuild_error = core::rebuild_index(&project).unwrap_err().to_string();
    assert!(
        rebuild_error.contains("inventory") || rebuild_error.contains("physical snapshot roots"),
        "unexpected rebuild error: {rebuild_error}"
    );
    let gc_error = core::analyze_gc(&project).unwrap_err().to_string();
    assert!(
        gc_error.contains("inventory") || gc_error.contains("physical snapshot roots"),
        "unexpected gc error: {gc_error}"
    );
}

#[test]
fn diff_and_restore_preview_use_the_requested_snapshot_when_latest_is_corrupt() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    fs::write(&file, "two").unwrap();
    let latest = core::create_checkpoint(&project, "Latest", Default::default()).unwrap();
    let repo = repo_path(&view);
    let latest_path = core::snapshot_path(&repo, &latest.checkpoint_id);
    let mut corrupt = fs::read(&latest_path).unwrap();
    corrupt.push(0);
    fs::write(&latest_path, corrupt).unwrap();

    let diff = core::diff_checkpoint(&project, first.checkpoint_id.as_str()).unwrap();
    let plan = core::preview_restore(&project, first.checkpoint_id.as_str()).unwrap();

    assert_eq!(diff.modified, vec!["Assets/Avatar/Foo.prefab"]);
    assert_eq!(plan.replace_count, 1);
    assert_eq!(plan.operations[0].path.as_str(), "Assets/Avatar/Foo.prefab");
}

#[test]
fn checkpoint_creation_is_blocked_when_latest_snapshot_is_corrupt() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    let repo = repo_path(&view);
    let first_path = core::snapshot_path(&repo, &first.checkpoint_id);
    let mut corrupt = fs::read(&first_path).unwrap();
    corrupt.push(0);
    fs::write(&first_path, corrupt).unwrap();
    fs::write(&file, "two").unwrap();

    let error = core::create_checkpoint(&project, "Second", Default::default()).unwrap_err();

    assert!(error.to_string().contains("digest mismatch"));
    assert!(first_path.is_file());
    assert_eq!(
        fs::read_to_string(repo.join("refs/latest")).unwrap(),
        first.checkpoint_id.to_string()
    );
}

#[test]
fn checkpoint_creation_is_blocked_when_latest_ref_is_malformed() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "First", Default::default()).unwrap();
    let repo = repo_path(&view);
    fs::write(repo.join("refs/latest"), "not-a-snapshot-id").unwrap();

    let error = core::create_checkpoint(&project, "Second", Default::default()).unwrap_err();

    assert!(matches!(error, core::CheckPoError::InvalidId(_)));
    assert_eq!(
        fs::read_to_string(repo.join("refs/latest")).unwrap(),
        "not-a-snapshot-id"
    );
}

#[test]
fn checkpoint_creation_is_blocked_when_latest_snapshot_is_missing() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    let repo = repo_path(&view);
    fs::remove_file(core::snapshot_path(&repo, &first.checkpoint_id)).unwrap();

    let error = core::create_checkpoint(&project, "Second", Default::default()).unwrap_err();

    assert!(matches!(error, core::CheckPoError::SnapshotNotFound(_)));
    assert_eq!(
        fs::read_to_string(repo.join("refs/latest")).unwrap(),
        first.checkpoint_id.to_string()
    );
}

#[test]
fn future_latest_root_payload_schema_blocks_checkpoint_creation() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    let repo = repo_path(&view);
    let first_path = core::snapshot_path(&repo, &first.checkpoint_id);
    let mut future = fs::read(first_path).unwrap();
    future[16..18].copy_from_slice(&2_u16.to_be_bytes());
    let future_id = publish_raw_snapshot_root(&repo, &future);
    fs::write(repo.join("refs/latest"), future_id.as_str()).unwrap();
    let snapshot_count_before = walkdir::WalkDir::new(repo.join("snapshots/v2"))
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();

    let error = core::create_checkpoint(&project, "Blocked", Default::default()).unwrap_err();

    assert!(error.to_string().contains("unsupported payload schema 2"));
    assert_eq!(
        fs::read_to_string(repo.join("refs/latest")).unwrap(),
        future_id.to_string()
    );
    assert_eq!(
        walkdir::WalkDir::new(repo.join("snapshots/v2"))
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count(),
        snapshot_count_before
    );
}

#[test]
fn external_future_root_keeps_head_status_current_and_blocks_gc_deletion() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    let repo = repo_path(&view);
    let first_path = core::snapshot_path(&repo, &first.checkpoint_id);
    let mut future = fs::read(first_path).unwrap();
    future[16..18].copy_from_slice(&2_u16.to_be_bytes());
    let _future_id = publish_raw_snapshot_root(&repo, &future);

    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Current
    );
    assert_eq!(
        core::list_checkpoints_with_warnings_for_project(&context)
            .unwrap()
            .checkpoints
            .len(),
        1
    );
    let gc_error = core::analyze_gc(&project).unwrap_err().to_string();
    assert!(
        gc_error.contains("inventory") || gc_error.contains("physical snapshot roots"),
        "unexpected gc error: {gc_error}"
    );
    assert!(core::apply_gc_with_expected_plan(&project, &"0".repeat(64)).is_err());
}

#[test]
fn direct_checkpoint_operations_reject_foreign_project_snapshot() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let mut foreign = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    foreign.project_id = core::ProjectId::parse("00000000000000000000000000000000").unwrap();
    let foreign_id = core::__debug_test_save_snapshot(&repo, &foreign).unwrap();

    let project_verification = core::verify_project(&project, false).unwrap();
    assert!(!project_verification.is_valid);
    assert!(project_verification
        .errors
        .iter()
        .any(|error| error.contains("project id does not match current project")));
    let checkpoint_verification =
        core::verify_checkpoint(&project, foreign_id.as_str(), false).unwrap();
    assert!(!checkpoint_verification.is_valid);
    assert!(checkpoint_verification
        .errors
        .iter()
        .any(|error| error.contains("project id does not match current project")));

    let error = core::diff_checkpoint(&project, foreign_id.as_str()).unwrap_err();

    assert!(error.to_string().contains("project id does not match"));
}

#[test]
fn checkpoint_list_ignores_invalid_root_filename_but_verify_warns() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let snapshots = view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("snapshots/v2");
    fs::write(snapshots.join("README.root"), "invalid").unwrap();
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    let verify = core::verify_project(&project, false).unwrap();
    assert!(verify.is_valid);
    assert!(verify
        .warnings
        .iter()
        .any(|warning| warning.contains("README.root")));
}

#[test]
fn discard_rejects_untracked_paths_and_allows_tracked_file() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    for path in [
        "README.md",
        ".git/config",
        "UserSettings/foo",
        "Assets",
        "Packages",
        "ProjectSettings",
    ] {
        let error = core::preview_discard_files(&project, &[path.to_string()], None).unwrap_err();
        assert!(error.to_string().contains("tracked path") || error.to_string().contains("scope"));
    }

    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    let plan =
        core::preview_discard_files(&project, &["Assets/Avatar/Foo.prefab".to_string()], None)
            .unwrap();
    assert_eq!(plan.kind, OperationPlanKind::Discard);
    assert_eq!(plan.replace_count, 1);
}

#[test]
fn operation_plan_estimates_temporary_staged_and_backup_bytes() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    fs::write(project.join("Assets/Avatar/Delete.prefab"), "delete").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "three").unwrap();
    fs::remove_file(project.join("Assets/Avatar/Delete.prefab")).unwrap();
    fs::write(project.join("Assets/Avatar/Extra.prefab"), "extra").unwrap();

    let restore_plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert_eq!(restore_plan.restore_count, 1);
    assert_eq!(restore_plan.replace_count, 1);
    assert_eq!(restore_plan.delete_count, 1);
    assert_eq!(restore_plan.staged_bytes, 9);
    assert_eq!(restore_plan.backup_bytes, 10);
    assert_eq!(restore_plan.estimated_temporary_bytes, 19);

    let discard_plan = core::preview_discard_files(
        &project,
        &[
            "Assets/Avatar/Foo.prefab".to_string(),
            "Assets/Avatar/Extra.prefab".to_string(),
        ],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    assert_eq!(discard_plan.restore_count, 0);
    assert_eq!(discard_plan.replace_count, 1);
    assert_eq!(discard_plan.delete_count, 1);
    assert_eq!(discard_plan.staged_bytes, 3);
    assert_eq!(discard_plan.backup_bytes, 10);
    assert_eq!(discard_plan.estimated_temporary_bytes, 13);
}

#[test]
fn restore_and_discard_never_touch_untracked_files() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    fs::write(project.join("README.md"), "keep").unwrap();
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::write(project.join(".git/config"), "keep").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    fs::write(project.join("Assets/Avatar/New.prefab"), "new").unwrap();
    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert!(plan
        .operations
        .iter()
        .any(
            |operation| operation.operation_type == FileOperationType::Delete
                && operation.path.as_str() == "Assets/Avatar/New.prefab"
        ));
    assert!(plan
        .operations
        .iter()
        .all(|operation| !operation.path.as_str().contains("README")));
    let expected_plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    core::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        expected_plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(project.join("README.md")).unwrap(),
        "keep"
    );
    assert_eq!(
        fs::read_to_string(project.join(".git/config")).unwrap(),
        "keep"
    );
    assert!(!project.join("Assets/Avatar/New.prefab").exists());
}

#[test]
fn restore_restores_deleted_tracked_file_and_mtime() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let original_time = SystemTime::now() - Duration::from_secs(7200);
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_time)).unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_file(&file).unwrap();

    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert_eq!(plan.restore_count, 1);
    core::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    let restored = fs::metadata(&file).unwrap().modified().unwrap();
    assert!(restored.duration_since(original_time).unwrap_or_default() < Duration::from_secs(2));
}

#[test]
fn restore_replaces_directory_tree_with_snapshot_file() {
    let (_guard, _temp, project, _data) = setup();
    let target = project.join("Assets/Topology");
    fs::write(&target, "snapshot-file").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "File", Default::default())
        .unwrap()
        .checkpoint_id;

    fs::remove_file(&target).unwrap();
    fs::create_dir_all(target.join("Nested/Empty")).unwrap();
    fs::write(target.join("Nested/current.asset"), "current").unwrap();

    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert!(plan
        .directories_to_remove
        .iter()
        .any(|path| path.as_str() == "Assets/Topology"));
    core::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert!(target.is_file());
    assert_eq!(fs::read_to_string(target).unwrap(), "snapshot-file");
}

#[test]
fn restore_replaces_blocking_file_with_snapshot_directory_tree() {
    let (_guard, _temp, project, _data) = setup();
    let target = project.join("Assets/Topology");
    fs::create_dir_all(target.join("Nested")).unwrap();
    fs::write(target.join("Nested/snapshot.asset"), "snapshot-tree").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Tree", Default::default())
        .unwrap()
        .checkpoint_id;

    fs::remove_dir_all(&target).unwrap();
    fs::write(&target, "blocking-file").unwrap();

    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert!(plan
        .directories_to_create
        .iter()
        .any(|path| path.as_str() == "Assets/Topology"));
    core::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert!(target.is_dir());
    assert_eq!(
        fs::read_to_string(target.join("Nested/snapshot.asset")).unwrap(),
        "snapshot-tree"
    );
}

#[test]
fn discard_expands_directory_blocker_and_restores_snapshot_file() {
    let (_guard, _temp, project, _data) = setup();
    let target = project.join("Assets/Topology");
    fs::write(&target, "snapshot-file").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "File", Default::default())
        .unwrap()
        .checkpoint_id;

    fs::remove_file(&target).unwrap();
    fs::create_dir_all(target.join("Nested/Empty")).unwrap();
    fs::write(target.join("Nested/current.asset"), "current").unwrap();
    let requested = vec!["Assets/Topology".to_string()];

    let plan =
        core::preview_discard_files(&project, &requested, Some(checkpoint.as_str())).unwrap();
    assert_eq!(
        plan.selected_paths.as_deref(),
        Some(
            ["Assets/Topology", "Assets/Topology/Nested/current.asset"]
                .map(|path| TrackedUnityFilePath::parse(path).unwrap())
                .as_slice()
        )
    );
    assert!(plan
        .directories_to_remove
        .iter()
        .any(|path| path.as_str() == "Assets/Topology"));

    core::apply_discard_files_plan(
        &project,
        &requested,
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert!(target.is_file());
    assert_eq!(fs::read_to_string(target).unwrap(), "snapshot-file");
}

#[test]
fn discard_expands_file_blocker_without_restoring_unselected_snapshot_sibling() {
    let (_guard, _temp, project, _data) = setup();
    let target = project.join("Assets/Topology");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("A.asset"), "snapshot-a").unwrap();
    fs::write(target.join("B.asset"), "snapshot-b").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Tree", Default::default())
        .unwrap()
        .checkpoint_id;

    fs::remove_dir_all(&target).unwrap();
    fs::write(&target, "blocking-file").unwrap();
    let requested = vec!["Assets/Topology/A.asset".to_string()];

    let plan =
        core::preview_discard_files(&project, &requested, Some(checkpoint.as_str())).unwrap();
    assert_eq!(
        plan.selected_paths.as_deref(),
        Some(
            ["Assets/Topology", "Assets/Topology/A.asset"]
                .map(|path| TrackedUnityFilePath::parse(path).unwrap())
                .as_slice()
        )
    );
    assert!(plan
        .directories_to_create
        .iter()
        .any(|path| path.as_str() == "Assets/Topology"));

    core::apply_discard_files_plan(
        &project,
        &requested,
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(target.join("A.asset")).unwrap(),
        "snapshot-a"
    );
    assert!(!target.join("B.asset").exists());
}

#[cfg(windows)]
#[test]
fn checkpoint_and_discard_support_windows_paths_longer_than_260_characters() {
    let (_guard, _temp, project, _data) = setup();
    let segment = "long-directory-segment-abcdefghijklmnopqrstuvwxyz0123456789";
    let relative = format!("Assets/{segment}/{segment}/{segment}/{segment}/LongAsset.prefab");
    let file = project.join(&relative);
    assert!(file.as_os_str().to_string_lossy().encode_utf16().count() > 260);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "before").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "long-path", Default::default())
        .unwrap()
        .checkpoint_id;

    fs::write(&file, "after").unwrap();
    let plan = core::preview_discard_files(
        &project,
        std::slice::from_ref(&relative),
        Some(checkpoint.as_str()),
    )
    .unwrap();
    core::apply_discard_files_plan(
        &project,
        std::slice::from_ref(&relative),
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(fs::read_to_string(file).unwrap(), "before");
}

#[test]
fn discard_deletes_selected_file_absent_from_checkpoint_only() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    fs::write(project.join("Assets/Avatar/Extra.prefab"), "extra").unwrap();

    let plan = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Extra.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    assert_eq!(plan.delete_count, 1);
    core::apply_discard_files_plan(
        &project,
        &["Assets/Avatar/Extra.prefab".to_string()],
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert!(!project.join("Assets/Avatar/Extra.prefab").exists());
    assert_eq!(
        fs::read_to_string(project.join("Assets/Avatar/Foo.prefab")).unwrap(),
        "two"
    );
}

#[test]
fn discard_expands_unity_asset_selection_to_its_meta_file() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let asset = project.join("Assets/Avatar/Added.asset");
    let meta = project.join("Assets/Avatar/Added.asset.meta");
    fs::write(&asset, "asset").unwrap();
    fs::write(&meta, "guid: added").unwrap();
    let requested = vec!["Assets/Avatar/Added.asset".to_string()];

    let plan =
        core::preview_discard_files(&project, &requested, Some(checkpoint.as_str())).unwrap();

    assert_eq!(plan.delete_count, 2);
    assert_eq!(plan.selected_paths.as_ref().unwrap().len(), 2);
    core::apply_discard_files_plan(
        &project,
        &requested,
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert!(!asset.exists());
    assert!(!meta.exists());
}

#[test]
fn discard_rejects_directory_meta_without_changing_the_directory() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let directory = project.join("Assets/Avatar/Folder");
    let meta = project.join("Assets/Avatar/Folder.meta");
    fs::create_dir_all(&directory).unwrap();
    fs::write(&meta, "guid: folder").unwrap();
    let requested = vec!["Assets/Avatar/Folder.meta".to_string()];

    let error =
        core::preview_discard_files(&project, &requested, Some(checkpoint.as_str())).unwrap_err();

    assert!(matches!(
        error,
        core::CheckPoError::UnsafeFolderMetaOperation(path)
            if path == "Assets/Avatar/Folder.meta"
    ));
    assert!(directory.is_dir());
    assert!(meta.is_file());
}

#[test]
fn discard_apply_invalidates_fingerprint_cache_for_changed_path() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = repo_path(&view);
    assert_eq!(fingerprint_count(&repo, "Assets/Avatar/Foo.prefab"), 1);
    fs::write(&file, "two").unwrap();
    let plan = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();

    core::apply_discard_files_plan(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(fingerprint_count(&repo, "Assets/Avatar/Foo.prefab"), 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
}

#[test]
fn discard_apply_moves_replaced_file_to_backup_and_restores_mtime() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let original_time = SystemTime::now() - Duration::from_secs(3600);
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_time)).unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(&file, "two").unwrap();
    let result = core::apply_discard_files_plan(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
        core::preview_discard_files(
            &project,
            &["Assets/Avatar/Foo.prefab".to_string()],
            Some(checkpoint.as_str()),
        )
        .unwrap(),
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert!(result.applied);
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    let tx = result.transaction_id.unwrap();
    assert!(view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("journals")
        .join("transactions")
        .join(tx)
        .join("backup/Assets/Avatar/Foo.prefab")
        .is_file());
    let restored = fs::metadata(&file).unwrap().modified().unwrap();
    assert!(restored.duration_since(original_time).unwrap_or_default() < Duration::from_secs(2));
}

#[cfg(windows)]
#[test]
fn discard_replaces_a_read_only_project_file_after_durable_backup() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(&file, "two").unwrap();
    let mut permissions = fs::metadata(&file).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&file, permissions).unwrap();

    let result = core::apply_discard_files_plan(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
        core::preview_discard_files(
            &project,
            &["Assets/Avatar/Foo.prefab".to_string()],
            Some(checkpoint.as_str()),
        )
        .unwrap(),
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert!(result.applied);
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
}

#[test]
fn mtime_only_change_is_reported_and_discard_restores_snapshot_mtime() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "same-content").unwrap();
    let snapshot_time = SystemTime::now() - Duration::from_secs(7_200);
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(snapshot_time)).unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let changed_time = SystemTime::now() - Duration::from_secs(60);
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(changed_time)).unwrap();

    let diff = core::diff_checkpoint(&project, checkpoint.as_str()).unwrap();
    assert_eq!(diff.modified, vec!["Assets/Avatar/Foo.prefab"]);
    let plan = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    assert_eq!(plan.replace_count, 0);
    assert_eq!(plan.metadata_count, 1);
    assert_eq!(plan.staged_bytes, 0);
    assert_eq!(plan.backup_bytes, 0);
    let result = core::apply_discard_files_plan(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    let transaction_id = result.transaction_id.unwrap();
    let context = core::load_project(&project).unwrap();
    assert!(!context
        .repo_root
        .join("journals/transactions")
        .join(transaction_id)
        .join("backup/Assets/Avatar/Foo.prefab")
        .exists());

    assert_eq!(fs::read_to_string(&file).unwrap(), "same-content");
    let restored = fs::metadata(&file).unwrap().modified().unwrap();
    assert!(restored.duration_since(snapshot_time).unwrap_or_default() < Duration::from_secs(2));
}

#[test]
fn restore_apply_uses_preview_plan_and_does_not_delete_later_added_file() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert_eq!(plan.delete_count, 0);

    fs::write(project.join("Assets/Avatar/New.prefab"), "new").unwrap();
    let error = core::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();
    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(
        fs::read_to_string(project.join("Assets/Avatar/Foo.prefab")).unwrap(),
        "two"
    );
    assert_eq!(
        fs::read_to_string(project.join("Assets/Avatar/New.prefab")).unwrap(),
        "new"
    );
}

#[test]
fn stale_preview_is_rejected_on_apply_plan() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    let context = core::load_project(&project).unwrap();
    let plan = core::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "three").unwrap();
    let error =
        core::apply_plan(&context, plan, core::ApplyOptions { yes: true }, None, None).unwrap_err();
    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
}

#[test]
fn pending_transaction_blocks_new_mutating_operation_and_cleanup_removes_committed_journal() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/pendingtx");
    fs::create_dir_all(&pending).unwrap();
    fs::write(
        pending.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "pendingtx",
            "state": "staged",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": [],
            "directoriesToRemove": [],
            "directoriesToCreate": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let error = core::create_checkpoint(&project, "Blocked", Default::default()).unwrap_err();
    assert!(matches!(error, core::CheckPoError::PendingTransaction(_)));

    core::recover_transactions(&project).unwrap();
    fs::write(
        pending.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "pendingtx",
            "state": "committed",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": [],
            "directoriesToRemove": [],
            "directoriesToCreate": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let denied = cleanup_journals_for_test(&project, false).unwrap_err();
    assert!(denied.to_string().contains("requires --yes"));
    assert!(pending.exists());

    let cleanup = cleanup_journals_for_test(&project, true).unwrap();
    assert_eq!(cleanup.deleted_directory_count, 1);
    assert!(!pending.exists());
}

#[test]
fn cleanup_removes_completed_journals_even_when_payload_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = repo_path(&view);
    for (transaction_id, state) in [
        ("committedwithpayload", "committed"),
        ("recoveredwithpayload", "recovered"),
    ] {
        let journal = repo.join("journals/transactions").join(transaction_id);
        fs::create_dir_all(journal.join("backup/Assets/Avatar")).unwrap();
        fs::create_dir_all(journal.join("staged/Assets/Avatar")).unwrap();
        fs::write(journal.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();
        fs::write(journal.join("staged/Assets/Avatar/Foo.prefab"), "staged").unwrap();
        fs::write(
            journal.join("journal.json"),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 4,
                "transactionId": transaction_id,
                "state": state,
                "intent": "rollbackToBefore",
                "checkpointId": checkpoint,
                "kind": "discard",
                "selectedPaths": null,
                "operations": [],
                "directoriesToRemove": [],
                "directoriesToCreate": [],
                "createdAtUtc": "2026-01-01T00:00:00Z",
                "updatedAtUtc": "2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let plan = core::analyze_transaction_cleanup(&project).unwrap();
    assert_eq!(plan.directory_count, 2);
    assert_eq!(plan.candidates.len(), 2);
    assert!(plan.file_count >= 6);
    assert!(plan.total_bytes > 0);

    let cleanup = core::cleanup_journals_with_expected_plan(
        &project,
        &plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(cleanup.deleted_directory_count, 2);
    assert!(!repo
        .join("journals/transactions/committedwithpayload")
        .exists());
    assert!(!repo
        .join("journals/transactions/recoveredwithpayload")
        .exists());
}

#[test]
fn cleanup_expected_plan_rejects_payload_changes_without_deleting_any_candidate() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let transaction_id = "cleanupplanchange";
    let tx_root = repo_path(&view)
        .join("journals/transactions")
        .join(transaction_id);
    fs::create_dir_all(tx_root.join("backup/Assets/Avatar")).unwrap();
    fs::write(tx_root.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();
    fs::write(
        tx_root.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": transaction_id,
            "state": "committed",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": [],
            "directoriesToRemove": [],
            "directoriesToCreate": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let expected = core::analyze_transaction_cleanup(&project).unwrap();
    assert_eq!(expected.directory_count, 1);
    fs::write(
        tx_root.join("backup/Assets/Avatar/New.prefab"),
        "added after preview",
    )
    .unwrap();

    let error = core::cleanup_journals_with_expected_plan(
        &project,
        &expected,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();
    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert!(tx_root.is_dir());
    assert!(tx_root.join("backup/Assets/Avatar/Foo.prefab").is_file());
    assert!(tx_root.join("backup/Assets/Avatar/New.prefab").is_file());

    let current = core::analyze_transaction_cleanup(&project).unwrap();
    let original = tx_root.join("backup/Assets/Avatar/Foo.prefab");
    fs::write(&original, "BACKUP").unwrap();
    let error = core::cleanup_journals_with_expected_plan(
        &project,
        &current,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();
    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&original).unwrap(), "BACKUP");

    let current = core::analyze_transaction_cleanup(&project).unwrap();
    let cleanup = core::cleanup_journals_with_expected_plan(
        &project,
        &current,
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert_eq!(cleanup.deleted_directory_count, 1);
    assert!(!tx_root.exists());
}

#[test]
fn cleanup_plan_recovers_a_previous_cleanup_trash_batch() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let trash_batch = repo_path(&view)
        .join("journals/transaction-cleanup-trash")
        .join("0123456789abcdef0123456789abcdef");
    fs::create_dir_all(trash_batch.join("old-transaction/backup")).unwrap();
    fs::write(
        trash_batch.join("old-transaction/backup/preserved.bin"),
        b"left after interrupted cleanup",
    )
    .unwrap();

    let plan = core::analyze_transaction_cleanup(&project).unwrap();
    assert_eq!(plan.directory_count, 1);
    assert_eq!(plan.candidates[0].location, "cleanupTrash");
    assert!(plan.total_bytes > 0);

    let result = core::cleanup_journals_with_expected_plan(
        &project,
        &plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert_eq!(result.deleted_directory_count, 1);
    assert!(!trash_batch.exists());
}

#[test]
fn recovery_quarantines_missing_journal_when_payload_is_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/missingjournal");
    fs::create_dir_all(pending.join("staged")).unwrap();
    fs::create_dir_all(pending.join("backup")).unwrap();

    let error = core::create_checkpoint(&project, "Blocked", Default::default()).unwrap_err();
    assert!(matches!(error, core::CheckPoError::PendingTransaction(_)));

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(!pending.exists());
    assert_eq!(
        core::unresolved_transaction_quarantines(&project)
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        core::create_checkpoint(&project, "Blocked", Default::default()).unwrap_err(),
        core::CheckPoError::UnresolvedTransactionQuarantine(_)
    ));
}

#[test]
fn recovery_rejects_missing_journal_when_backup_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/missingjournal");
    fs::create_dir_all(pending.join("backup/Assets/Avatar")).unwrap();
    fs::write(pending.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(!pending.exists());
    let unresolved = core::unresolved_transaction_quarantines(&project).unwrap();
    assert_eq!(unresolved.len(), 1);
    assert!(unresolved[0]
        .quarantine_path
        .join("backup/Assets/Avatar/Foo.prefab")
        .is_file());
}

#[test]
fn recovery_rejects_missing_journal_when_staged_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/missingjournal");
    fs::create_dir_all(pending.join("backup")).unwrap();
    fs::create_dir_all(pending.join("staged/Assets/Avatar")).unwrap();
    fs::write(pending.join("staged/Assets/Avatar/Foo.prefab"), "staged").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0].error.contains("quarantined"));
    assert!(!pending.exists());
    let unresolved = core::unresolved_transaction_quarantines(&project).unwrap();
    assert_eq!(unresolved.len(), 1);
    assert!(unresolved[0]
        .quarantine_path
        .join("staged/Assets/Avatar/Foo.prefab")
        .is_file());
}

#[test]
fn recovery_quarantines_unreadable_journal_when_payload_is_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/unreadablejournal");
    fs::create_dir_all(pending.join("staged")).unwrap();
    fs::create_dir_all(pending.join("backup")).unwrap();
    fs::write(pending.join("journal.json"), "not json").unwrap();

    let pending_transactions = core::pending_transactions(&project).unwrap();
    assert_eq!(pending_transactions.len(), 1);
    assert_eq!(pending_transactions[0].transaction_id, "unreadablejournal");
    assert_eq!(pending_transactions[0].state, "unreadable");

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(!pending.exists());
    assert_eq!(
        core::unresolved_transaction_quarantines(&project)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn recovery_rejects_unreadable_journal_when_backup_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/unreadablejournal");
    fs::create_dir_all(pending.join("backup/Assets/Avatar")).unwrap();
    fs::write(pending.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();
    fs::write(pending.join("journal.json"), "not json").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0].error.contains("quarantined"));
    assert!(!pending.exists());
    let unresolved = core::unresolved_transaction_quarantines(&project).unwrap();
    assert_eq!(unresolved.len(), 1);
    assert!(unresolved[0]
        .quarantine_path
        .join("backup/Assets/Avatar/Foo.prefab")
        .is_file());
}

#[test]
fn recovery_rejects_unreadable_journal_when_staged_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/transactions/unreadablejournal");
    fs::create_dir_all(pending.join("backup")).unwrap();
    fs::create_dir_all(pending.join("staged/Assets/Avatar")).unwrap();
    fs::write(pending.join("staged/Assets/Avatar/Foo.prefab"), "staged").unwrap();
    fs::write(pending.join("journal.json"), "not json").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0].error.contains("quarantined"));
    assert!(!pending.exists());
    let unresolved = core::unresolved_transaction_quarantines(&project).unwrap();
    assert_eq!(unresolved.len(), 1);
    assert!(unresolved[0]
        .quarantine_path
        .join("staged/Assets/Avatar/Foo.prefab")
        .is_file());
}

#[test]
fn recovery_removes_completed_restore_operation() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_file(project.join("Assets/Avatar/Foo.prefab")).unwrap();
    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert_eq!(plan.restore_count, 1);

    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let repo = view.storage_root_path.join("repos").join(view.project_id);
    let tx = repo.join("journals/transactions/restoretx");
    fs::create_dir_all(tx.join("staged")).unwrap();
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "restoretx",
            "state": "applying",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": plan.operations,
            "directoriesToRemove": plan.directories_to_remove,
            "directoriesToCreate": plan.directories_to_create,
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let result = core::recover_transactions(&project).unwrap();
    assert_eq!(result.recovered_transaction_count, 1);
    assert_eq!(result.failed_transaction_count, 0);
    assert!(!project.join("Assets/Avatar/Foo.prefab").exists());
}

#[test]
fn recovery_rejects_unknown_journal_schema_without_touching_project() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "keep").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let tx = repo_path(&view).join("journals/transactions/badschema");
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::create_dir_all(tx.join("staged")).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 99,
            "transactionId": "badschema",
            "state": "applying",
            "checkpointId": checkpoint,
            "kind": "restore",
            "operations": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0].error.contains("schema"));
    assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    assert!(tx.exists());
}

#[test]
fn future_journal_with_unknown_state_is_never_treated_as_unreadable_or_deleted() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "keep").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let transaction_id = "2".repeat(32);
    let tx = repo_path(&view)
        .join("journals/transactions")
        .join(&transaction_id);
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::create_dir_all(tx.join("staged")).unwrap();
    fs::write(
        tx.join("journal.json"),
        br#"{"schemaVersion":5,"state":"pausedByNewClient"}"#,
    )
    .unwrap();

    let pending = core::pending_transactions(&project).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].transaction_id, transaction_id);
    assert_eq!(pending[0].state, "unsupportedSchema:5");

    let recovery = core::recover_transactions(&project).unwrap();
    assert_eq!(recovery.recovered_transaction_count, 0);
    assert_eq!(recovery.failed_transaction_count, 1);
    assert!(recovery.failed_transactions[0]
        .error
        .contains("transaction journal schema"));
    assert!(tx.exists());

    let cleanup = cleanup_journals_for_test(&project, true).unwrap_err();
    assert!(matches!(
        cleanup,
        core::CheckPoError::UnsupportedFormat { found: 5, .. }
    ));
    assert!(tx.exists());
    assert_eq!(
        fs::read_to_string(project.join("Assets/Avatar/Foo.prefab")).unwrap(),
        "keep"
    );

    let quarantined =
        core::quarantine_transaction(&project, &transaction_id, core::ApplyOptions { yes: true })
            .unwrap();
    assert!(quarantined.quarantine_path.join("journal.json").is_file());
    assert!(!tx.exists());
    assert_eq!(
        core::unresolved_transaction_quarantines(&project)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn recovery_rejects_journal_transaction_id_mismatch_without_touching_project() {
    let (_guard, _temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "keep").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let tx = repo_path(&view).join("journals/transactions/directoryid");
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::create_dir_all(tx.join("staged")).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "differentid",
            "state": "applying",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": [],
            "directoriesToRemove": [],
            "directoriesToCreate": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0].error.contains("transaction"));
    assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    assert!(tx.exists());
}

#[test]
fn recovery_does_not_restore_untracked_backup_path() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("README.md"), "keep").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = view.storage_root_path.join("repos").join(view.project_id);
    let tx = repo.join("journals/transactions/badtx");
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::write(tx.join("backup/README.md"), "bad").unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "badtx",
            "state": "applying",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": [],
            "directoriesToRemove": [],
            "directoriesToCreate": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let result = core::recover_transactions(&project).unwrap();
    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(tx.exists());
    assert_eq!(
        fs::read_to_string(project.join("README.md")).unwrap(),
        "keep"
    );
}

#[test]
fn delete_latest_checkpoint_points_latest_ref_to_newest_remaining_checkpoint() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    for index in 0..12 {
        fs::write(project.join("Assets/Avatar/Foo.prefab"), index.to_string()).unwrap();
        core::create_checkpoint(&project, &format!("cp{index}"), Default::default()).unwrap();
    }
    let before = core::list_checkpoints(&project).unwrap();
    let deleted = before[0].checkpoint_id.clone();
    let expected_latest = before[1].checkpoint_id.clone();

    let result = core::delete_checkpoint(&project, deleted.as_str()).unwrap();

    let repo = view.storage_root_path.join("repos").join(view.project_id);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(expected_latest)
    );
}

#[test]
fn delete_checkpoint_leaves_incomplete_sqlite_index_stale_until_explicit_rebuild() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    core::create_checkpoint(&project, "cp1", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    core::create_checkpoint(&project, "cp2", Default::default()).unwrap();
    let before = core::list_checkpoints(&project).unwrap();
    let deleted = before[0].checkpoint_id.clone();
    let expected_latest = before[1].checkpoint_id.clone();
    let repo = repo_path(&view);
    let conn = open_test_index(&repo);
    conn.execute("DELETE FROM object_refs", []).unwrap();
    drop(conn);

    let result = core::delete_checkpoint(&project, deleted.as_str()).unwrap();

    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("SQLite index update failed")));
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(expected_latest.clone())
    );
    let context = core::load_project(&project).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&context).unwrap().state,
        core::CheckpointIndexState::Stale
    );
    assert!(matches!(
        core::list_checkpoints(&project),
        Err(core::CheckPoError::IndexUnavailable(_))
    ));

    core::rebuild_index(&project).unwrap();
    let after = core::list_checkpoints(&project).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].checkpoint_id, expected_latest);
}

#[test]
fn delete_checkpoint_updates_aggregate_object_reference_counts() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    for index in 0..3 {
        fs::write(project.join("Assets/Avatar/Foo.prefab"), index.to_string()).unwrap();
        core::create_checkpoint(&project, &format!("cp{index}"), Default::default()).unwrap();
    }
    let before = core::list_checkpoints(&project).unwrap();
    let deleted = before[0].checkpoint_id.clone();
    let repo = repo_path(&view);
    let deleted_snapshot = core::load_snapshot(&repo, &deleted).unwrap();
    let deleted_file_object = deleted_snapshot
        .files
        .iter()
        .find(|entry| entry.path.as_str() == "Assets/Avatar/Foo.prefab")
        .unwrap()
        .content_hash()
        .to_string();
    let conn = open_test_index(&repo);
    let before_ref_count: i64 = conn
        .query_row(
            "SELECT reference_count FROM object_refs WHERE object_id = ?1",
            [deleted_file_object.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before_ref_count, 1);
    drop(conn);

    let result = core::delete_checkpoint(&project, deleted.as_str()).unwrap();
    let after = core::list_checkpoints(&project).unwrap();

    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(after.len(), 2);
    let conn = open_test_index(&repo);
    let deleted_object_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM object_refs WHERE object_id = ?1",
            [deleted_file_object.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(deleted_object_count, 0);
}

#[test]
fn delete_only_checkpoint_removes_latest_ref_before_snapshot() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    let result = core::delete_checkpoint(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    let repo = repo_path(&view);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(core::read_latest_snapshot_id(&repo).unwrap(), None);
    assert!(!core::snapshot_path(&repo, &checkpoint.checkpoint_id).exists());
}

#[test]
fn checkpoint_create_recovery_discards_prepared_journal_without_published_root() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let unpublished = core::SnapshotId::parse(&"a".repeat(64)).unwrap();
    let journal = write_checkpoint_create_journal(
        &repo,
        "11111111111111111111111111111111",
        &unpublished,
        Some(&first.checkpoint_id),
        "prepared",
    );

    core::recover_checkpoint_deletions(&project).unwrap();

    assert!(!journal.exists());
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(first.checkpoint_id)
    );
}

#[test]
fn checkpoint_create_recovery_finishes_published_root_with_compare_and_swap_latest() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let mut published = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    published.parent_snapshot_id = Some(first.checkpoint_id.clone());
    published.created_at_utc = "2026-07-13T00:00:00.000000000Z".to_string();
    published.name = "two".to_string();
    let published_id = core::__debug_test_save_snapshot(&repo, &published).unwrap();
    assert_eq!(
        core::checkpoint_index_status(&core::load_project(&project).unwrap())
            .unwrap()
            .state,
        core::CheckpointIndexState::Current
    );
    let journal = write_checkpoint_create_journal(
        &repo,
        "22222222222222222222222222222222",
        &published_id,
        Some(&first.checkpoint_id),
        "rootPublished",
    );

    core::recover_checkpoint_deletions(&project).unwrap();

    assert!(!journal.exists());
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(published_id.clone())
    );
    assert_eq!(
        core::checkpoint_index_status(&core::load_project(&project).unwrap())
            .unwrap()
            .state,
        core::CheckpointIndexState::Current
    );
    assert!(core::list_checkpoints(&project)
        .unwrap()
        .iter()
        .any(|checkpoint| checkpoint.checkpoint_id == published_id));
}

#[test]
fn checkpoint_create_recovery_preserves_newer_latest_as_a_branch() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(&file, "two").unwrap();
    let latest = core::create_checkpoint(&project, "latest", Default::default()).unwrap();
    let repo = repo_path(&view);
    let mut branch = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    branch.parent_snapshot_id = Some(first.checkpoint_id.clone());
    branch.created_at_utc = "2026-07-13T00:00:00.000000000Z".to_string();
    branch.name = "recovered branch".to_string();
    let branch_id = core::__debug_test_save_snapshot(&repo, &branch).unwrap();
    let journal = write_checkpoint_create_journal(
        &repo,
        "33333333333333333333333333333333",
        &branch_id,
        Some(&first.checkpoint_id),
        "rootPublished",
    );

    core::recover_checkpoint_deletions(&project).unwrap();

    assert!(!journal.exists());
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(latest.checkpoint_id)
    );
    assert!(core::load_snapshot(&repo, &branch_id).is_ok());
    assert!(core::list_checkpoints(&project)
        .unwrap()
        .iter()
        .any(|checkpoint| checkpoint.checkpoint_id == branch_id));
}

#[test]
fn interrupted_checkpoint_delete_is_completed_from_its_journal() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    let second = core::create_checkpoint(&project, "two", Default::default()).unwrap();
    let repo = repo_path(&view);
    let inventory_head_before = fs::read_to_string(repo.join("inventory/snapshots/head"))
        .unwrap()
        .trim()
        .to_string();
    let transaction_id = "11111111111111111111111111111111";
    let transaction_dir = repo.join("journals/checkpoint-delete").join(transaction_id);
    fs::create_dir_all(&transaction_dir).unwrap();
    fs::rename(
        core::snapshot_path(&repo, &second.checkpoint_id),
        transaction_dir.join("snapshot.root"),
    )
    .unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": transaction_id,
            "checkpointId": second.checkpoint_id,
            "oldLatest": second.checkpoint_id,
            "newLatest": first.checkpoint_id,
            "remainingCheckpointCount": 1,
            "updateIndex": true,
            "inventoryHeadBefore": inventory_head_before,
            "state": "staged",
            "createdAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let checkpoints = core::list_checkpoints(&project).unwrap();

    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_id, first.checkpoint_id);
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(first.checkpoint_id)
    );
    assert!(!transaction_dir.exists());
}

#[test]
fn prepared_checkpoint_delete_with_durable_copy_aborts_without_deleting_original() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let inventory_head_before = fs::read_to_string(repo.join("inventory/snapshots/head"))
        .unwrap()
        .trim()
        .to_string();
    let transaction_id = "12121212121212121212121212121212";
    let transaction_dir = repo.join("journals/checkpoint-delete").join(transaction_id);
    fs::create_dir_all(&transaction_dir).unwrap();
    fs::copy(
        core::snapshot_path(&repo, &checkpoint.checkpoint_id),
        transaction_dir.join("snapshot.root"),
    )
    .unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": transaction_id,
            "checkpointId": checkpoint.checkpoint_id,
            "oldLatest": checkpoint.checkpoint_id,
            "newLatest": null,
            "remainingCheckpointCount": 0,
            "updateIndex": true,
            "inventoryHeadBefore": inventory_head_before,
            "state": "prepared",
            "createdAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    core::recover_checkpoint_deletions(&project).unwrap();

    assert!(core::snapshot_path(&repo, &checkpoint.checkpoint_id).is_file());
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(checkpoint.checkpoint_id)
    );
    assert!(!transaction_dir.exists());
}

#[test]
fn checkpoint_cleanup_trash_is_drained_before_recovery_and_next_operations() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let tracked = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&tracked, "one").unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);

    for family in ["checkpoint-create", "checkpoint-delete"] {
        let journal_family = repo.join("journals").join(family);
        fs::create_dir_all(journal_family.join(".cleanup-full/nested")).unwrap();
        fs::write(
            journal_family.join(".cleanup-full/journal.json"),
            b"invalid and intentionally opaque",
        )
        .unwrap();
        fs::write(
            journal_family.join(".cleanup-full/nested/payload"),
            b"payload",
        )
        .unwrap();
        fs::create_dir_all(journal_family.join(".cleanup-partial")).unwrap();
        fs::write(
            journal_family.join(".cleanup-partial/remainder"),
            b"remainder",
        )
        .unwrap();
        fs::create_dir_all(journal_family.join(".prepare-empty")).unwrap();
    }

    core::recover_checkpoint_deletions(&project).unwrap();
    core::recover_checkpoint_deletions(&project).unwrap();
    for family in ["checkpoint-create", "checkpoint-delete"] {
        assert!(fs::read_dir(repo.join("journals").join(family))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')));
    }

    fs::write(&tracked, "two").unwrap();
    let second = core::create_checkpoint(&project, "two", Default::default()).unwrap();
    core::delete_checkpoint(&project, first.checkpoint_id.as_str()).unwrap();
    let checkpoints = core::list_checkpoints(&project).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_id, second.checkpoint_id);
}

#[test]
fn empty_active_checkpoint_journal_directory_remains_corruption() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let empty = repo_path(&view)
        .join("journals/checkpoint-create")
        .join("44444444444444444444444444444444");
    fs::create_dir_all(&empty).unwrap();

    let error = core::recover_checkpoint_deletions(&project).unwrap_err();

    assert!(empty.is_dir());
    assert!(error.to_string().contains("journal.json"));
}

#[test]
fn storage_gc_deletes_only_unreferenced_loose_objects() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    let second = core::create_checkpoint(&project, "two", Default::default()).unwrap();

    let result = core::delete_checkpoint(&project, first.checkpoint_id.as_str()).unwrap();
    let plan = core::analyze_gc(&project).unwrap();

    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(plan.checkpoint_count, 1);
    assert_eq!(plan.referenced_blob_count, 2);
    assert_eq!(plan.unreferenced_blob_count, 1);
    assert!(!plan.has_integrity_problems);

    let result = core::apply_gc_with_expected_plan(&project, &plan.plan_id).unwrap();
    assert!(result.completed);
    assert!(!result.committed_partially);
    assert_eq!(result.remaining_candidate_count, 0);
    assert_eq!(result.deleted_blob_count, 1);
    let after = core::analyze_gc(&project).unwrap();
    assert_eq!(after.unreferenced_blob_count, 0);
    let diff = core::diff_checkpoint(&project, second.checkpoint_id.as_str()).unwrap();
    assert_eq!(diff.unchanged_count, 2);
}

#[test]
fn storage_gc_reports_partial_progress_after_a_later_candidate_changes() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    for bytes in [b"orphan-a".as_slice(), b"orphan-b".as_slice()] {
        let path = core::object_path(&repo, &core::hash_bytes(bytes));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let plan = core::analyze_gc(&project).unwrap();
    assert_eq!(plan.unreferenced_blobs.len(), 2);
    let first = repo.join(&plan.unreferenced_blobs[0].object_path);
    let second_relative = plan.unreferenced_blobs[1].object_path.clone();
    let second = repo.join(&second_relative);
    let changed = AtomicBool::new(false);
    let progress = |progress: core::OperationProgress| {
        if progress.phase == "gcDeletingObjects"
            && progress.completed == 1
            && !changed.swap(true, Ordering::SeqCst)
        {
            fs::write(&second, b"changed!").unwrap();
        }
    };

    let result = core::apply_gc_with_expected_plan_and_progress_and_cancellation(
        &project,
        &plan.plan_id,
        Some(&progress),
        None,
    )
    .unwrap();

    assert!(!result.completed);
    assert!(result.committed_partially);
    assert_eq!(result.deleted_blob_count, 1);
    assert_eq!(
        result.failed_candidate.as_deref(),
        Some(second_relative.as_path())
    );
    assert_eq!(result.remaining_candidate_count, 1);
    assert!(result.failure.is_some());
    assert!(!first.exists());
    assert_eq!(fs::read(second).unwrap(), b"changed!");
}

#[test]
fn storage_gc_expected_plan_rejects_new_candidate_without_deleting_previewed_objects() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let first_id = core::hash_bytes(b"first orphan");
    let first = core::object_path(&repo, &first_id);
    fs::create_dir_all(first.parent().unwrap()).unwrap();
    fs::write(&first, b"first orphan").unwrap();
    let plan = core::analyze_gc(&project).unwrap();
    let second_id = core::hash_bytes(b"second orphan");
    let second = core::object_path(&repo, &second_id);
    fs::create_dir_all(second.parent().unwrap()).unwrap();
    fs::write(&second, b"second orphan").unwrap();

    let error = core::apply_gc_with_expected_plan(&project, &plan.plan_id).unwrap_err();

    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert!(first.is_file());
    assert!(second.is_file());
}

#[test]
fn storage_gc_expected_plan_rejects_same_size_candidate_replacement() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let object_id = core::hash_bytes(b"first");
    let object = core::object_path(&repo, &object_id);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(&object, b"first").unwrap();
    let plan = core::analyze_gc(&project).unwrap();
    fs::remove_file(&object).unwrap();
    fs::write(&object, b"other").unwrap();

    let error = core::apply_gc_with_expected_plan(&project, &plan.plan_id).unwrap_err();

    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read(&object).unwrap(), b"other");
}

#[test]
fn storage_gc_preserves_shared_manifest_chunks_and_removes_unreachable_chunks() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let bulk = project.join("Assets/Bulk");
    fs::create_dir_all(&bulk).unwrap();
    for index in 0..600_u32 {
        fs::write(
            bulk.join(format!("File{index:04}.asset")),
            format!("{index:04}"),
        )
        .unwrap();
    }
    let first = core::create_checkpoint(&project, "first", Default::default()).unwrap();
    let repo = repo_path(&view);
    let manifest_root = repo.join("manifests/v2");
    let first_chunks = walkdir::WalkDir::new(&manifest_root)
        .follow_links(false)
        .into_iter()
        .map(std::result::Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect::<std::collections::BTreeSet<_>>();

    fs::write(bulk.join("File0000.asset"), "next").unwrap();
    let second = core::create_checkpoint(&project, "second", Default::default()).unwrap();

    core::delete_checkpoint(&project, first.checkpoint_id.as_str()).unwrap();
    let plan = core::analyze_gc(&project).unwrap();
    let unreferenced = plan
        .unreferenced_manifest_chunks
        .iter()
        .map(|chunk| repo.join(&chunk.chunk_path))
        .collect::<std::collections::BTreeSet<_>>();
    let shared_chunks = first_chunks
        .difference(&unreferenced)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(plan.checkpoint_count, 1);
    assert!(plan.unreferenced_manifest_chunk_count > 0);
    assert!(plan.referenced_manifest_chunk_count > 0);
    assert!(!plan.has_integrity_problems);
    assert!(
        !shared_chunks.is_empty(),
        "the second root should retain unchanged chunks from the deleted first root"
    );

    let result = core::apply_gc_with_expected_plan(&project, &plan.plan_id).unwrap();
    assert_eq!(
        result.deleted_manifest_chunk_count,
        plan.unreferenced_manifest_chunk_count
    );
    assert!(shared_chunks.iter().all(|path| path.is_file()));
    let after = core::analyze_gc(&project).unwrap();
    assert_eq!(after.unreferenced_manifest_chunk_count, 0);
    assert_eq!(
        after.manifest_chunk_file_count,
        after.referenced_manifest_chunk_count
    );
    let snapshot = core::load_snapshot(&repo, &second.checkpoint_id).unwrap();
    assert!(snapshot
        .files
        .iter()
        .any(|file| file.path.as_str() == "Assets/Bulk/File0000.asset"));
}

#[test]
fn storage_gc_blocks_on_invalid_manifest_filename() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let invalid = repo_path(&view).join("manifests/v2/leaves/aa/not-a-digest");
    fs::create_dir_all(invalid.parent().unwrap()).unwrap();
    fs::write(&invalid, "orphan").unwrap();

    let plan = core::analyze_gc(&project).unwrap();

    assert!(plan.has_integrity_problems);
    assert!(plan
        .invalid_manifest_chunk_locations
        .iter()
        .any(|location| location.chunk_path.ends_with("not-a-digest")));
    assert!(core::apply_gc_with_expected_plan(&project, &plan.plan_id).is_err());
}

#[cfg(unix)]
#[test]
fn storage_gc_blocks_on_manifest_chunk_symlink() {
    use std::os::unix::fs::symlink;

    let (_guard, temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let digest = "00".repeat(32);
    let link = repo.join("manifests/v2/nodes/00").join(&digest);
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    let target = temp.path().join("outside-manifest");
    fs::write(&target, "outside").unwrap();
    symlink(&target, &link).unwrap();

    let plan = core::analyze_gc(&project).unwrap();

    assert!(plan.has_integrity_problems);
    assert!(plan
        .invalid_manifest_chunk_locations
        .iter()
        .any(|location| location.chunk_path.ends_with(&digest)));
    assert!(core::apply_gc_with_expected_plan(&project, &plan.plan_id).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "outside");
}

#[test]
fn storage_gc_blocks_on_corrupted_reachable_manifest_chunk() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let chunk = first_regular_file_below(&repo.join("manifests/v2"));
    fs::write(&chunk, "corrupt").unwrap();

    let plan = core::analyze_gc(&project).unwrap();

    assert!(plan.has_integrity_problems);
    assert!(plan
        .skipped_snapshots
        .iter()
        .any(|snapshot| snapshot.checkpoint_id == checkpoint.checkpoint_id));
    assert!(core::apply_gc_with_expected_plan(&project, &plan.plan_id).is_err());
    assert!(chunk.exists());
}

#[test]
fn storage_gc_blocks_when_reachable_object_size_does_not_match_snapshot() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let snapshot = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::write(&object, "wrong-size").unwrap();

    let plan = core::analyze_gc(&project).unwrap();

    assert!(plan.has_integrity_problems);
    assert!(plan
        .invalid_object_locations
        .iter()
        .any(|location| location.reason.contains("reachable object size mismatch")));
    assert!(core::apply_gc_with_expected_plan(&project, &plan.plan_id).is_err());
}

#[test]
fn storage_gc_blocks_conflicting_expected_sizes_for_same_object_id() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let mut conflicting = core::load_snapshot(&repo, &checkpoint.checkpoint_id).unwrap();
    let object_id = conflicting.files[0].content_hash().clone();
    conflicting.parent_snapshot_id = Some(checkpoint.checkpoint_id);
    conflicting.name = "conflicting size".to_string();
    conflicting.files[0].size_bytes = 4;
    conflicting.files[0].content = core::SnapshotContent::Whole {
        hash: object_id,
        size_bytes: 4,
    };
    let conflicting_id = core::__debug_test_save_snapshot(&repo, &conflicting).unwrap();
    core::__debug_test_add_snapshot_to_inventory(
        &repo,
        &core::ProjectId::parse(&view.project_id).unwrap(),
        &conflicting_id,
    )
    .unwrap();

    let plan = core::analyze_gc(&project).unwrap();

    assert!(plan.has_integrity_problems);
    assert!(plan
        .invalid_object_locations
        .iter()
        .any(|location| location.reason.contains("conflicting expected sizes")));
    assert!(core::apply_gc_with_expected_plan(&project, &plan.plan_id).is_err());
}

#[test]
fn storage_gc_truncates_display_details_but_apply_deletes_every_candidate() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    for index in 0..1_005_u32 {
        let bytes = index.to_le_bytes();
        let object_id = core::hash_bytes(&bytes);
        let path = core::object_path(&repo, &object_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    let plan = core::analyze_gc(&project).unwrap();
    assert_eq!(plan.unreferenced_blob_count, 1_005);
    assert_eq!(plan.unreferenced_blobs.len(), 1_000);
    assert!(plan.details_truncated);

    let result = core::apply_gc_with_expected_plan(&project, &plan.plan_id).unwrap();
    assert_eq!(result.deleted_blob_count, 1_005);
    assert!(result.completed);
    assert!(!result.committed_partially);
    assert_eq!(result.remaining_candidate_count, 0);
    assert!(result.plan.details_truncated);
    assert_eq!(
        core::analyze_gc(&project).unwrap().unreferenced_blob_count,
        0
    );
}

#[test]
fn storage_gc_sweeps_unreachable_inventory_nodes() {
    let (_guard, _temp, project, _data) = setup();
    let tracked = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&tracked, "one").unwrap();
    init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(&tracked, "two").unwrap();
    core::create_checkpoint(&project, "two", Default::default()).unwrap();

    let plan = core::analyze_gc(&project).unwrap();
    assert!(plan.unreferenced_inventory_node_count > 0);
    assert_eq!(
        plan.unreferenced_inventory_node_count,
        plan.unreferenced_inventory_nodes.len()
    );

    let result = core::apply_gc_with_expected_plan(&project, &plan.plan_id).unwrap();
    assert!(result.completed);
    assert_eq!(
        result.deleted_inventory_node_count,
        plan.unreferenced_inventory_node_count
    );
    assert!(result.deleted_inventory_node_bytes > 0);
    let after = core::analyze_gc(&project).unwrap();
    assert_eq!(after.unreferenced_inventory_node_count, 0);
    assert!(core::verify_project(&project, true).unwrap().is_valid);
}

#[test]
fn storage_gc_honors_pre_cancelled_token() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    let token = core::CancellationToken::new();
    token.cancel();

    let error =
        core::analyze_gc_with_progress_and_cancellation(&project, None, Some(&token)).unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
}

#[test]
fn copied_project_can_be_initialized_as_separate_project() {
    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let original = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Original", Default::default()).unwrap();
    let copied = temp.path().join("UnityProjectCopy");
    fs::create_dir_all(copied.join("Assets/Avatar")).unwrap();
    fs::create_dir_all(copied.join("Packages")).unwrap();
    fs::create_dir_all(copied.join("ProjectSettings")).unwrap();
    fs::create_dir_all(copied.join(".checkpo")).unwrap();
    fs::write(copied.join("Assets/Avatar/Foo.prefab"), "copy").unwrap();
    fs::copy(
        project.join(".checkpo/project.json"),
        copied.join(".checkpo/project.json"),
    )
    .unwrap();

    let copied_load = core::load_project_view(&copied).unwrap();

    assert_eq!(copied_load.project_id, original.project_id);
    assert_eq!(
        copied_load.location_status,
        ProjectLocationStatus::CopiedSuspected
    );
    assert_eq!(copied_load.warnings.len(), 1);
    let warning = &copied_load.warnings[0];
    assert_eq!(
        warning.kind,
        core::ProjectWarningKind::CopiedProjectSuspected
    );
    assert_eq!(
        warning.location_status,
        ProjectLocationStatus::CopiedSuspected
    );
    assert!(warning.message.contains("copied Unity project"));
    assert_eq!(
        warning.previous_project_root_path,
        project.canonicalize().unwrap()
    );
    assert_eq!(
        warning.current_project_root_path,
        copied.canonicalize().unwrap()
    );
    assert!(warning.previous_path_exists);
    assert!(warning.previous_marker_has_same_project_id);
    assert!(warning.requires_user_decision);
    assert!(!warning.destructive_operations_allowed);

    let error = init_project_for_test(&copied).unwrap_err();
    assert_copied_project_error(&error);

    let pending_project_id = "a".repeat(32);
    fs::write(
        copied.join(".checkpo/pending-separate-init.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "previousProjectId": original.project_id.as_str(),
            "newMarker": {
                "schemaVersion": 1,
                "projectId": pending_project_id,
                "createdAtUtc": "2026-07-15T00:00:00Z"
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let copied_view =
        core::start_as_separate_project(&copied, core::ApplyOptions { yes: true }).unwrap();
    let copied_marker: core::ProjectMarkerFile =
        core::read_json(&copied.join(".checkpo/project.json")).unwrap();

    assert_ne!(copied_view.project_id, original.project_id);
    assert_eq!(copied_view.project_id, pending_project_id);
    assert_eq!(copied_marker.project_id.as_str(), copied_view.project_id);
    assert!(!copied.join(".checkpo/pending-separate-init.json").exists());
    assert!(repo_path(&copied_view).join("repo.json").is_file());
    assert_eq!(core::list_checkpoints(&copied).unwrap().len(), 0);
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
}

#[test]
fn pending_separate_init_cannot_claim_an_existing_project_id() {
    let (_guard, temp, project, _data) = setup();
    let source = init_project_for_test(&project).unwrap();
    let owner = temp.path().join("ExistingOwner");
    fs::create_dir_all(owner.join("Assets")).unwrap();
    fs::create_dir_all(owner.join("Packages")).unwrap();
    fs::create_dir_all(owner.join("ProjectSettings")).unwrap();
    fs::write(
        owner.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    )
    .unwrap();
    let existing_owner = init_project_for_test(&owner).unwrap();
    let copied = temp.path().join("CopiedSource");
    fs::create_dir_all(copied.join("Assets")).unwrap();
    fs::create_dir_all(copied.join("Packages")).unwrap();
    fs::create_dir_all(copied.join("ProjectSettings")).unwrap();
    fs::create_dir_all(copied.join(".checkpo")).unwrap();
    fs::copy(
        project.join(".checkpo/project.json"),
        copied.join(".checkpo/project.json"),
    )
    .unwrap();
    fs::write(
        copied.join(".checkpo/pending-separate-init.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "previousProjectId": source.project_id.as_str(),
            "newMarker": {
                "schemaVersion": 1,
                "projectId": existing_owner.project_id.as_str(),
                "createdAtUtc": "2026-07-15T00:00:00Z"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    assert!(matches!(
        core::start_as_separate_project(&copied, core::ApplyOptions { yes: true }),
        Err(core::CheckPoError::InvalidProject(_))
    ));
    assert_eq!(
        core::load_project_view(&owner).unwrap().project_id,
        existing_owner.project_id
    );
    let copied_marker: core::ProjectMarkerFile =
        core::read_json(&copied.join(".checkpo/project.json")).unwrap();
    assert_eq!(copied_marker.project_id.as_str(), source.project_id);
}

#[test]
fn copied_project_location_can_be_confirmed_as_current() {
    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let original = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Original", Default::default()).unwrap();
    let copied = temp.path().join("UnityProjectCopy");
    fs::create_dir_all(copied.join("Assets/Avatar")).unwrap();
    fs::create_dir_all(copied.join("Packages")).unwrap();
    fs::create_dir_all(copied.join("ProjectSettings")).unwrap();
    fs::create_dir_all(copied.join(".checkpo")).unwrap();
    fs::write(copied.join("Assets/Avatar/Foo.prefab"), "copy").unwrap();
    fs::copy(
        project.join(".checkpo/project.json"),
        copied.join(".checkpo/project.json"),
    )
    .unwrap();

    let copied_load = core::load_project_view(&copied).unwrap();
    assert_eq!(copied_load.project_id, original.project_id);
    assert_eq!(
        copied_load.location_status,
        ProjectLocationStatus::CopiedSuspected
    );
    assert_eq!(copied_load.warnings.len(), 1);
    assert!(copied_load.warnings[0].previous_path_exists);
    assert!(copied_load.warnings[0].previous_marker_has_same_project_id);

    let confirmed = core::confirm_project_location(&copied).unwrap();
    assert_eq!(confirmed.project_id, original.project_id);
    assert_eq!(confirmed.location_status, ProjectLocationStatus::Current);
    assert!(confirmed.warnings.is_empty());
    assert!(core::load_project_view(&copied)
        .unwrap()
        .warnings
        .is_empty());

    let original_reload = core::load_project_view(&project).unwrap();
    assert_eq!(original_reload.project_id, original.project_id);
    assert_eq!(original_reload.warnings.len(), 1);
    assert_eq!(
        original_reload.location_status,
        ProjectLocationStatus::CopiedSuspected
    );
    assert_eq!(
        original_reload.warnings[0].previous_project_root_path,
        copied.canonicalize().unwrap()
    );
    assert!(original_reload.warnings[0].previous_path_exists);
    assert!(original_reload.warnings[0].previous_marker_has_same_project_id);
}

#[test]
fn load_project_refreshes_registry_after_project_move() {
    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let original = init_project_for_test(&project).unwrap();
    let original_project_root = project.canonicalize().unwrap();
    let moved = temp.path().join("UnityProjectMoved");
    fs::rename(&project, &moved).unwrap();

    let moved_view = core::load_project_view(&moved).unwrap();
    assert_eq!(moved_view.project_id, original.project_id);
    assert_eq!(
        moved_view.location_status,
        ProjectLocationStatus::MovedFromMissingOrDifferentMarker
    );
    assert_eq!(moved_view.warnings.len(), 1);
    let warning = &moved_view.warnings[0];
    assert_eq!(warning.kind, core::ProjectWarningKind::ProjectMoved);
    assert_eq!(
        warning.location_status,
        ProjectLocationStatus::MovedFromMissingOrDifferentMarker
    );
    assert_eq!(warning.previous_project_root_path, original_project_root);
    assert_eq!(
        warning.current_project_root_path,
        moved.canonicalize().unwrap()
    );
    assert!(!warning.previous_path_exists);
    assert!(!warning.previous_marker_has_same_project_id);
    assert!(!warning.requires_user_decision);
    assert!(warning.destructive_operations_allowed);

    fs::create_dir_all(project.join("Assets")).unwrap();
    fs::create_dir_all(project.join("Packages")).unwrap();
    fs::create_dir_all(project.join("ProjectSettings")).unwrap();

    let reloaded = core::load_project_view(&moved).unwrap();
    assert_eq!(reloaded.project_root_path, moved.canonicalize().unwrap());
    assert_eq!(reloaded.location_status, ProjectLocationStatus::Current);
    assert!(reloaded.warnings.is_empty());
}

#[test]
fn concurrent_moved_project_claim_allows_only_one_location_to_win() {
    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let original = init_project_for_test(&project).unwrap();
    let marker = fs::read(project.join(".checkpo/project.json")).unwrap();
    let mut candidates = Vec::new();
    for name in ["MovedA", "MovedB"] {
        let candidate = temp.path().join(name);
        fs::create_dir_all(candidate.join("Assets/Avatar")).unwrap();
        fs::create_dir_all(candidate.join("Packages")).unwrap();
        fs::create_dir_all(candidate.join("ProjectSettings")).unwrap();
        fs::create_dir_all(candidate.join(".checkpo")).unwrap();
        fs::write(candidate.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
        fs::write(candidate.join(".checkpo/project.json"), &marker).unwrap();
        candidates.push(candidate);
    }
    fs::remove_dir_all(&project).unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(3));
    let handles = candidates
        .into_iter()
        .map(|candidate| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let result = core::load_project_view(&candidate);
                (candidate, result)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, result)| {
                result.as_ref().is_ok_and(|view| {
                    view.location_status == ProjectLocationStatus::MovedFromMissingOrDifferentMarker
                })
            })
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, result)| {
                result.as_ref().is_ok_and(|view| {
                    view.location_status == ProjectLocationStatus::CopiedSuspected
                }) || matches!(result, Err(core::CheckPoError::RepositoryLocked(_)))
            })
            .count(),
        1
    );
    let winner = outcomes
        .iter()
        .find(|(_, result)| {
            result.as_ref().is_ok_and(|view| {
                view.location_status == ProjectLocationStatus::MovedFromMissingOrDifferentMarker
            })
        })
        .map(|(path, _)| path)
        .unwrap();
    let loser = outcomes
        .iter()
        .find(|(path, _)| path != winner)
        .map(|(path, _)| path)
        .unwrap();
    let winner_view = core::load_project_view(winner).unwrap();
    assert_eq!(winner_view.project_id, original.project_id);
    assert_eq!(winner_view.location_status, ProjectLocationStatus::Current);
    assert_eq!(
        core::load_project_view(loser).unwrap().location_status,
        ProjectLocationStatus::CopiedSuspected
    );
}

#[test]
fn concurrent_initialization_publishes_only_one_project_lineage() {
    let (_guard, _temp, project, data) = setup();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            std::thread::spawn(move || {
                barrier.wait();
                core::init_project(&project)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let successful_ids = outcomes
        .iter()
        .filter_map(|result| result.as_ref().ok().map(|view| view.project_id.clone()))
        .collect::<Vec<_>>();
    assert!(!successful_ids.is_empty());
    assert!(successful_ids
        .iter()
        .all(|project_id| project_id == &successful_ids[0]));
    let reloaded = core::init_project(&project).unwrap();
    assert_eq!(reloaded.project_id, successful_ids[0]);
    let repositories = fs::read_dir(data.join("repos"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count();
    assert_eq!(repositories, 1);
}

#[test]
fn copied_project_blocks_mutating_operations_until_decided() {
    let (_guard, temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let original = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Original", Default::default())
        .unwrap()
        .checkpoint_id;
    let copied = temp.path().join("UnityProjectCopy");
    fs::create_dir_all(copied.join("Assets/Avatar")).unwrap();
    fs::create_dir_all(copied.join("Packages")).unwrap();
    fs::create_dir_all(copied.join("ProjectSettings")).unwrap();
    fs::create_dir_all(copied.join(".checkpo")).unwrap();
    fs::write(copied.join("Assets/Avatar/Foo.prefab"), "copy").unwrap();
    fs::copy(
        project.join(".checkpo/project.json"),
        copied.join(".checkpo/project.json"),
    )
    .unwrap();

    assert_eq!(
        core::load_project_view(&copied).unwrap().location_status,
        ProjectLocationStatus::CopiedSuspected
    );
    assert_eq!(
        core::preview_restore(&copied, checkpoint.as_str())
            .unwrap()
            .checkpoint_id,
        checkpoint
    );

    for error in [
        core::create_checkpoint(&copied, "blocked", Default::default()).unwrap_err(),
        core::delete_checkpoint(&copied, checkpoint.as_str()).unwrap_err(),
        core::apply_gc_with_expected_plan(&copied, &"0".repeat(64)).unwrap_err(),
        core::rebuild_index(&copied).unwrap_err(),
    ] {
        assert_copied_project_error(&error);
    }

    let plan = core::preview_restore(&copied, checkpoint.as_str()).unwrap();
    let error = core::apply_restore_plan(
        &copied,
        checkpoint.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap_err();
    assert_copied_project_error(&error);

    let repo = repo_path(&original);
    let index_db = core::db_path(&repo).unwrap();
    if index_db.exists() {
        fs::remove_file(&index_db).unwrap();
    }
    assert!(matches!(
        core::list_checkpoints(&copied),
        Err(core::CheckPoError::IndexUnavailable(_))
    ));
    assert!(!index_db.exists());

    let tx = repo.join("journals/transactions/copiedtx");
    fs::create_dir_all(&tx).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 4,
            "transactionId": "copiedtx",
            "state": "created",
            "intent": "rollbackToBefore",
            "checkpointId": checkpoint,
            "kind": "restore",
            "selectedPaths": null,
            "operations": [],
            "directoriesToRemove": [],
            "directoriesToCreate": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let error = core::recover_transactions(&copied).unwrap_err();
    assert_copied_project_error(&error);
    let error = cleanup_journals_for_test(&copied, true).unwrap_err();
    assert_copied_project_error(&error);
    assert!(tx.join("journal.json").is_file());

    let pending_error =
        core::start_as_separate_project(&copied, core::ApplyOptions { yes: true }).unwrap_err();
    assert!(matches!(
        pending_error,
        core::CheckPoError::PendingTransaction(_)
    ));
    fs::remove_dir_all(&tx).unwrap();
    let copied_view =
        core::start_as_separate_project(&copied, core::ApplyOptions { yes: true }).unwrap();
    assert_ne!(copied_view.project_id, original.project_id);
    core::create_checkpoint(&copied, "separate", Default::default()).unwrap();
}

#[cfg(unix)]
#[test]
fn stale_repository_lock_metadata_does_not_block_or_get_modified_by_an_os_lock() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = view.storage_root_path.join("repos").join(view.project_id);
    let lock = repo.join("locks/repository.lock");
    fs::write(
        &lock,
        "operation=test\npid=99999999\ncreatedAtUtc=2026-01-01T00:00:00Z\n",
    )
    .unwrap();

    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    let metadata = fs::read_to_string(lock).unwrap();
    assert_eq!(
        metadata,
        "operation=test\npid=99999999\ncreatedAtUtc=2026-01-01T00:00:00Z\n"
    );
}

#[test]
fn malformed_repository_lock_metadata_is_ignored_without_being_modified() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let lock = repo.join("locks/repository.lock");
    fs::write(&lock, "not a valid lock").unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();

    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    assert_eq!(fs::read_to_string(lock).unwrap(), "not a valid lock");
}

#[test]
fn old_malformed_repository_lock_metadata_is_ignored_without_age_heuristics() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let lock = repo.join("locks/repository.lock");
    fs::write(&lock, "not a valid lock").unwrap();
    filetime::set_file_mtime(
        &lock,
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
    )
    .unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();

    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    assert_eq!(fs::read_to_string(lock).unwrap(), "not a valid lock");
}
