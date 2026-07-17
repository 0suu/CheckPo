use checkpo_core as core;
use std::fs::{self, File, OpenOptions};
use std::sync::{Mutex, MutexGuard, OnceLock};

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

#[cfg(windows)]
fn hold_os_lock(path: &std::path::Path) -> File {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
        .unwrap()
}

#[cfg(unix)]
fn hold_os_lock(path: &std::path::Path) -> File {
    use std::os::fd::AsRawFd;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
    file
}

#[test]
fn set_project_storage_root_uses_manually_moved_repository() {
    let (_guard, _temp, project, data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = core::init_project(&project).unwrap();
    let summary = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let new_storage = data.join("new-store");
    let new_repo = new_storage.join("repos").join(&view.project_id);
    fs::create_dir_all(new_repo.parent().unwrap()).unwrap();
    fs::rename(&old_repo, &new_repo).unwrap();

    let updated = core::set_project_storage_root(&project, &new_storage).unwrap();

    assert_eq!(
        updated.storage_root_path,
        new_storage.canonicalize().unwrap()
    );
    assert_eq!(
        core::list_checkpoints(&project).unwrap()[0].checkpoint_id,
        summary.checkpoint_id
    );
}

#[test]
fn set_project_storage_root_recovers_prepared_checkpoint_create_journal() {
    let (_guard, _temp, project, data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = core::init_project(&project).unwrap();
    let first = core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let new_storage = data.join("new-store");
    let new_repo = new_storage.join("repos").join(&view.project_id);
    copy_dir(&old_repo, &new_repo);
    let transaction_id = "11111111111111111111111111111111";
    let journal_root = new_repo
        .join("journals/checkpoint-create")
        .join(transaction_id);
    let inventory_head_before = fs::read_to_string(new_repo.join("inventory/snapshots/head"))
        .unwrap()
        .trim()
        .to_string();
    fs::create_dir_all(&journal_root).unwrap();
    fs::write(
        journal_root.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 3,
            "transactionId": transaction_id,
            "state": "prepared",
            "checkpointId": "a".repeat(64),
            "expectedOldLatest": first.checkpoint_id,
            "inventoryHeadBefore": inventory_head_before,
            "createdAtUtc": "2026-01-01T00:00:00Z",
            "updatedAtUtc": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let updated = core::set_project_storage_root(&project, &new_storage).unwrap();

    assert_eq!(
        updated.storage_root_path,
        new_storage.canonicalize().unwrap()
    );
    assert!(!journal_root.exists());
    assert_eq!(
        core::list_checkpoints(&project).unwrap()[0].checkpoint_id,
        first.checkpoint_id
    );
}

#[test]
fn set_project_storage_root_recovers_staged_checkpoint_delete_journal() {
    let (_guard, _temp, project, data) = setup();
    let tracked = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&tracked, "one").unwrap();
    let view = core::init_project(&project).unwrap();
    let first = core::create_checkpoint(&project, "First", Default::default()).unwrap();
    fs::write(&tracked, "two").unwrap();
    let second = core::create_checkpoint(&project, "Second", Default::default()).unwrap();
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let new_storage = data.join("new-store");
    let new_repo = new_storage.join("repos").join(&view.project_id);
    copy_dir(&old_repo, &new_repo);
    let transaction_id = "22222222222222222222222222222222";
    let transaction_root = new_repo
        .join("journals/checkpoint-delete")
        .join(transaction_id);
    let inventory_head_before = fs::read_to_string(new_repo.join("inventory/snapshots/head"))
        .unwrap()
        .trim()
        .to_string();
    fs::create_dir_all(&transaction_root).unwrap();
    fs::rename(
        core::snapshot_path(&new_repo, &second.checkpoint_id),
        transaction_root.join("snapshot.root"),
    )
    .unwrap();
    fs::write(
        transaction_root.join("journal.json"),
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

    core::set_project_storage_root(&project, &new_storage).unwrap();

    assert!(!transaction_root.exists());
    let checkpoints = core::list_checkpoints(&project).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_id, first.checkpoint_id);
    assert!(core::verify_project(&project, false).unwrap().is_valid);
}

#[test]
fn portable_repository_set_roundtrips_without_local_indexes_or_journals() {
    let (_guard, _temp, project, data) = setup();
    let tracked = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&tracked, "one").unwrap();
    let view = core::init_project(&project).unwrap();
    let first = core::create_checkpoint(&project, "first", Default::default()).unwrap();
    fs::write(&tracked, "two").unwrap();
    let second = core::create_checkpoint(&project, "second", Default::default()).unwrap();
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let new_storage = data.join("portable-restore");
    let new_repo = new_storage.join("repos").join(&view.project_id);
    fs::create_dir_all(&new_repo).unwrap();
    fs::copy(old_repo.join("repo.json"), new_repo.join("repo.json")).unwrap();
    for relative in ["refs", "inventory", "snapshots", "manifests", "objects"] {
        copy_dir(&old_repo.join(relative), &new_repo.join(relative));
    }
    for relative in ["indexes", "journals/transactions", "tmp", "locks"] {
        fs::create_dir_all(new_repo.join(relative)).unwrap();
    }
    fs::remove_dir_all(&old_repo).unwrap();

    core::set_project_storage_root(&project, &new_storage).unwrap();
    core::rebuild_index(&project).unwrap();

    let checkpoints = core::list_checkpoints(&project).unwrap();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].checkpoint_id, second.checkpoint_id);
    assert!(core::verify_project(&project, false).unwrap().is_valid);
    assert!(core::verify_project(&project, true).unwrap().is_valid);
    let clean = core::diff_checkpoint(&project, second.checkpoint_id.as_str()).unwrap();
    assert!(clean.added.is_empty() && clean.modified.is_empty() && clean.deleted.is_empty());

    fs::write(&tracked, "changed after import").unwrap();
    let restore = core::preview_restore(&project, first.checkpoint_id.as_str()).unwrap();
    core::apply_restore_plan(
        &project,
        first.checkpoint_id.as_str(),
        restore,
        core::ApplyOptions { yes: true },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&tracked).unwrap(), "one");
    let gc = core::analyze_gc(&project).unwrap();
    assert!(!gc.has_integrity_problems);
}

#[cfg(any(unix, windows))]
#[test]
#[ignore = "requires CHECKPO_CROSS_DEVICE_TEST_ROOT on a different volume"]
fn cross_device_restore_publishes_verified_staged_content() {
    let external_root = std::env::var_os("CHECKPO_CROSS_DEVICE_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .expect("CHECKPO_CROSS_DEVICE_TEST_ROOT is required");
    let external = tempfile::Builder::new()
        .prefix("checkpo-cross-device-")
        .tempdir_in(external_root)
        .unwrap();
    let storage = external.path().join("storage");
    let (_guard, _temp, project, _data) = setup();
    fs::create_dir_all(&storage).unwrap();

    let probe_source = storage.join("volume-probe");
    let probe_destination = project.join("volume-probe");
    fs::write(&probe_source, b"probe").unwrap();
    let rename_error = fs::rename(&probe_source, &probe_destination)
        .expect_err("test roots must be on different volumes");
    #[cfg(unix)]
    assert_eq!(rename_error.raw_os_error(), Some(libc::EXDEV));
    #[cfg(windows)]
    assert_eq!(rename_error.raw_os_error(), Some(17));

    let tracked = project.join("Assets/Avatar/Large.asset");
    let expected = vec![0x5a_u8; 16 * 1024 * 1024];
    fs::write(&tracked, &expected).unwrap();
    core::init_project_with_storage_root(&project, &storage).unwrap();
    let checkpoint = core::create_checkpoint(&project, "cross-device", Default::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_file(&tracked).unwrap();

    let plan = core::preview_restore(&project, checkpoint.as_str()).unwrap();
    core::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(fs::read(&tracked).unwrap(), expected);
    let diff = core::diff_checkpoint(&project, checkpoint.as_str()).unwrap();
    assert!(diff.added.is_empty() && diff.modified.is_empty() && diff.deleted.is_empty());
}

#[test]
fn set_project_storage_root_locks_old_repository_when_copy_exists() {
    let (_guard, _temp, project, data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = core::init_project(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let new_storage = data.join("new-store");
    let new_repo = new_storage.join("repos").join(&view.project_id);
    copy_dir(&old_repo, &new_repo);
    let lock_dir = old_repo.join("locks");
    fs::create_dir_all(&lock_dir).unwrap();
    let _held_lock = hold_os_lock(&lock_dir.join("repository.lock"));

    let error = core::set_project_storage_root(&project, &new_storage).unwrap_err();

    assert!(matches!(error, core::CheckPoError::RepositoryLocked(_)));
    assert_eq!(
        core::load_project_view(&project)
            .unwrap()
            .storage_root_path
            .canonicalize()
            .unwrap(),
        view.storage_root_path.canonicalize().unwrap()
    );
}

#[test]
fn set_project_storage_root_rejects_old_repository_pending_transaction_when_copy_exists() {
    let (_guard, _temp, project, data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = core::init_project(&project).unwrap();
    let checkpoint = core::create_checkpoint(&project, "Initial", Default::default())
        .unwrap()
        .checkpoint_id;
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let new_storage = data.join("new-store");
    let new_repo = new_storage.join("repos").join(&view.project_id);
    copy_dir(&old_repo, &new_repo);
    let pending = old_repo.join("journals/transactions/pendingtx");
    fs::create_dir_all(&pending).unwrap();
    fs::write(
        pending.join("journal.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "transactionId": "pendingtx",
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

    let error = core::set_project_storage_root(&project, &new_storage).unwrap_err();

    assert!(matches!(error, core::CheckPoError::PendingTransaction(_)));
    assert_eq!(
        core::load_project_view(&project)
            .unwrap()
            .storage_root_path
            .canonicalize()
            .unwrap(),
        view.storage_root_path.canonicalize().unwrap()
    );
}

#[test]
fn init_project_uses_custom_storage_root_for_new_project() {
    let (_guard, _temp, project, data) = setup();
    let custom_storage = data.join("custom-store");

    let view = core::init_project_with_storage_root(&project, &custom_storage).unwrap();

    assert_eq!(
        view.storage_root_path.canonicalize().unwrap(),
        custom_storage.canonicalize().unwrap()
    );
    assert!(custom_storage.join("repos").join(&view.project_id).is_dir());
}

#[test]
fn init_rejects_explicit_storage_root_that_conflicts_with_registry() {
    let (_guard, temp, project, _data) = setup();
    let original_storage = temp.path().join("storage-a");
    let conflicting_storage = temp.path().join("storage-b");
    fs::create_dir_all(&original_storage).unwrap();
    fs::create_dir_all(&conflicting_storage).unwrap();
    let original = core::init_project_with_storage_root(&project, &original_storage).unwrap();

    let error = core::init_project_with_storage_root(&project, &conflicting_storage).unwrap_err();

    assert!(matches!(
        error,
        core::CheckPoError::StorageRootConflict { .. }
    ));
    let loaded = core::load_project_view(&project).unwrap();
    assert_eq!(loaded.storage_root_path, original.storage_root_path);
}

#[test]
fn start_as_separate_project_uses_custom_storage_root() {
    let (_guard, temp, project, data) = setup();
    let original = core::init_project(&project).unwrap();
    let copied = temp.path().join("UnityProjectCopy");
    copy_dir(&project, &copied);
    assert_eq!(
        core::load_project_view(&copied).unwrap().location_status,
        core::ProjectLocationStatus::CopiedSuspected
    );
    let custom_storage = data.join("separate-store");

    let view = core::start_as_separate_project_with_storage_root(
        &copied,
        &custom_storage,
        core::ApplyOptions { yes: true },
    )
    .unwrap();

    assert_ne!(view.project_id, original.project_id);
    assert_eq!(
        view.storage_root_path.canonicalize().unwrap(),
        custom_storage.canonicalize().unwrap()
    );
    assert!(custom_storage.join("repos").join(&view.project_id).is_dir());
}

#[test]
fn start_as_separate_requires_copied_project_and_confirmation_without_side_effects() {
    let (_guard, temp, project, _data) = setup();
    let original = core::init_project(&project).unwrap();
    let original_marker = fs::read(project.join(".checkpo/project.json")).unwrap();

    let error =
        core::start_as_separate_project(&project, core::ApplyOptions { yes: true }).unwrap_err();
    assert!(error
        .to_string()
        .contains("only allowed for a copied project"));
    assert_eq!(
        fs::read(project.join(".checkpo/project.json")).unwrap(),
        original_marker
    );

    let copied = temp.path().join("UnityProjectCopy");
    copy_dir(&project, &copied);
    let copied_marker = fs::read(copied.join(".checkpo/project.json")).unwrap();
    let error =
        core::start_as_separate_project(&copied, core::ApplyOptions { yes: false }).unwrap_err();
    assert!(error.to_string().contains("requires --yes"));
    assert_eq!(
        fs::read(copied.join(".checkpo/project.json")).unwrap(),
        copied_marker
    );
    assert_eq!(
        core::load_project_view(&copied).unwrap().project_id,
        original.project_id
    );
}

#[test]
fn set_project_storage_root_rejects_folder_without_moved_repository() {
    let (_guard, _temp, project, data) = setup();
    core::init_project(&project).unwrap();
    let new_storage = data.join("empty-store");
    fs::create_dir_all(&new_storage).unwrap();

    let error = core::set_project_storage_root(&project, &new_storage).unwrap_err();

    assert!(error.to_string().contains("Move the existing repository"));
    assert_ne!(
        core::load_project_view(&project).unwrap().storage_root_path,
        new_storage.canonicalize().unwrap()
    );
}

#[test]
fn set_project_storage_root_rejects_repository_inside_tracked_folder() {
    let (_guard, _temp, project, _data) = setup();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let view = core::init_project(&project).unwrap();
    core::create_checkpoint(&project, "Initial", Default::default()).unwrap();
    let old_repo = view.storage_root_path.join("repos").join(&view.project_id);
    let dangerous_storage = project.join("Assets/CheckpointStorage");
    let dangerous_repo = dangerous_storage.join("repos").join(&view.project_id);
    fs::create_dir_all(dangerous_repo.parent().unwrap()).unwrap();
    fs::rename(&old_repo, &dangerous_repo).unwrap();

    let error = core::set_project_storage_root(&project, &dangerous_storage).unwrap_err();

    assert!(error.to_string().contains("inside the Unity project"));
}

#[test]
fn init_rejects_repository_anywhere_inside_unity_project() {
    let (_guard, _temp, project, _data) = setup();
    for relative in ["Library/CheckPo", "Temp/CheckPo", ".checkpo/storage"] {
        let storage = project.join(relative);
        fs::create_dir_all(&storage).unwrap();
        let error = core::init_project_with_storage_root(&project, &storage).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must not be inside the Unity project"),
            "{relative}: {error}"
        );
        assert!(!project.join(".checkpo/project.json").exists());
    }
}

fn copy_dir(source: &std::path::Path, destination: &std::path::Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir(&source_path, &destination_path);
        } else {
            fs::copy(&source_path, &destination_path).unwrap();
        }
    }
}
