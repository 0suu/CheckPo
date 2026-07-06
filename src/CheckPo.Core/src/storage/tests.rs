use super::*;

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
