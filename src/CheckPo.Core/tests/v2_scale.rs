use checkpo_core as core;
use std::fs;
use std::time::Instant;

/// Destructive end-to-end scale verification for the snapshot v2 repository.
///
/// Run explicitly, for example:
/// `CHECKPO_SCALE_CHECKPOINTS=1000 cargo test -p checkpo-core --test v2_scale -- --ignored --nocapture`
#[test]
#[ignore = "destructive scale test; run explicitly on the CheckPo test volume"]
fn snapshot_v2_checkpoint_scale() {
    let checkpoint_count = std::env::var("CHECKPO_SCALE_CHECKPOINTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1000);
    assert!(checkpoint_count > 0);
    let base = std::env::var_os("CHECKPO_SCALE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!(
                "/Volumes/870EVO/CheckPo-TestProjects/snapshot-v2-scale-{checkpoint_count}"
            ))
        });
    if base.exists() {
        fs::remove_dir_all(&base).unwrap();
    }
    let project = base.join("UnityProject");
    let storage = base.join("Storage");
    fs::create_dir_all(project.join("Assets/Scale")).unwrap();
    fs::create_dir_all(project.join("Packages")).unwrap();
    fs::create_dir_all(project.join("ProjectSettings")).unwrap();
    fs::write(
        project.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.0f1\n",
    )
    .unwrap();
    for index in 0..256 {
        fs::write(
            project.join(format!("Assets/Scale/File{index:04}.asset")),
            format!("initial-{index:04}\n"),
        )
        .unwrap();
    }
    std::env::set_var("CHECKPO_DATA_DIR", &storage);
    let view = core::init_project(&project).unwrap();
    let repo = view.storage_root_path.join("repos").join(view.project_id);
    let started = Instant::now();
    let mut first_checkpoint = None;
    for index in 0..checkpoint_count {
        let changed = index % 256;
        fs::write(
            project.join(format!("Assets/Scale/File{changed:04}.asset")),
            format!("checkpoint-{index:04}-file-{changed:04}\n"),
        )
        .unwrap();
        let checkpoint =
            core::create_checkpoint(&project, &format!("scale-{index:04}"), Default::default())
                .unwrap();
        first_checkpoint.get_or_insert(checkpoint.checkpoint_id);

        let completed = index + 1;
        if matches!(completed, 100 | 500 | 1000) || completed == checkpoint_count {
            let listed = core::list_checkpoints(&project).unwrap();
            assert_eq!(listed.len(), completed);
            let verification = core::verify_project(&project, false).unwrap();
            assert!(verification.is_valid, "{:?}", verification.errors);
            let rebuilt = core::rebuild_index(&project).unwrap();
            assert_eq!(rebuilt.snapshot_count, completed);
            let context = core::load_project(&project).unwrap();
            assert_eq!(
                core::checkpoint_index_status(&context).unwrap().state,
                core::CheckpointIndexState::Current
            );
            let gc = core::analyze_gc(&project).unwrap();
            assert!(!gc.has_integrity_problems, "{:?}", gc.skipped_snapshots);
            eprintln!(
                "verified {completed} checkpoints in {:.2?}",
                started.elapsed()
            );
        }
    }

    let manifest_files_before = walkdir::WalkDir::new(repo.join("manifests/v2"))
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();
    assert!(manifest_files_before > 1);
    core::delete_checkpoint(&project, first_checkpoint.as_ref().unwrap().as_str()).unwrap();
    let gc_plan = core::analyze_gc(&project).unwrap();
    let gc = core::apply_gc_with_expected_plan(&project, &gc_plan.plan_id).unwrap();
    assert!(gc.deleted_blob_count > 0 || gc.deleted_manifest_chunk_count > 0);
    let manifest_files_after = walkdir::WalkDir::new(repo.join("manifests/v2"))
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();
    assert!(manifest_files_after <= manifest_files_before);
    let verification = core::verify_project(&project, true).unwrap();
    assert!(verification.is_valid, "{:?}", verification.errors);
}
