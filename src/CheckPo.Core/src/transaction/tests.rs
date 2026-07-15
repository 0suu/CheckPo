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
    let guard = TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    let interrupted_copy =
        transaction_root.join("backup/Assets/Avatar/.checkpo-0123456789abcdef0123456789abcdef.tmp");
    fs::create_dir_all(interrupted_copy.parent().unwrap()).unwrap();
    fs::write(&interrupted_copy, "partial backup").unwrap();
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn recovery_removes_transaction_owned_project_temp_left_before_publish() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&file).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
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

    let pending = crate::pending_transactions(&project).unwrap();
    let transaction_id = pending[0].transaction_id.clone();
    let tracked = TrackedUnityFilePath::parse("Assets/Avatar/Foo.prefab").unwrap();
    let project_temp =
        transaction_materialization_temp_path(&file, &tracked, &transaction_id).unwrap();
    fs::write(&project_temp, "partial").unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(!file.exists());
    assert!(!project_temp.exists());
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
    let second = crate::recover_transactions(&project).unwrap();
    assert_eq!(second.recovered_transaction_count, 0);
}

#[cfg(unix)]
#[test]
fn recovery_rejects_transaction_owned_project_temp_symlink_without_touching_target() {
    use std::os::unix::fs::symlink;

    let (_guard, temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&file).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
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

    let pending = crate::pending_transactions(&project).unwrap();
    let tracked = TrackedUnityFilePath::parse("Assets/Avatar/Foo.prefab").unwrap();
    let project_temp =
        transaction_materialization_temp_path(&file, &tracked, &pending[0].transaction_id).unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, "do not touch").unwrap();
    symlink(&outside, &project_temp).unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 0);
    assert_eq!(recovered.failed_transaction_count, 1);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "do not touch");
}

#[test]
fn recovery_rolls_back_published_restore_when_staged_source_still_exists() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&file).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
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

    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    let staged = transaction_root.join("staged/Assets/Avatar/Foo.prefab");
    fs::hard_link(&staged, &file).unwrap();
    crate::sync_parent_dir(&file).unwrap();
    assert!(staged.exists());
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(!file.exists());
    assert!(!staged.exists());
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
    let second = crate::recover_transactions(&project).unwrap();
    assert_eq!(second.recovered_transaction_count, 0);
    crate::create_checkpoint(&project, "unblocked", CreateCheckpointOptions::default()).unwrap();
}

#[test]
fn restore_directory_barrier_and_staged_cleanup_faults_recover_whole_before_state() {
    for fault_point in [
        TransactionFaultPoint::ProjectRestoreDirectoryBarrierBefore,
        TransactionFaultPoint::ProjectRestoreDirectoryBarrierAfter,
        TransactionFaultPoint::StagedPayloadCleanupBefore,
        TransactionFaultPoint::StagedPayloadCleanupAfter,
    ] {
        let (_guard, _temp, project, _view) = setup_project();
        let file = project.join("Assets/Avatar/Foo.prefab");
        fs::write(&file, "one").unwrap();
        let checkpoint =
            crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
                .unwrap()
                .checkpoint_id;
        fs::remove_file(&file).unwrap();
        let context = crate::load_project(&project).unwrap();
        let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

        let error = apply_plan_inner(
            &context,
            plan,
            ApplyOptions { yes: true },
            None,
            None,
            Some(&|point| {
                if point == fault_point {
                    return Err(injected_fault(point));
                }
                Ok(())
            }),
        )
        .unwrap_err();

        assert!(matches!(error, CheckPoError::Unexpected(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), "one");
        let recovered = crate::recover_transactions(&project).unwrap();
        assert_eq!(
            recovered.recovered_transaction_count, 1,
            "fault point: {fault_point:?}; failures: {:?}",
            recovered.failed_transactions
        );
        assert_eq!(recovered.failed_transaction_count, 0);
        assert!(!file.exists(), "fault point: {fault_point:?}");
        assert!(crate::pending_transactions(&project).unwrap().is_empty());
    }
}

#[test]
fn restore_fault_after_first_publish_rolls_back_all_paths() {
    use std::cell::Cell;

    let (_guard, _temp, project, _view) = setup_project();
    let first = project.join("Assets/Avatar/First.prefab");
    let second = project.join("Assets/Avatar/Second.prefab");
    fs::write(&first, "first").unwrap();
    fs::write(&second, "second").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&first).unwrap();
    fs::remove_file(&second).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();
    let restored_count = Cell::new(0_usize);

    apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectFileRestored {
                let current = restored_count.get() + 1;
                restored_count.set(current);
                if current == 1 {
                    return Err(injected_fault(point));
                }
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert_eq!(restored_count.get(), 1);
    assert_eq!(
        usize::from(first.exists()) + usize::from(second.exists()),
        1
    );
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(!first.exists());
    assert!(!second.exists());
}

#[cfg(unix)]
#[test]
fn successful_restore_never_hard_links_project_file_to_cas_object() {
    use std::os::unix::fs::MetadataExt;

    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&file).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();
    let object_id = plan.operations[0].after_hash.clone().unwrap();

    let applied =
        apply_plan_inner(&context, plan, ApplyOptions { yes: true }, None, None, None).unwrap();

    let object = crate::object_path(
        &view.storage_root_path.join("repos").join(view.project_id),
        &object_id,
    );
    let object_metadata = fs::metadata(object).unwrap();
    let project_metadata = fs::metadata(&file).unwrap();
    assert_ne!(
        (object_metadata.dev(), object_metadata.ino()),
        (project_metadata.dev(), project_metadata.ino())
    );
    let transaction_root = applied
        .journal_path
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(!transaction_root
        .join("staged/Assets/Avatar/Foo.prefab")
        .exists());
}

#[test]
fn recovery_rejects_near_match_atomic_copy_temporary_file() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    apply_plan_inner(
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

    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    let near_match =
        transaction_root.join("backup/Assets/Avatar/.checkpo-0123456789abcdef0123456789abcdeF.tmp");
    fs::create_dir_all(near_match.parent().unwrap()).unwrap();
    fs::write(&near_match, "not owned").unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 0);
    assert_eq!(recovered.failed_transaction_count, 1);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[cfg(unix)]
#[test]
fn recovery_rejects_atomic_copy_temporary_symlink() {
    let (_guard, temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    apply_plan_inner(
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

    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    let outside = temp.path().join("outside");
    fs::write(&outside, "do not touch").unwrap();
    let temporary =
        transaction_root.join("backup/Assets/Avatar/.checkpo-0123456789abcdef0123456789abcdef.tmp");
    fs::create_dir_all(temporary.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &temporary).unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 0);
    assert_eq!(recovered.failed_transaction_count, 1);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "do not touch");
}

#[test]
fn corrupt_snapshot_transaction_can_be_quarantined_without_deleting_payload() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let checkpoint_id = plan.checkpoint_id.clone();

    apply_plan_inner(
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

    fs::write(
        crate::snapshot_path(&context.repo_root, &checkpoint_id),
        b"corrupt",
    )
    .unwrap();
    let recovery = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovery.failed_transaction_count, 1);
    let pending = crate::pending_transactions(&project).unwrap();
    assert_eq!(pending.len(), 1);

    let denied = crate::quarantine_transaction(
        &project,
        &pending[0].transaction_id,
        ApplyOptions { yes: false },
    )
    .unwrap_err();
    assert!(matches!(denied, CheckPoError::User(_)));

    let result = crate::quarantine_transaction(
        &project,
        &pending[0].transaction_id,
        ApplyOptions { yes: true },
    )
    .unwrap();
    assert!(result.quarantine_path.is_dir());
    assert!(result.quarantine_path.join("journal.json").is_file());
    assert!(result.quarantine_path.join("staged").is_dir());
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
}

#[test]
fn committed_transaction_cannot_be_quarantined() {
    let (_guard, _temp, project, _view) = setup_project();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let applied =
        apply_plan_inner(&context, plan, ApplyOptions { yes: true }, None, None, None).unwrap();
    let transaction_id = applied.transaction_id.unwrap();
    let transaction_root = context
        .repo_root
        .join("journals/transactions")
        .join(&transaction_id);

    let error =
        crate::quarantine_transaction(&project, &transaction_id, ApplyOptions { yes: true })
            .unwrap_err();

    assert!(matches!(error, CheckPoError::User(_)));
    assert!(transaction_root.join("journal.json").is_file());
}

#[test]
fn unverified_quarantine_blocks_mutation_until_full_restore_succeeds() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let checkpoint_id = plan.checkpoint_id.clone();

    apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::BackupSourceCleanupAfter {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();
    assert!(!file.exists());

    let pending = crate::pending_transactions(&project).unwrap();
    assert_eq!(pending.len(), 1);
    crate::quarantine_transaction(
        &project,
        &pending[0].transaction_id,
        ApplyOptions { yes: true },
    )
    .unwrap();

    let unresolved = crate::unresolved_transaction_quarantines(&project).unwrap();
    assert_eq!(unresolved.len(), 1);
    let blocked = crate::create_checkpoint(
        &project,
        "must be blocked",
        CreateCheckpointOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(
        blocked,
        CheckPoError::UnresolvedTransactionQuarantine(_)
    ));

    let record_path = fs::read_dir(context.repo_root.join("quarantined-journals"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .unwrap();
    fs::write(&record_path, "corrupt quarantine record").unwrap();
    assert_eq!(
        crate::unresolved_transaction_quarantines(&project)
            .unwrap()
            .len(),
        1
    );

    let restore_plan = crate::preview_restore(&project, checkpoint_id.as_str()).unwrap();
    crate::apply_restore_plan(
        &project,
        checkpoint_id.as_str(),
        restore_plan,
        ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    assert!(crate::unresolved_transaction_quarantines(&project)
        .unwrap()
        .is_empty());
    crate::create_checkpoint(&project, "unblocked", CreateCheckpointOptions::default()).unwrap();
}

#[test]
fn orphan_quarantine_payload_blocks_mutation_until_full_restore() {
    let (_guard, _temp, project, view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "known good", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::write(&file, "two").unwrap();
    let quarantine = view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("quarantined-journals/orphan-payload");
    fs::create_dir_all(&quarantine).unwrap();

    let unresolved = crate::unresolved_transaction_quarantines(&project).unwrap();

    assert_eq!(unresolved.len(), 1);
    assert!(unresolved[0].reason.contains("no matching record"));
    assert!(matches!(
        crate::create_checkpoint(&project, "blocked", CreateCheckpointOptions::default())
            .unwrap_err(),
        CheckPoError::UnresolvedTransactionQuarantine(_)
    ));

    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();
    crate::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        ApplyOptions { yes: true },
    )
    .unwrap();

    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    assert!(crate::unresolved_transaction_quarantines(&project)
        .unwrap()
        .is_empty());
}

#[test]
fn zero_change_full_restore_resolves_quarantine() {
    let (_guard, _temp, project, view) = setup_project();
    fs::write(project.join("Assets/Avatar/Foo.prefab"), "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "known good", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    let quarantine = view
        .storage_root_path
        .join("repos")
        .join(view.project_id)
        .join("quarantined-journals/orphan-zero-change");
    fs::create_dir_all(&quarantine).unwrap();
    assert_eq!(
        crate::unresolved_transaction_quarantines(&project)
            .unwrap()
            .len(),
        1
    );

    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();
    assert!(!plan.has_changes);
    let result = crate::apply_restore_plan(
        &project,
        checkpoint.as_str(),
        plan,
        ApplyOptions { yes: true },
    )
    .unwrap();

    assert!(!result.applied);
    assert!(crate::unresolved_transaction_quarantines(&project)
        .unwrap()
        .is_empty());
}

#[test]
fn recovery_after_fault_after_backup_move_restores_missing_project_file() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, initial_plan) = replace_plan(&project);
    let original_mtime = FileTime::from_unix_time(1_600_000_000, 123_000_000);
    filetime::set_file_mtime(&file, original_mtime).unwrap();
    let plan = crate::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(initial_plan.checkpoint_id.as_str()),
    )
    .unwrap();

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
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    let interrupted_copy =
        transaction_root.join("backup/Assets/Avatar/.checkpo-0123456789abcdef0123456789abcdef.tmp");
    fs::write(&interrupted_copy, "partial backup").unwrap();
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
    assert_eq!(
        FileTime::from_last_modification_time(&fs::metadata(&file).unwrap()),
        original_mtime
    );
    assert!(transaction_root
        .join("backup/Assets/Avatar/Foo.prefab")
        .is_file());
}

#[cfg(not(windows))]
#[test]
fn concurrent_source_replacement_after_backup_copy_is_preserved() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let preserved = project.join("Assets/Avatar/Foo-before-concurrent-edit.prefab");

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectFileBackedUp {
                fs::rename(&file, &preserved).unwrap();
                fs::write(&file, "concurrent-edit").unwrap();
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "concurrent-edit");
    assert_eq!(fs::read_to_string(&preserved).unwrap(), "two");
    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(
        fs::read_to_string(transaction_root.join("backup/Assets/Avatar/Foo.prefab")).unwrap(),
        "two"
    );
}

#[cfg(not(windows))]
#[test]
fn concurrent_same_inode_write_after_backup_copy_is_preserved() {
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
                fs::write(&file, "concurrent-write").unwrap();
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "concurrent-write");
    let transaction_root = crate::pending_transactions(&project).unwrap()[0]
        .journal_path
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(
        fs::read_to_string(transaction_root.join("backup/Assets/Avatar/Foo.prefab")).unwrap(),
        "two"
    );
}

#[test]
fn backup_barrier_faults_recover_the_complete_before_state() {
    for fault_point in [
        TransactionFaultPoint::BackupDirectoryBarrierBefore,
        TransactionFaultPoint::BackupDirectoryBarrierAfter,
        TransactionFaultPoint::BackupSourceCleanupBefore,
        TransactionFaultPoint::BackupSourceCleanupAfter,
    ] {
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
                if point == fault_point {
                    return Err(injected_fault(point));
                }
                Ok(())
            }),
        )
        .unwrap_err();

        assert!(matches!(error, CheckPoError::Unexpected(_)));
        let recovered = crate::recover_transactions(&project).unwrap();
        assert_eq!(
            recovered.recovered_transaction_count, 1,
            "fault point: {fault_point:?}; failures: {:?}",
            recovered.failed_transactions
        );
        assert_eq!(recovered.failed_transaction_count, 0);
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert!(crate::pending_transactions(&project).unwrap().is_empty());
    }
}

#[test]
fn second_backup_batch_barrier_fault_recovers_every_source() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (_guard, _temp, project, _view) = setup_project();
    let mut selections = Vec::new();
    for index in 0..=TRANSACTION_BACKUP_FILE_BATCH_SIZE {
        let relative = format!("Assets/Avatar/Batch-{index:03}.asset");
        fs::write(project.join(&relative), format!("snapshot-{index}")).unwrap();
        selections.push(relative);
    }
    let checkpoint = crate::create_checkpoint(
        &project,
        "batch baseline",
        CreateCheckpointOptions::default(),
    )
    .unwrap()
    .checkpoint_id;
    for (index, relative) in selections.iter().enumerate() {
        fs::write(project.join(relative), format!("current-{index}")).unwrap();
    }
    let context = crate::load_project(&project).unwrap();
    let plan =
        crate::preview_discard_files(&project, &selections, Some(checkpoint.as_str())).unwrap();
    assert_eq!(
        plan.operations.len(),
        TRANSACTION_BACKUP_FILE_BATCH_SIZE + 1
    );
    let barrier_count = AtomicUsize::new(0);

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::BackupDirectoryBarrierAfter
                && barrier_count.fetch_add(1, Ordering::SeqCst) == 1
            {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Unexpected(_)));
    assert_eq!(barrier_count.load(Ordering::SeqCst), 2);
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    for (index, relative) in selections.iter().enumerate() {
        assert_eq!(
            fs::read_to_string(project.join(relative)).unwrap(),
            format!("current-{index}")
        );
    }
}

#[cfg(unix)]
#[test]
fn recovery_quarantines_transaction_with_symlink_journal() {
    let (_guard, temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    apply_plan_inner(
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
    let pending = crate::pending_transactions(&project).unwrap();
    let journal = &pending[0].journal_path;
    let outside = temp.path().join("outside.json");
    fs::write(&outside, "do not touch").unwrap();
    fs::remove_file(journal).unwrap();
    std::os::unix::fs::symlink(&outside, journal).unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 0);
    assert_eq!(recovered.failed_transaction_count, 1);
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "do not touch");
    assert_eq!(
        crate::unresolved_transaction_quarantines(&project)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn recovery_restores_blocking_file_after_directory_creation_fault() {
    let (_guard, _temp, project, _view) = setup_project();
    let target = project.join("Assets/Topology");
    fs::create_dir_all(target.join("Nested")).unwrap();
    fs::write(target.join("Nested/snapshot.asset"), "snapshot").unwrap();
    let checkpoint = crate::create_checkpoint(&project, "Tree", CreateCheckpointOptions::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_dir_all(&target).unwrap();
    fs::write(&target, "blocking").unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectDirectoriesCreated {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();
    assert!(target.is_dir());

    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(target.is_file());
    assert_eq!(fs::read_to_string(target).unwrap(), "blocking");
}

#[test]
fn recovery_restores_blocking_file_after_topology_files_were_applied() {
    let (_guard, _temp, project, _view) = setup_project();
    let target = project.join("Assets/Topology");
    fs::create_dir_all(target.join("Nested")).unwrap();
    fs::write(target.join("A.asset"), "snapshot-a").unwrap();
    fs::write(target.join("Nested/B.asset"), "snapshot-b").unwrap();
    let checkpoint = crate::create_checkpoint(&project, "Tree", CreateCheckpointOptions::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_dir_all(&target).unwrap();
    fs::write(&target, "blocking").unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
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
    assert!(target.join("A.asset").is_file());
    assert!(target.join("Nested/B.asset").is_file());

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(target.is_file());
    assert_eq!(fs::read_to_string(&target).unwrap(), "blocking");
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
}

#[test]
fn recovery_recreates_removed_directory_tree_after_topology_fault() {
    let (_guard, _temp, project, _view) = setup_project();
    let target = project.join("Assets/Topology");
    fs::write(&target, "snapshot").unwrap();
    let checkpoint = crate::create_checkpoint(&project, "File", CreateCheckpointOptions::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_file(&target).unwrap();
    fs::create_dir_all(target.join("Nested/Empty")).unwrap();
    fs::write(target.join("Nested/current.asset"), "current").unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectDirectoriesRemoved {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();
    assert!(!target.exists());

    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(target.join("Nested/Empty").is_dir());
    assert_eq!(
        fs::read_to_string(target.join("Nested/current.asset")).unwrap(),
        "current"
    );
}

#[test]
fn recovery_rolls_back_when_staged_journal_has_a_durable_backup() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    apply_plan_inner(
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
    let journal_path = pending_transactions_for_project(&context).unwrap()[0]
        .journal_path
        .clone();
    let mut journal: TransactionJournal = crate::read_json(&journal_path).unwrap();
    journal.state = JournalState::Staged;
    write_journal(&journal_path, &journal).unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(file).unwrap(), "two");
}

#[test]
fn recovery_rejects_journal_operation_tampering_before_project_mutation() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);

    apply_plan_inner(
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
    let journal_path = pending_transactions_for_project(&context).unwrap()[0]
        .journal_path
        .clone();
    let mut journal: TransactionJournal = crate::read_json(&journal_path).unwrap();
    journal.operations[0].operation_type = FileOperationType::Delete;
    write_journal(&journal_path, &journal).unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 0);
    assert_eq!(recovered.failed_transaction_count, 1);
    assert_eq!(fs::read_to_string(file).unwrap(), "two");
    assert!(journal_path.exists());
}

#[test]
fn recovery_before_topology_changes_ignores_unreachable_materialization_temp_path() {
    let (_guard, _temp, project, _view) = setup_project();
    let target = project.join("Assets/Topology");
    fs::create_dir_all(target.join("Nested")).unwrap();
    fs::write(target.join("Nested/snapshot.asset"), "snapshot").unwrap();
    let checkpoint = crate::create_checkpoint(&project, "Tree", CreateCheckpointOptions::default())
        .unwrap()
        .checkpoint_id;
    fs::remove_dir_all(&target).unwrap();
    fs::write(&target, "blocking").unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
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

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(
        recovered.recovered_transaction_count, 1,
        "{:?}",
        recovered.failed_transactions
    );
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&target).unwrap(), "blocking");
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
}

#[test]
fn recovery_rolls_back_staged_restore_when_staged_file_was_moved() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "Initial", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    fs::remove_file(&file).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.as_str()).unwrap();

    apply_plan_inner(
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
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    let journal_path = pending_transactions_for_project(&context).unwrap()[0]
        .journal_path
        .clone();
    let mut journal: TransactionJournal = crate::read_json(&journal_path).unwrap();
    journal.state = JournalState::Staged;
    write_journal(&journal_path, &journal).unwrap();

    let recovered = crate::recover_transactions(&project).unwrap();

    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert!(!file.exists());
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
                let backup = fs::read_dir(repo.join("journals").join("transactions"))
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
fn backup_reflink_or_copy_keeps_project_file_when_backup_hash_mismatches() {
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
        .join("journals/transactions/copyfallback/backup/Assets/Avatar/Foo.prefab");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();

    let error = backup_project_file_by_reflink_or_copy(
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
                before_modified_at_utc: None,
                after_hash: Some(ObjectId::parse(&"2".repeat(64)).unwrap()),
                after_size_bytes: Some(10),
                after_modified_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
            },
            FileOperation {
                operation_type: FileOperationType::Replace,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Grow.prefab").unwrap(),
                before_hash: Some(ObjectId::parse(&"3".repeat(64)).unwrap()),
                before_size_bytes: Some(4),
                before_modified_at_utc: Some("2025-01-01T00:00:00Z".to_string()),
                after_hash: Some(ObjectId::parse(&"4".repeat(64)).unwrap()),
                after_size_bytes: Some(9),
                after_modified_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
            },
            FileOperation {
                operation_type: FileOperationType::Replace,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Shrink.prefab").unwrap(),
                before_hash: Some(ObjectId::parse(&"5".repeat(64)).unwrap()),
                before_size_bytes: Some(20),
                before_modified_at_utc: Some("2025-01-01T00:00:00Z".to_string()),
                after_hash: Some(ObjectId::parse(&"6".repeat(64)).unwrap()),
                after_size_bytes: Some(5),
                after_modified_at_utc: Some("2026-01-01T00:00:00Z".to_string()),
            },
            FileOperation {
                operation_type: FileOperationType::Delete,
                path: TrackedUnityFilePath::parse("Assets/Avatar/Delete.prefab").unwrap(),
                before_hash: Some(ObjectId::parse(&"7".repeat(64)).unwrap()),
                before_size_bytes: Some(100),
                before_modified_at_utc: Some("2025-01-01T00:00:00Z".to_string()),
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
fn metadata_only_apply_uses_no_payload_and_recovery_restores_before_mtime() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "same-content").unwrap();
    let snapshot_mtime = FileTime::from_unix_time(1_700_000_000, 123_000_000);
    filetime::set_file_mtime(&file, snapshot_mtime).unwrap();
    let checkpoint =
        crate::create_checkpoint(&project, "metadata", CreateCheckpointOptions::default())
            .unwrap()
            .checkpoint_id;
    let before_mtime = FileTime::from_unix_time(1_710_000_000, 456_000_000);
    filetime::set_file_mtime(&file, before_mtime).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_discard_files(
        &project,
        &["Assets/Avatar/Foo.prefab".to_string()],
        Some(checkpoint.as_str()),
    )
    .unwrap();

    assert_eq!(plan.metadata_count, 1);
    assert_eq!(plan.replace_count, 0);
    assert_eq!(plan.staged_bytes, 0);
    assert_eq!(plan.backup_bytes, 0);
    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        None,
        None,
        Some(&|point| {
            if point == TransactionFaultPoint::ProjectMetadataUpdated {
                return Err(injected_fault(point));
            }
            Ok(())
        }),
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Unexpected(_)));
    assert_eq!(
        FileTime::from_last_modification_time(&fs::metadata(&file).unwrap()),
        snapshot_mtime
    );
    let recovered = crate::recover_transactions(&project).unwrap();
    assert_eq!(recovered.recovered_transaction_count, 1);
    assert_eq!(recovered.failed_transaction_count, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "same-content");
    assert_eq!(
        FileTime::from_last_modification_time(&fs::metadata(&file).unwrap()),
        before_mtime
    );
}

#[test]
fn destructive_preview_dtos_reject_unknown_fields_and_schema_mismatch() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, mut plan) = replace_plan(&project);
    let mut operation_value = serde_json::to_value(&plan).unwrap();
    operation_value
        .as_object_mut()
        .unwrap()
        .insert("unexpected".to_string(), serde_json::json!(true));
    assert!(serde_json::from_value::<OperationPlan>(operation_value).is_err());

    plan.schema_version += 1;
    let error =
        apply_plan_inner(&context, plan, ApplyOptions { yes: true }, None, None, None).unwrap_err();
    assert!(
        matches!(error, CheckPoError::Corruption(message) if message.contains("schema version"))
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");

    let cleanup = analyze_transaction_cleanup(&project).unwrap();
    let mut cleanup_value = serde_json::to_value(cleanup).unwrap();
    cleanup_value
        .as_object_mut()
        .unwrap()
        .insert("unexpected".to_string(), serde_json::json!(true));
    assert!(serde_json::from_value::<TransactionCleanupPlan>(cleanup_value).is_err());
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
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
}

#[test]
fn parallel_staging_reports_progress_in_plan_order_and_restores_every_file() {
    let (_guard, _temp, project, _view) = setup_project();
    let bulk = project.join("Assets/Bulk");
    let mut expected_contents = BTreeMap::new();
    for index in 0..24_u32 {
        let relative = format!("Assets/Bulk/Group{}/File{index:03}.asset", index % 4);
        let content = format!("payload-{index:03}");
        let path = project.join(&relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &content).unwrap();
        expected_contents.insert(relative, content);
    }
    let checkpoint = crate::create_checkpoint(
        &project,
        "parallel staging",
        CreateCheckpointOptions::default(),
    )
    .unwrap();
    fs::remove_dir_all(&bulk).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.checkpoint_id.as_str()).unwrap();
    let expected_order = plan
        .operations
        .iter()
        .map(|operation| operation.path.to_string())
        .collect::<Vec<_>>();
    assert_eq!(expected_order.len(), expected_contents.len());
    let staging_events = Mutex::new(Vec::new());
    let progress = |event: OperationProgress| {
        if event.phase == "staging" {
            staging_events.lock().unwrap().push(event);
        }
    };

    apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        Some(&progress),
        None,
        None,
    )
    .unwrap();

    let staging_events = staging_events.into_inner().unwrap();
    assert_eq!(staging_events.len(), expected_order.len());
    assert_eq!(
        staging_events
            .iter()
            .map(|event| event.completed)
            .collect::<Vec<_>>(),
        (1..=expected_order.len()).collect::<Vec<_>>()
    );
    assert_eq!(
        staging_events
            .iter()
            .map(|event| event.current_item.clone().unwrap())
            .collect::<Vec<_>>(),
        expected_order
    );
    for (relative, expected) in expected_contents {
        assert_eq!(
            fs::read_to_string(project.join(relative)).unwrap(),
            expected
        );
    }
}

#[test]
fn parallel_staging_returns_the_lowest_plan_index_error() {
    let (_guard, _temp, project, _view) = setup_project();
    let bulk = project.join("Assets/Bulk");
    for index in 0..12_u32 {
        let path = bulk.join(format!("File{index:03}.asset"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("payload-{index:03}")).unwrap();
    }
    let checkpoint = crate::create_checkpoint(
        &project,
        "parallel staging errors",
        CreateCheckpointOptions::default(),
    )
    .unwrap();
    fs::remove_dir_all(&bulk).unwrap();
    let context = crate::load_project(&project).unwrap();
    let plan = crate::preview_restore(&project, checkpoint.checkpoint_id.as_str()).unwrap();
    assert_eq!(plan.operations.len(), 12);
    let first_corrupt = plan.operations[1].after_hash.clone().unwrap();
    let later_corrupt = plan.operations[9].after_hash.clone().unwrap();
    fs::write(
        crate::object_path(&context.repo_root, &first_corrupt),
        "corrupt-first",
    )
    .unwrap();
    fs::write(
        crate::object_path(&context.repo_root, &later_corrupt),
        "corrupt-later",
    )
    .unwrap();

    let error =
        apply_plan_inner(&context, plan, ApplyOptions { yes: true }, None, None, None).unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::ObjectHashMismatch(ref message)
            if message.contains(first_corrupt.as_str())
    ));
    assert!(!bulk.exists());
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
}

#[test]
fn prepared_staging_worker_does_not_create_a_missing_destination_parent() {
    let (_guard, _temp, project, _view) = setup_project();
    let tracked = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&tracked, "one").unwrap();
    let checkpoint = crate::create_checkpoint(
        &project,
        "prepared parent",
        CreateCheckpointOptions::default(),
    )
    .unwrap();
    let context = crate::load_project(&project).unwrap();
    let snapshot = crate::load_project_snapshot(&context, &checkpoint.checkpoint_id).unwrap();
    let file = snapshot
        .files
        .iter()
        .find(|file| file.path.as_str() == "Assets/Avatar/Foo.prefab")
        .unwrap();
    let destination = context
        .repo_root
        .join("journals/transactions/manual/staged/Assets/Missing/Foo.prefab");
    let anchored_repo = crate::storage::AnchoredRoot::open(&context.repo_root).unwrap();
    let mut sync_batch = crate::storage::AnchoredParentSyncBatch::new();

    let error = stage_object_for_transaction_prepared(
        &context,
        &anchored_repo,
        file.content_hash(),
        &destination,
        file.content_size_bytes(),
        Some(file.modified_at_utc.as_str()),
        &mut sync_batch,
        None,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CheckPoError::Io { source, .. } if source.kind() == ErrorKind::NotFound
    ));
    assert!(!destination.parent().unwrap().exists());
}

#[test]
fn cancellation_during_staging_aborts_without_leaving_a_pending_transaction() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let cancellation = CancellationToken::new();
    let progress = |event: OperationProgress| {
        if event.phase == "staging" {
            cancellation.cancel();
        }
    };

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        Some(&progress),
        Some(&cancellation),
        None,
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::Cancelled));
    assert_eq!(fs::read_to_string(&file).unwrap(), "two");
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
}

#[test]
fn precondition_failure_after_staging_aborts_without_leaving_a_pending_transaction() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let progress_file = file.clone();
    let progress = move |event: OperationProgress| {
        if event.phase == "staging" {
            fs::write(&progress_file, "three").unwrap();
        }
    };

    let error = apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        Some(&progress),
        None,
        None,
    )
    .unwrap_err();

    assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "three");
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
}

#[test]
fn backup_is_an_independent_copy_of_the_project_file() {
    use std::io::{Seek, SeekFrom, Write};

    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let operation = plan.operations.first().unwrap();
    let backup = context
        .repo_root
        .join("journals/transactions/copy/backup/Assets/Avatar/Foo.prefab");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();
    let mut open_handle = fs::OpenOptions::new().write(true).open(&file).unwrap();

    backup_project_file_by_reflink_or_copy(
        &context,
        operation,
        &file,
        &backup,
        required_before_hash(operation).unwrap(),
    )
    .unwrap();
    open_handle.seek(SeekFrom::Start(0)).unwrap();
    open_handle.write_all(b"mutated").unwrap();
    open_handle.flush().unwrap();

    assert_eq!(fs::read_to_string(&backup).unwrap(), "two");
    assert!(!file.exists());
    assert!(fs::read_dir(file.parent().unwrap())
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".checkpo-")));
}

#[test]
fn apply_progress_moves_past_staging_during_destructive_work() {
    let (_guard, _temp, project, _view) = setup_project();
    let file = project.join("Assets/Avatar/Foo.prefab");
    fs::write(&file, "one").unwrap();
    let (context, plan) = replace_plan(&project);
    let events = Mutex::new(Vec::new());
    let cancellation = CancellationToken::new();
    let progress = |event: OperationProgress| {
        if event.phase == "backingUp" && event.completed == 0 {
            cancellation.cancel();
        }
        events.lock().unwrap().push(event);
    };

    apply_plan_inner(
        &context,
        plan,
        ApplyOptions { yes: true },
        Some(&progress),
        Some(&cancellation),
        None,
    )
    .unwrap();

    let events = events.into_inner().unwrap();
    assert!(events
        .iter()
        .any(|event| { event.phase == "backingUp" && event.completed == 0 && event.total == 1 }));
    assert!(events
        .iter()
        .any(|event| { event.phase == "backingUp" && event.completed == 1 && event.total == 1 }));
    let phases =
        events
            .into_iter()
            .map(|event| event.phase)
            .fold(Vec::new(), |mut phases, phase| {
                if phases.last() != Some(&phase) {
                    phases.push(phase);
                }
                phases
            });
    assert_eq!(
        phases,
        ["staging", "backingUp", "applying", "finalizing", "complete"]
    );
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
                let staged = fs::read_dir(repo.join("journals").join("transactions"))
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
    assert_eq!(recovered.recovered_transaction_count, 0);
    assert_eq!(recovered.failed_transaction_count, 1);
    assert_eq!(recovered.failed_transactions.len(), 1);
    assert!(recovered.failed_transactions[0]
        .error
        .contains("transaction payload contains a symlink"));

    let pending = crate::pending_transactions(&project).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].transaction_id,
        recovered.failed_transactions[0].transaction_id
    );

    let quarantine = crate::quarantine_transaction(
        &project,
        &pending[0].transaction_id,
        ApplyOptions { yes: true },
    )
    .unwrap();
    assert!(quarantine.quarantine_path.is_dir());
    assert!(quarantine.quarantine_path.join("journal.json").is_file());
    assert!(quarantine.quarantine_path.join("staged").is_dir());
    assert!(crate::pending_transactions(&project).unwrap().is_empty());
    assert!(!file.exists());
}
