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

#[cfg(unix)]
#[test]
fn fingerprint_database_open_rejects_leaf_symlink_without_touching_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("indexes")).unwrap();
    let target = temp.path().join("outside.db");
    fs::write(&target, b"outside-must-not-change").unwrap();
    let cache = file_fingerprint_db_path(&repo);
    symlink(&target, &cache).unwrap();
    let before = fs::read(&target).unwrap();

    assert!(open_file_fingerprint_db(&repo).is_err());
    assert!(remove_file_fingerprint_db_if_exists(&repo).is_err());
    assert_eq!(fs::read(&target).unwrap(), before);
    assert!(fs::symlink_metadata(&cache)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn fingerprint_database_removal_is_identity_bound() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("indexes")).unwrap();
    let cache = file_fingerprint_db_path(&repo);
    fs::write(&cache, b"cache").unwrap();

    remove_file_fingerprint_db_if_exists(&repo).unwrap();

    assert!(!cache.exists());
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
    config.schema_version = 3;
    let bytes = serde_json::to_vec(&config).unwrap();
    fs::write(&config_path, &bytes).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::UnsupportedFormat {
            artifact,
            found: 3,
            supported: 2,
        } if artifact == "repository config schema"
    ));
    assert_eq!(fs::read(&config_path).unwrap(), bytes);
}

#[test]
fn repository_config_future_schema_is_detected_before_v2_fields_are_required() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config_path = repo.join("repo.json");
    let bytes = br#"{"schemaVersion":3}"#;
    fs::write(&config_path, bytes).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::UnsupportedFormat {
            artifact,
            found: 3,
            supported: 2,
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
    config.repo_format_version = 6;
    let bytes = serde_json::to_vec(&config).unwrap();
    fs::write(&config_path, &bytes).unwrap();

    let error = init_repo_layout(temp.path(), &project_id).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::UnsupportedFormat {
            artifact,
            found: 6,
            supported: 5,
        } if artifact == "repository format"
    ));
    assert_eq!(fs::read(&config_path).unwrap(), bytes);
}

#[test]
fn repository_v5_uses_one_level_content_addressed_shards_and_root_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let project_id = ProjectId::parse("11111111111111111111111111111111").unwrap();
    let repo = init_repo_layout(temp.path(), &project_id).unwrap();
    let config = super::layout::default_repository_config(&project_id);
    assert_eq!(config.repo_format_version, 5);
    assert_eq!(config.object_format, "loose-whole-file-one-level-v2");
    assert_eq!(
        config.manifest_storage_format,
        "loose-content-addressed-one-level-v2"
    );
    assert_eq!(
        validate_physical_snapshot_inventory(&repo, &project_id).unwrap(),
        Vec::<SnapshotId>::new()
    );
    assert_eq!(inventory_head_id(&repo, &project_id).unwrap().len(), 64);

    let object_id_text = format!("ab{}", "0".repeat(62));
    let object_id = ObjectId::parse(&object_id_text).unwrap();
    let path = object_path(&repo, &object_id);
    let relative = path.strip_prefix(&repo).unwrap();
    assert_eq!(
        relative,
        Path::new("objects/loose/ab").join(object_id.as_str())
    );
    assert_eq!(
        super::layout::object_id_from_loose_relative_path(relative).unwrap(),
        object_id
    );
    assert!(super::layout::object_id_from_loose_relative_path(
        &Path::new("objects/loose/ab/00").join(object_id.as_str())
    )
    .is_err());
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
        schema_version: 2,
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
        schema_version: 2,
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
        schema_version: 2,
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

#[cfg(unix)]
#[test]
fn snapshot_load_rejects_leaf_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot = SnapshotFile {
        schema_version: 2,
        project_id: ProjectId::parse("11111111111111111111111111111111").unwrap(),
        parent_snapshot_id: None,
        created_at_utc: "2026-01-01T00:00:00.000000000Z".to_string(),
        name: "symlink".to_string(),
        tool_version: "test".to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files: Vec::new(),
    };
    let bytes = canonical_snapshot_bytes(&snapshot).unwrap();
    let snapshot_id = snapshot_id_from_bytes(&bytes);
    let path = snapshot_path(temp.path(), &snapshot_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let outside = temp.path().join("outside.json");
    fs::write(&outside, bytes).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();

    let error = load_snapshot(temp.path(), &snapshot_id).unwrap_err();

    assert!(matches!(error, CheckPoError::Corruption(message) if message.contains("no-follow")));
}

#[test]
fn snapshot_load_rejects_file_over_size_limit_before_reading_it() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot_id = SnapshotId::parse(&"0".repeat(64)).unwrap();
    let path = snapshot_path(temp.path(), &snapshot_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::File::create(&path).unwrap();
    file.set_len(super::snapshot_store::MAX_SNAPSHOT_FILE_BYTES + 1)
        .unwrap();

    let error = load_snapshot(temp.path(), &snapshot_id).unwrap_err();

    assert!(
        matches!(error, CheckPoError::Corruption(message) if message.contains("exceeds maximum size"))
    );
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
    std::os::unix::fs::symlink(&outside, loose.join(first)).unwrap();
    fs::write(outside.join(object_id.as_str()), "content").unwrap();

    let read_error =
        copy_object_to_file(&repo, &object_id, &temp.path().join("destination"), 7).unwrap_err();
    let write_error =
        put_object_from_file_with_known_hash(&repo, &source, &object_id, 7).unwrap_err();

    assert!(matches!(read_error, CheckPoError::Corruption(_)));
    assert!(matches!(write_error, CheckPoError::Corruption(_)));
    assert_eq!(
        fs::read_to_string(outside.join(object_id.as_str())).unwrap(),
        "content"
    );
}

#[test]
fn known_hash_object_store_cleans_temp_file_on_hash_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("objects/loose")).unwrap();
    fs::create_dir_all(repo.join("tmp")).unwrap();
    let source = temp.path().join("source");
    fs::write(&source, "one").unwrap();
    let wrong_hash = ObjectId::parse(&"0".repeat(64)).unwrap();

    let error = put_object_from_file_with_known_hash(&repo, &source, &wrong_hash, 3).unwrap_err();

    assert!(matches!(error, CheckPoError::ObjectHashMismatch(_)));
    assert!(fs::read_dir(repo.join("tmp")).unwrap().next().is_none());
    let shard = object_path(&repo, &wrong_hash)
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(fs::read_dir(shard).unwrap().next().is_none());
}
