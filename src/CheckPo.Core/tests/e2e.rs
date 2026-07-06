use checkpo_core as core;
use core::{
    FileOperationType, OperationPlanKind, ProjectLocationStatus, SnapshotContent, SnapshotEntry,
    SnapshotFile, TrackedUnityFilePath,
};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

fn setup() -> (
    MutexGuard<'static, ()>,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
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

fn init_project_for_test(project: &std::path::Path) -> core::Result<core::ProjectView> {
    core::init_project(project)
}

fn fingerprint_count(repo: &std::path::Path, path: &str) -> i64 {
    let conn = core::open_db(repo).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM file_fingerprints WHERE path = ?1",
        [path],
        |row| row.get(0),
    )
    .unwrap()
}

fn assert_copied_project_error(error: &core::CheckPoError) {
    assert!(matches!(
        error,
        core::CheckPoError::CopiedProjectSuspected(_)
    ));
    assert!(error.to_string().contains("appears to be a copy"));
}

#[test]
fn tracked_unity_file_path_validation() {
    for path in [
        "Assets/Foo.prefab",
        "Assets/Foo.prefab ",
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

    assert!(error.to_string().contains("tracked Unity folder"));
}

#[test]
fn canonical_snapshot_json_v1_is_stable() {
    let snapshot = SnapshotFile {
        schema_version: 1,
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
    let expected = "{\"schemaVersion\":1,\"projectId\":\"11111111111111111111111111111111\",\"parentSnapshotId\":null,\"createdAtUtc\":\"2026-01-01T00:00:00Z\",\"name\":\"Initial\",\"toolVersion\":\"0.1.0\",\"trackedRoots\":[\"Assets\",\"Packages\",\"ProjectSettings\"],\"files\":[{\"path\":\"Assets/Avatar/Foo.prefab\",\"sizeBytes\":3,\"modifiedAtUtc\":\"2026-01-01T00:00:01Z\",\"content\":{\"type\":\"whole\",\"hash\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"sizeBytes\":3}}]}";

    let bytes = core::canonical_snapshot_bytes(&snapshot).unwrap();

    assert_eq!(std::str::from_utf8(&bytes).unwrap(), expected);
    assert_eq!(
        core::snapshot_id_from_bytes(&bytes).as_str(),
        "b586ee640bb7bccaab8326ccbc92db38f81c2ea641a79867df0398c6e6658b89"
    );
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
    assert!(repo
        .join("snapshots")
        .join(format!("{}.json", summary.checkpoint_id))
        .is_file());
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

    assert!(matches!(error, core::CheckPoError::WorkingTreeChanged(_)));
}

#[test]
fn known_hash_object_store_cleans_temp_file_on_hash_mismatch() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let source = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&source, "one").unwrap();
    let wrong_hash = core::ObjectId::parse(&"0".repeat(64)).unwrap();

    let error =
        core::put_object_from_file_with_known_hash(&repo, &source, &wrong_hash, 3).unwrap_err();

    assert!(matches!(error, core::CheckPoError::ObjectHashMismatch(_)));
    assert!(fs::read_dir(repo.join("tmp")).unwrap().next().is_none());
}

#[test]
fn checkpoint_reuses_existing_object_without_full_rehash() {
    let (_guard, _temp, project, _data) = setup();
    let source = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&source, "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let snapshot = core::load_snapshot(&repo, &first.checkpoint_id).unwrap();
    let object = core::object_path(&repo, snapshot.files[0].content_hash());
    fs::write(&object, "two").unwrap();

    let second = core::create_checkpoint(&project, "Reuse", Default::default()).unwrap();

    assert_eq!(second.newly_stored_bytes, 0);
    assert_eq!(fs::read_to_string(&object).unwrap(), "two");
    assert!(!core::verify_project(&project, true).unwrap().is_valid);
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
    let tx = repo.join("journals/symlinktx");
    fs::create_dir_all(tx.join("backup/Assets/Avatar")).unwrap();
    fs::write(tx.join("backup/Assets/Avatar/Foo.prefab"), "two").unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "symlinktx",
            "state": "applying",
            "checkpointId": checkpoint,
            "kind": "discard",
            "operations": plan.operations,
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
    let tx = repo.join("journals/symlinkbackuptx");
    fs::create_dir_all(tx.join("backup/Assets/Avatar")).unwrap();
    let outside = temp.path().join("outside.prefab");
    fs::write(&outside, "two").unwrap();
    std::os::unix::fs::symlink(&outside, tx.join("backup/Assets/Avatar/Foo.prefab")).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "symlinkbackuptx",
            "state": "applying",
            "checkpointId": checkpoint,
            "kind": "discard",
            "operations": plan.operations,
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
fn checkpoint_ignores_symlink_file_but_apply_rejects_tracked_leaf_symlink() {
    let (_guard, temp, project, _data) = setup();
    let file = project.join("Assets/Avatar/Foo.prefab");
    let link = project.join("Assets/Avatar/Linked.prefab");
    let outside = temp.path().join("outside.prefab");
    fs::write(&file, "one").unwrap();
    fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let view = init_project_for_test(&project).unwrap();

    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    let snapshot = core::load_snapshot(&repo_path(&view), &checkpoint.checkpoint_id).unwrap();
    assert_eq!(snapshot.files.len(), 2);
    assert!(snapshot
        .files
        .iter()
        .all(|file| file.path.as_str() != "Assets/Avatar/Linked.prefab"));

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

#[test]
fn checkpoint_recreates_incompatible_sqlite_index_schema() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let conn = core::open_db(&repo).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE snapshots(old_snapshot_id TEXT PRIMARY KEY);
        CREATE TABLE legacy_cache(value TEXT);
        CREATE TABLE \"legacy-cache\"(value TEXT);
        ",
    )
    .unwrap();
    drop(conn);

    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    let conn = core::open_db(&repo).unwrap();
    let snapshot_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!(snapshot_count, 1);
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
fn list_and_storage_summary_rebuild_missing_sqlite_index() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    core::create_checkpoint(&project, "two", Default::default()).unwrap();
    let repo = repo_path(&view);
    fs::remove_file(core::db_path(&repo)).unwrap();

    let checkpoints = core::list_checkpoints(&project).unwrap();
    let summary = core::storage_summary(&project).unwrap();

    assert_eq!(checkpoints.len(), 2);
    assert_eq!(summary.checkpoint_count, 2);
    assert_eq!(summary.unique_blob_count, 3);
    assert!(core::db_path(&repo).is_file());
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
    let (indexed_checkpoints, _) =
        core::checkpoint_summaries_and_storage_summary_from_index(&context).unwrap();
    assert_eq!(indexed_checkpoints[0].name, "before");
    assert!(!indexed_checkpoints[0].warnings.is_empty());
}

#[test]
fn list_checkpoints_propagates_unreadable_sqlite_index_for_current_project() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let db_path = core::db_path(&repo);
    fs::remove_file(&db_path).unwrap();
    fs::create_dir_all(&db_path).unwrap();

    let error = core::list_checkpoints(&project).unwrap_err();

    assert!(matches!(error, core::CheckPoError::Io { .. }));
}

#[test]
fn cancelled_rebuild_index_does_not_remove_existing_index_db() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "one", Default::default()).unwrap();
    let repo = repo_path(&view);
    let index_db = core::db_path(&repo);
    assert!(index_db.is_file());
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);

    let token = core::CancellationToken::new();
    token.cancel();
    let context = core::load_project(&project).unwrap();
    let error = core::rebuild_index_for_project_with_progress_and_cancellation(
        &context,
        None,
        Some(&token),
    )
    .unwrap_err();

    assert!(matches!(error, core::CheckPoError::Cancelled));
    assert!(index_db.is_file());
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
}

#[test]
fn corrupt_snapshot_rebuild_does_not_record_current_fingerprint() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let first = core::create_checkpoint(&project, "one", Default::default()).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    core::create_checkpoint(&project, "two", Default::default()).unwrap();
    let repo = repo_path(&view);
    fs::write(
        repo.join("snapshots")
            .join(format!("{}.json", first.checkpoint_id)),
        "not json",
    )
    .unwrap();

    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    let conn = core::open_db(&repo).unwrap();
    let fingerprint_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM index_state WHERE key = 'snapshot_dir_fingerprint'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fingerprint_count, 0);
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
fn checkpoint_scan_skips_checkpo_temporary_files() {
    let (_guard, _temp, project, _data) = setup();
    init_project_for_test(&project).unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    fs::write(
        project.join("Assets/Avatar/.checkpo-Foo.prefab-1234567890abcdef1234567890abcdef.tmp"),
        "temp",
    )
    .unwrap();
    fs::write(
        project.join("Assets/Avatar/.Foo.prefab.1234567890abcdef1234567890abcdef.tmp"),
        "temp",
    )
    .unwrap();

    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let diff = core::diff_checkpoint(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    assert_eq!(checkpoint.file_count, 2);
    assert!(checkpoint
        .warnings
        .iter()
        .any(|warning| warning.contains(".checkpo-Foo.prefab")));
    assert!(checkpoint
        .warnings
        .iter()
        .any(|warning| warning.contains(".Foo.prefab.")));
    assert!(diff.added.is_empty());
    assert_eq!(diff.unchanged_count, 2);
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
fn fast_diff_detects_same_size_same_mtime_change() {
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
fn tampered_snapshot_path_cannot_reach_restore_operation() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "safe-object").unwrap();
    fs::write(project.join("README.md"), "keep-readme").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let snapshot_path = repo_path(&view)
        .join("snapshots")
        .join(format!("{checkpoint}.json"));
    let tampered = fs::read_to_string(&snapshot_path)
        .unwrap()
        .replace("Assets/Avatar/Foo.prefab", "README.md");
    fs::write(&snapshot_path, tampered).unwrap();

    let error = core::preview_restore(&project, checkpoint.as_str()).unwrap_err();
    assert!(
        error.to_string().contains("tracked path")
            || error
                .to_string()
                .contains("snapshot filename digest mismatch")
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
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let snapshot_path = repo_path(&view)
        .join("snapshots")
        .join(format!("{checkpoint}.json"));
    let valid_hash = core::load_snapshot(&repo_path(&view), &checkpoint)
        .unwrap()
        .files[0]
        .content_hash()
        .to_string();
    let tampered = fs::read_to_string(&snapshot_path)
        .unwrap()
        .replace(&valid_hash, "short");
    fs::write(&snapshot_path, tampered).unwrap();

    let result = core::verify_project(&project, false).unwrap();
    assert!(!result.is_valid);
    assert!(result
        .errors
        .iter()
        .any(|error| error.contains("object id") || error.contains("short")));
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
        schema_version: 1,
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
    let duplicate_id = core::save_snapshot(&repo, &duplicate).unwrap();
    let duplicate_error = core::load_snapshot(&repo, &duplicate_id).unwrap_err();
    assert!(duplicate_error.to_string().contains("duplicate"));

    let mut invalid_roots = duplicate;
    invalid_roots.files = vec![entry.clone()];
    invalid_roots.tracked_roots = vec!["Assets".to_string()];
    let invalid_roots_id = core::save_snapshot(&repo, &invalid_roots).unwrap();
    let invalid_roots_error = core::load_snapshot(&repo, &invalid_roots_id).unwrap_err();
    assert!(invalid_roots_error.to_string().contains("tracked roots"));

    let mut invalid_time = invalid_roots;
    invalid_time.tracked_roots = vec![
        "Assets".to_string(),
        "Packages".to_string(),
        "ProjectSettings".to_string(),
    ];
    invalid_time.files[0].modified_at_utc = "not-a-time".to_string();
    let invalid_time_id = core::save_snapshot(&repo, &invalid_time).unwrap();
    let invalid_time_error = core::load_snapshot(&repo, &invalid_time_id).unwrap_err();
    assert!(invalid_time_error.to_string().contains("modifiedAtUtc"));
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
    let foreign_id = core::save_snapshot(&repo, &foreign).unwrap();

    let error = core::diff_checkpoint(&project, foreign_id.as_str()).unwrap_err();

    assert!(error.to_string().contains("project id does not match"));
}

#[test]
fn checkpoint_list_ignores_invalid_json_but_verify_warns() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let snapshots = view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("snapshots");
    fs::write(snapshots.join("README.json"), "{}").unwrap();
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
    let verify = core::verify_project(&project, false).unwrap();
    assert!(verify.is_valid);
    assert!(verify
        .warnings
        .iter()
        .any(|warning| warning.contains("README.json")));
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
        .join(tx)
        .join("backup/Assets/Avatar/Foo.prefab")
        .is_file());
    let restored = fs::metadata(&file).unwrap().modified().unwrap();
    assert!(restored.duration_since(original_time).unwrap_or_default() < Duration::from_secs(2));
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
    let pending = repo.join("journals/pendingtx");
    fs::create_dir_all(&pending).unwrap();
    fs::write(
        pending.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "pendingtx",
            "state": "staged",
            "checkpointId": checkpoint,
            "kind": "restore",
            "operations": [],
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
            "schemaVersion": 1,
            "transactionId": "pendingtx",
            "state": "committed",
            "checkpointId": checkpoint,
            "kind": "restore",
            "operations": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let cleanup = core::cleanup_journals(&project).unwrap();
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
        let journal = repo.join("journals").join(transaction_id);
        fs::create_dir_all(journal.join("backup/Assets/Avatar")).unwrap();
        fs::create_dir_all(journal.join("staged/Assets/Avatar")).unwrap();
        fs::write(journal.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();
        fs::write(journal.join("staged/Assets/Avatar/Foo.prefab"), "staged").unwrap();
        fs::write(
            journal.join("journal.json"),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "transactionId": transaction_id,
                "state": state,
                "checkpointId": checkpoint,
                "kind": "discard",
                "operations": [],
                "createdAtUtc": "2026-01-01T00:00:00Z",
                "updatedAtUtc": "2026-01-01T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let cleanup = core::cleanup_journals(&project).unwrap();

    assert_eq!(cleanup.deleted_directory_count, 2);
    assert!(!repo.join("journals/committedwithpayload").exists());
    assert!(!repo.join("journals/recoveredwithpayload").exists());
}

#[test]
fn recovery_removes_missing_journal_when_backup_is_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/missingjournal");
    fs::create_dir_all(pending.join("staged")).unwrap();
    fs::create_dir_all(pending.join("backup")).unwrap();

    let error = core::create_checkpoint(&project, "Blocked", Default::default()).unwrap_err();
    assert!(matches!(error, core::CheckPoError::PendingTransaction(_)));

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 1);
    assert_eq!(result.failed_transaction_count, 0);
    assert!(!pending.exists());
    core::create_checkpoint(&project, "Unblocked", Default::default()).unwrap();
}

#[test]
fn recovery_rejects_missing_journal_when_backup_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/missingjournal");
    fs::create_dir_all(pending.join("backup/Assets/Avatar")).unwrap();
    fs::write(pending.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(pending.exists());
}

#[test]
fn recovery_rejects_missing_journal_when_staged_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/missingjournal");
    fs::create_dir_all(pending.join("backup")).unwrap();
    fs::create_dir_all(pending.join("staged/Assets/Avatar")).unwrap();
    fs::write(pending.join("staged/Assets/Avatar/Foo.prefab"), "staged").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0]
        .error
        .contains("staged files are not empty"));
    assert!(pending.exists());
}

#[test]
fn recovery_removes_unreadable_journal_when_backup_is_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/unreadablejournal");
    fs::create_dir_all(pending.join("staged")).unwrap();
    fs::create_dir_all(pending.join("backup")).unwrap();
    fs::write(pending.join("journal.json"), "not json").unwrap();

    let pending_transactions = core::pending_transactions(&project).unwrap();
    assert_eq!(pending_transactions.len(), 1);
    assert_eq!(pending_transactions[0].transaction_id, "unreadablejournal");
    assert_eq!(pending_transactions[0].state, "unreadable");

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 1);
    assert_eq!(result.failed_transaction_count, 0);
    assert!(!pending.exists());
    core::create_checkpoint(&project, "Unblocked", Default::default()).unwrap();
}

#[test]
fn recovery_rejects_unreadable_journal_when_backup_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/unreadablejournal");
    fs::create_dir_all(pending.join("backup/Assets/Avatar")).unwrap();
    fs::write(pending.join("backup/Assets/Avatar/Foo.prefab"), "backup").unwrap();
    fs::write(pending.join("journal.json"), "not json").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0]
        .error
        .contains("journal is unreadable"));
    assert!(pending.exists());
}

#[test]
fn recovery_rejects_unreadable_journal_when_staged_is_not_empty() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let pending = repo.join("journals/unreadablejournal");
    fs::create_dir_all(pending.join("backup")).unwrap();
    fs::create_dir_all(pending.join("staged/Assets/Avatar")).unwrap();
    fs::write(pending.join("staged/Assets/Avatar/Foo.prefab"), "staged").unwrap();
    fs::write(pending.join("journal.json"), "not json").unwrap();

    let result = core::recover_transactions(&project).unwrap();

    assert_eq!(result.recovered_transaction_count, 0);
    assert_eq!(result.failed_transaction_count, 1);
    assert!(result.failed_transactions[0]
        .error
        .contains("staged files are not empty"));
    assert!(pending.exists());
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
    let tx = repo.join("journals/restoretx");
    fs::create_dir_all(tx.join("staged")).unwrap();
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "restoretx",
            "state": "applying",
            "checkpointId": checkpoint,
            "kind": "restore",
            "operations": plan.operations,
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
fn recovery_does_not_restore_untracked_backup_path() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("README.md"), "keep").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let repo = view.storage_root_path.join("repos").join(view.project_id);
    let tx = repo.join("journals/badtx");
    fs::create_dir_all(tx.join("backup")).unwrap();
    fs::write(tx.join("backup/README.md"), "bad").unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "badtx",
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
    assert_eq!(result.recovered_transaction_count, 1);
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
    assert!(result.warnings.is_empty());
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(expected_latest)
    );
}

#[test]
fn delete_checkpoint_recovers_when_sqlite_index_is_incomplete_but_marked_current() {
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
    let conn = core::open_db(&repo).unwrap();
    conn.execute("DELETE FROM snapshot_entries", []).unwrap();
    conn.execute("DELETE FROM snapshots", []).unwrap();
    drop(conn);

    let result = core::delete_checkpoint(&project, deleted.as_str()).unwrap();
    let after = core::list_checkpoints(&project).unwrap();

    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("SQLite index update failed")));
    assert_eq!(
        core::read_latest_snapshot_id(&repo).unwrap(),
        Some(expected_latest.clone())
    );
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].checkpoint_id, expected_latest);
}

#[test]
fn delete_checkpoint_does_not_mark_incomplete_sqlite_index_current() {
    let (_guard, _temp, project, _data) = setup();
    let view = init_project_for_test(&project).unwrap();
    for index in 0..3 {
        fs::write(project.join("Assets/Avatar/Foo.prefab"), index.to_string()).unwrap();
        core::create_checkpoint(&project, &format!("cp{index}"), Default::default()).unwrap();
    }
    let before = core::list_checkpoints(&project).unwrap();
    let deleted = before[0].checkpoint_id.clone();
    let missing_index_row = before[2].checkpoint_id.clone();
    let repo = repo_path(&view);
    let conn = core::open_db(&repo).unwrap();
    conn.execute(
        "DELETE FROM snapshot_entries WHERE snapshot_id = ?1",
        [missing_index_row.as_str()],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM snapshots WHERE snapshot_id = ?1",
        [missing_index_row.as_str()],
    )
    .unwrap();
    drop(conn);

    let result = core::delete_checkpoint(&project, deleted.as_str()).unwrap();
    let after = core::list_checkpoints(&project).unwrap();

    assert!(result.warnings.is_empty());
    assert_eq!(after.len(), 2);
    assert!(after
        .iter()
        .any(|checkpoint| checkpoint.checkpoint_id == before[1].checkpoint_id));
    assert!(after
        .iter()
        .any(|checkpoint| checkpoint.checkpoint_id == missing_index_row));
}

#[test]
fn delete_only_checkpoint_removes_latest_ref_before_snapshot() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = init_project_for_test(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    let result = core::delete_checkpoint(&project, checkpoint.checkpoint_id.as_str()).unwrap();

    let repo = repo_path(&view);
    assert!(result.warnings.is_empty());
    assert_eq!(core::read_latest_snapshot_id(&repo).unwrap(), None);
    assert!(!core::snapshot_path(&repo, &checkpoint.checkpoint_id).exists());
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

    assert!(result.warnings.is_empty());
    assert_eq!(plan.checkpoint_count, 1);
    assert_eq!(plan.referenced_blob_count, 2);
    assert_eq!(plan.unreferenced_blob_count, 1);
    assert!(!plan.has_integrity_problems);

    let result = core::apply_gc(&project).unwrap();
    assert_eq!(result.deleted_blob_count, 1);
    let after = core::analyze_gc(&project).unwrap();
    assert_eq!(after.unreferenced_blob_count, 0);
    let diff = core::diff_checkpoint(&project, second.checkpoint_id.as_str()).unwrap();
    assert_eq!(diff.unchanged_count, 2);
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

    let copied_view = core::start_as_separate_project(&copied).unwrap();
    let copied_marker: core::ProjectMarkerFile =
        core::read_json(&copied.join(".checkpo/project.json")).unwrap();

    assert_ne!(copied_view.project_id, original.project_id);
    assert_eq!(copied_marker.project_id.as_str(), copied_view.project_id);
    assert!(repo_path(&copied_view).join("repo.json").is_file());
    assert_eq!(core::list_checkpoints(&copied).unwrap().len(), 0);
    assert_eq!(core::list_checkpoints(&project).unwrap().len(), 1);
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
        core::apply_gc(&copied).unwrap_err(),
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
    let index_db = core::db_path(&repo);
    if index_db.exists() {
        fs::remove_file(&index_db).unwrap();
    }
    let checkpoints = core::list_checkpoints(&copied).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_id, checkpoint);
    assert!(!index_db.exists());

    let tx = repo.join("journals/copiedtx");
    fs::create_dir_all(&tx).unwrap();
    fs::write(
        tx.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "copiedtx",
            "state": "created",
            "checkpointId": checkpoint,
            "kind": "restore",
            "operations": [],
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let error = core::recover_transactions(&copied).unwrap_err();
    assert_copied_project_error(&error);
    let error = core::cleanup_journals(&copied).unwrap_err();
    assert_copied_project_error(&error);
    assert!(tx.join("journal.json").is_file());

    let copied_view = core::start_as_separate_project(&copied).unwrap();
    assert_ne!(copied_view.project_id, original.project_id);
    core::create_checkpoint(&copied, "separate", Default::default()).unwrap();
}

#[cfg(unix)]
#[test]
fn stale_repository_lock_is_removed_when_pid_is_not_running() {
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

    assert!(!lock.exists());
}

#[test]
fn malformed_repository_and_registry_locks_are_reclaimed() {
    let (_guard, _temp, project, _data) = setup();
    let registry_lock = core::registry_path().unwrap().with_extension("lock");
    fs::create_dir_all(registry_lock.parent().unwrap()).unwrap();
    fs::write(&registry_lock, "not a valid lock").unwrap();

    let view = init_project_for_test(&project).unwrap();
    let repo = repo_path(&view);
    let lock = repo.join("locks/repository.lock");
    fs::write(&lock, "not a valid lock").unwrap();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();

    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();

    assert!(!registry_lock.exists());
    assert!(!lock.exists());
}
