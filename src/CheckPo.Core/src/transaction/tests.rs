use super::*;
use crate::{CreateCheckpointOptions, ProjectView};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn setup_project() -> (
    MutexGuard<'static, ()>,
    tempfile::TempDir,
    PathBuf,
    ProjectView,
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
    let view = crate::init_project(&project).unwrap();
    (guard, temp, project, view)
}

fn replace_plan(project: &Path) -> (ProjectContext, OperationPlan) {
    let checkpoint =
        crate::create_checkpoint(project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "two").unwrap();
    let context = crate::load_project(project).unwrap();
    let plan = crate::preview_discard_files(
        project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();
    (context, plan)
}

fn injected_fault(point: TransactionFaultPoint) -> CheckPoError {
    CheckPoError::Unexpected(format!("injected fault at {point:?}"))
}

#[test]
fn recovery_after_fault_immediately_after_applying_journal_keeps_original_file() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ApplyingJournalWritten {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Unexpected(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn recovery_after_fault_after_backup_move_restores_missing_project_file() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectFileBackedUp {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Unexpected(_)));
    assert!(!file.exists());
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn backup_move_refuses_existing_backup_path_without_removing_project_file() {
    let (_guard, _temp, project, view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let repo = view.storage_root_path.join("repos").join(view.project_id);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ApplyingJournalWritten {
                let backup = fs::read_dir(repo.join("journals"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path()
                    .join("backup/Assets/Avatar/Foo.prefab");
                fs::create_dir_all(backup.parent().unwrap()).unwrap();
                fs::write(&backup, "occupied").unwrap();
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn backup_copy_fallback_keeps_project_file_when_backup_hash_mismatches() {
    let (_guard, _temp, project, view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    fs::write(&file, "mutated").unwrap();
    let operation = plan.operations.first().unwrap();
    let backup = view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("journals/copyfallback/backup/Assets/Avatar/Foo.prefab");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();

    let error = backup_project_file_by_copy(
        &context,
        operation,
        &file,
        &backup,
        required_before_hash(operation).unwrap(),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "mutated");
    assert!(!backup.exists());
}

#[test]
fn capacity_check_blocks_when_required_bytes_exceed_available_space() {
    let (_guard, temp, _project, _view) = setup_project();
    let available = crate::available_space_bytes(temp.path()).unwrap();

    let error = ensure_available_space("test volume", temp.path(), available.saturating_add(1))
        .unwrap_err();

    assert!(matches!(error, CheckPoError::User(_)));
    assert!(error.to_string().contains("not enough free space"));
}

#[test]
fn capacity_estimates_count_project_growth_and_repository_staging_backup() {
    let checkpoint_id = SnapshotId::parse(&"1".repeat(64)).unwrap();
    let plan = OperationPlan::new(
        checkpoint_id,
        OperationPlanKind::Restore,
        None,
        vec![
            FileOperation {
                operation_type: FileOperationType::Restore,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Restore.prefab").unwrap(),
                before_hash: None,
                before_size_bytes: None,
                after_hash: Some(ObjectId::parse(&"2".repeat(64)).unwrap()),
                after_size_bytes: Some(10),
                after_modified_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
            },
            FileOperation {
                operation_type: FileOperationType::Replace,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Grow.prefab").unwrap(),
                before_hash: Some(ObjectId::parse(&"3".repeat(64)).unwrap()),
                before_size_bytes: Some(4),
                after_hash: Some(ObjectId::parse(&"4".repeat(64)).unwrap()),
                after_size_bytes: Some(9),
                after_modified_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
            },
            FileOperation {
                operation_type: FileOperationType::Replace,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Shrink.prefab").unwrap(),
                before_hash: Some(ObjectId::parse(&"5".repeat(64)).unwrap()),
                before_size_bytes: Some(20),
                after_hash: Some(ObjectId::parse(&"6".repeat(64)).unwrap()),
                after_size_bytes: Some(5),
                after_modified_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
            },
            FileOperation {
                operation_type: FileOperationType::Delete,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Delete.prefab").unwrap(),
                before_hash: Some(ObjectId::parse(&"7".repeat(64)).unwrap()),
                before_size_bytes: Some(100),
                after_hash: None,
                after_size_bytes: None,
                after_modified_at_utc: None,
            },
        ],
    );

    assert_eq!(estimated_project_required_bytes(&plan), 15);
    assert_eq!(plan.staged_bytes, 24);
    assert_eq!(plan.backup_bytes, 124);
    assert_eq!(estimated_repository_required_bytes(&plan), 148);
}

#[test]
fn apply_rejects_corrupt_snapshot_object_during_staging_without_touching_project() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let operation = plan.operations.first().unwrap();
    let object = crate::object_path(
        context.repo_root.as_path(),
        operation.after_hash.as_ref().unwrap(),
    );
    fs::write(&object, "corrupt").unwrap();

    let error =
        apply_plan_inner(&context, plan, ApplyOptions { yes: true }, None, None, None).unwrap_err();

    assert!(matches!(error, CheckPoError::ObjectHashMismatch(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn hardlink_backup_rechecks_quarantine_before_deleting_original() {
    let (_guard, _temp, project, view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (_context, plan) = replace_plan(&project);
    let operation = plan.operations.first().unwrap();
    let backup = view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("journals/hardlink/backup/Assets/Avatar/Foo.prefab");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();
    fs::hard_link(&file, &backup).unwrap();
    fs::remove_file(&file).unwrap();
    fs::write(&file, "mutated").unwrap();

    let error = finish_linked_backup(
        operation,
        &file,
        &backup,
        required_before_hash(operation).unwrap(),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "mutated");
    assert!(!backup.exists());
    assert!(fs::read_dir(file.parent().unwrap())
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".checkpo-")));
}

#[test]
fn recovery_after_fault_after_restore_file_rolls_replace_back_to_before_hash() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectFileRestored {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Unexpected(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn recovery_after_fault_before_committed_journal_rolls_completed_apply_back() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::OperationsAppliedBeforeCommit {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Unexpected(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[cfg(unix)]
#[test]
fn apply_rejects_staged_symlink_injected_before_restore() {
    let (_guard, temp, project, view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&file).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();
    let outside = temp.path().join("outside.prefab");
    fs::write(&outside, "one").unwrap();
    let repo = view.storage_root_path.join("repos").join(view.project_id);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ApplyingJournalWritten {
                let staged = fs::read_dir(repo.join("journals"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path()
                    .join("staged/Assets/Avatar/Foo.prefab");
                fs::remove_file(&staged).unwrap();
                std::os::unix::fs::symlink(&outside, &staged).unwrap();
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::ObjectHashMismatch(_)));
    assert!(!file.exists());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "one");
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(!file.exists());
}
