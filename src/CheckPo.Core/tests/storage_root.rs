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
    let pending = old_repo.join("journals/pendingtx");
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

    assert!(error.to_string().contains("tracked Unity folder"));
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
