use super::*;
use crate::{SnapshotContent, SnapshotEntry, TrackedUnityFilePath};

#[test]
fn open_db_sets_busy_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let conn = open_db(temp.path()).unwrap();

    let timeout_ms: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();

    assert_eq!(timeout_ms, 5000);
}

#[test]
fn move_file_no_replace_preserves_existing_destination() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::write(&source, "source").unwrap();
    fs::write(&destination, "destination").unwrap();

    let error = move_file_no_replace(&source, &destination).unwrap_err();

    assert!(matches!(error, CheckPoError::Io { .. }));
    assert_eq!(fs::read_to_string(&source).unwrap(), "source");
    assert_eq!(fs::read_to_string(&destination).unwrap(), "destination");
}

#[test]
fn reflink_or_copy_file_no_replace_preserves_existing_destination() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    fs::write(&source, "source").unwrap();
    fs::write(&destination, "destination").unwrap();

    let error = reflink_or_copy_file_no_replace(&source, &destination).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read_to_string(&source).unwrap(), "source");
    assert_eq!(fs::read_to_string(&destination).unwrap(), "destination");
}

#[test]
fn copy_object_to_file_removes_destination_on_hash_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let expected = temp.path().join("expected");
    fs::write(&expected, "expected").unwrap();
    let object_id = hash_file(&expected).unwrap();
    let object = object_path(temp.path(), &object_id);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(&object, "actual").unwrap();
    let destination = temp.path().join("staged/Assets/Avatar/Foo.prefab");

    let error = copy_object_to_file(temp.path(), &object_id, &destination, 8).unwrap_err();

    assert!(matches!(error, CheckPoError::ObjectHashMismatch(_)));
    assert!(!destination.exists());
}

#[test]
fn init_repo_layout_does_not_overwrite_invalid_existing_config() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config_path = repo.join("repo.json");
    let invalid = br#"{"schemaVersion":1,"repoFormatVersion":2}"#;
    fs::write(&config_path, invalid).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::Json { .. }
            | CheckPoError::Corruption(_)
            | CheckPoError::UnsupportedFormat { .. }
    ));
    assert_eq!(fs::read(&config_path).unwrap(), invalid);
}

#[test]
fn repository_config_future_versions_are_unsupported_and_not_rewritten() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config_path = repo.join("repo.json");
    let mut config = super::layout::default_repository_config(&project_id);
    config.schema_version = 2;
    let bytes = serde_json::to_vec(&config).unwrap();
    fs::write(&config_path, &bytes).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::UnsupportedFormat {
            artifact,
            found: 2,
            supported: 1,
        } if artifact == "repository config schema"
    ));
    assert_eq!(fs::read(&config_path).unwrap(), bytes);
}

#[test]
fn repository_config_future_schema_is_detected_before_v1_fields_are_required() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config_path = repo.join("repo.json");
    let bytes = br#"{"schemaVersion":2}"#;
    fs::write(&config_path, bytes).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::UnsupportedFormat {
            artifact,
            found: 2,
            supported: 1,
        } if artifact == "repository config schema"
    ));
    assert_eq!(fs::read(&config_path).unwrap(), bytes);
}

#[test]
fn repository_format_future_version_is_unsupported_and_not_rewritten() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config_path = repo.join("repo.json");
    let mut config = super::layout::default_repository_config(&project_id);
    config.repo_format_version = 2;
    let bytes = serde_json::to_vec(&config).unwrap();
    fs::write(&config_path, &bytes).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::UnsupportedFormat {
            artifact,
            found: 2,
            supported: 1,
        } if artifact == "repository format"
    ));
    assert_eq!(fs::read(&config_path).unwrap(), bytes);
}

#[test]
fn snapshot_case_insensitive_path_collisions_are_rejected_on_save_and_load() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let object_id = ObjectId::parse(&"0".repeat(64)).unwrap();
    let entry = |path: &str| SnapshotEntry {
        path: TrackedUnityFilePath::parse(path).unwrap(),
        size_bytes: 0,
        modified_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        content: SnapshotContent::Whole {
            hash: object_id.clone(),
            size_bytes: 0,
        },
    };
    let snapshot = SnapshotFile {
        schema_version: 1,
        project_id,
        parent_snapshot_id: None,
        created_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        name: "collision".to_string(),
        tool_version: "test".to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files: vec![entry("Assets/Foo.asset"), entry("Assets/foo.asset")],
    };

    let error = save_snapshot(temp.path(), &snapshot).unwrap_err();
    assert!(
        matches!(error, CheckPoError::Corruption(message) if message.contains("case/Unicode-normalization-insensitive"))
    );

    let bytes = canonical_snapshot_bytes(&snapshot).unwrap();
    let snapshot_id = snapshot_id_from_bytes(&bytes);
    let path = snapshot_path(temp.path(), &snapshot_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    let error = load_snapshot(temp.path(), &snapshot_id).unwrap_err();
    assert!(
        matches!(error, CheckPoError::Corruption(message) if message.contains("case/Unicode-normalization-insensitive"))
    );
}

#[test]
fn snapshot_unicode_normalization_path_collisions_are_rejected_without_rewriting_paths() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let object_id = ObjectId::parse(&"0".repeat(64)).unwrap();
    let entry = |path: &str| SnapshotEntry {
        path: TrackedUnityFilePath::parse(path).unwrap(),
        size_bytes: 0,
        modified_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        content: SnapshotContent::Whole {
            hash: object_id.clone(),
            size_bytes: 0,
        },
    };
    let decomposed = "Assets/cafe\u{301}.asset";
    let composed = "Assets/caf\u{e9}.asset";
    let mut files = vec![entry(composed), entry(decomposed)];
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let snapshot = SnapshotFile {
        schema_version: 1,
        project_id,
        parent_snapshot_id: None,
        created_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        name: "unicode collision".to_string(),
        tool_version: "test".to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files,
    };

    let error = save_snapshot(temp.path(), &snapshot).unwrap_err();
    assert!(matches!(
        error,
        CheckPoError::Corruption(message) if message.contains("Unicode-normalization-insensitive")
    ));

    let single = SnapshotFile {
        files: vec![entry(decomposed)],
        name: "single decomposed path".to_string(),
        ..snapshot
    };
    let id = save_snapshot(temp.path(), &single).unwrap();
    let loaded = load_snapshot(temp.path(), &id).unwrap();
    assert_eq!(loaded.files[0].path.as_str(), decomposed);
}

#[test]
fn snapshot_file_directory_ancestor_conflicts_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let object_id = ObjectId::parse(&"0".repeat(64)).unwrap();
    let entry = |path: &str| SnapshotEntry {
        path: TrackedUnityFilePath::parse(path).unwrap(),
        size_bytes: 0,
        modified_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        content: SnapshotContent::Whole {
            hash: object_id.clone(),
            size_bytes: 0,
        },
    };
    let snapshot = SnapshotFile {
        schema_version: 1,
        project_id,
        parent_snapshot_id: None,
        created_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        name: "ancestor conflict".to_string(),
        tool_version: "test".to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files: vec![entry("Assets/Foo"), entry("Assets/Foo/Bar.asset")],
    };

    let error = save_snapshot(temp.path(), &snapshot).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::Corruption(message) if message.contains("ancestor conflict")
    ));
}

#[test]
fn init_repo_layout_leaves_valid_existing_config_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config_path = repo.join("repo.json");
    let pretty =
        serde_json::to_vec_pretty(&super::layout::default_repository_config(&project_id)).unwrap();
    fs::write(&config_path, &pretty).unwrap();
    let original_mtime = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_mtime(&config_path, original_mtime).unwrap();

    let reopened = init_repo_layout(temp.path(), &project_id).unwrap();

    assert_eq!(reopened, repo);
    assert_eq!(fs::read(&config_path).unwrap(), pretty);
    assert_eq!(
        filetime::FileTime::from_last_modification_time(&fs::metadata(config_path).unwrap()),
        original_mtime
    );
}

#[cfg(unix)]
#[test]
fn copy_object_to_file_rejects_symlink_object() {
    let temp = tempfile::tempdir().unwrap();
    let object_id = ObjectId::parse(&"0".repeat(64)).unwrap();
    let object = object_path(temp.path(), &object_id);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, &object).unwrap();
    let destination = temp.path().join("staged/Assets/Avatar/Foo.prefab");

    let error = copy_object_to_file(temp.path(), &object_id, &destination, 6).unwrap_err();

    assert!(matches!(error, CheckPoError::ObjectHashMismatch(_)));
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn object_store_rejects_symlink_shard_parent_for_read_and_write() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let loose = repo.join("objects/loose");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&loose).unwrap();
    fs::create_dir(&outside).unwrap();
    let source = temp.path().join("source");
    fs::write(&source, "content").unwrap();
    let object_id = hash_file(&source).unwrap();
    let first = &object_id.as_str()[..2];
    let second = &object_id.as_str()[2..4];
    std::os::unix::fs::symlink(&outside, loose.join(first)).unwrap();
    fs::create_dir_all(outside.join(second)).unwrap();
    fs::write(outside.join(second).join(object_id.as_str()), "content").unwrap();

    let read_error =
        copy_object_to_file(&repo, &object_id, &temp.path().join("destination"), 7).unwrap_err();
    let write_error =
        put_object_from_file_with_known_hash(&repo, &source, &object_id, 7).unwrap_err();

    assert!(matches!(read_error, CheckPoError::Corruption(_)));
    assert!(matches!(write_error, CheckPoError::Corruption(_)));
    assert_eq!(
        fs::read_to_string(outside.join(second).join(object_id.as_str())).unwrap(),
        "content"
    );
}
