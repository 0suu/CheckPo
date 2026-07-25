use super::*;

pub(super) fn create_recovery_export_stage(
    project: &ProjectContext,
    export_root: &Path,
    transaction_id: &str,
) -> Result<RecoveryExportStage> {
    if !export_root.is_absolute() {
        return Err(crate::user_error(
            "recovery save location must be an absolute path.",
        ));
    }
    let export_root = export_root
        .canonicalize()
        .map_err(|error| crate::io_error(export_root, error))?;
    let project_root = project
        .project_root
        .as_path()
        .canonicalize()
        .map_err(|error| crate::io_error(project.project_root.as_path(), error))?;
    let repo_root = project
        .repo_root
        .canonicalize()
        .map_err(|error| crate::io_error(&project.repo_root, error))?;
    if export_root.starts_with(&project_root) || export_root.starts_with(&repo_root) {
        return Err(crate::user_error(
            "recovery files must be saved outside the Unity project and CheckPo storage.",
        ));
    }
    let root = crate::storage::AnchoredRoot::open(&export_root)?;
    for _ in 0..16 {
        let suffix = &Uuid::new_v4().simple().to_string()[..8];
        let final_name = std::ffi::OsString::from(format!(
            "CheckPo-Recovery-{}-{}",
            &transaction_id[..8],
            suffix
        ));
        let staging_name = std::ffi::OsString::from(format!(
            ".CheckPo-Recovery-{}-{}-incomplete",
            &transaction_id[..8],
            suffix
        ));
        let relative = Path::new(&staging_name);
        let (parent, leaf) = root.open_parent_for_mutation(relative, false)?;
        match parent.create_directory(&leaf) {
            Ok(_) => {
                parent.sync_all()?;
                root.verify_root_binding()?;
                return Ok(RecoveryExportStage {
                    staging_directory: export_root.join(&staging_name),
                    export_root,
                    staging_name,
                    final_name,
                });
            }
            Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(CheckPoError::Unexpected(
        "could not create a unique recovery export directory".to_string(),
    ))
}

pub(super) fn complete_recovery_export<'a>(
    stage: RecoveryExportStage,
    transaction_id: &str,
    conflicts: impl Iterator<Item = &'a TransactionRecoveryConflict>,
) -> Result<PathBuf> {
    let files = conflicts.collect::<Vec<_>>();
    let staging_root = crate::storage::AnchoredRoot::open(&stage.staging_directory)?;
    staging_root.write_json_atomic_new(
        Path::new(RECOVERY_EXPORT_MANIFEST_FILE),
        &RecoveryExportManifest {
            schema_version: RECOVERY_EXPORT_MANIFEST_SCHEMA_VERSION,
            transaction_id,
            created_at_utc: crate::now_utc_string(),
            files,
        },
    )?;
    staging_root.write_bytes_atomic_new(
        Path::new(RECOVERY_EXPORT_COMPLETE_FILE),
        "このフォルダーの保存は完了しています。\r\n".as_bytes(),
    )?;
    staging_root.verify_root_binding()?;

    let export_root = crate::storage::AnchoredRoot::open(&stage.export_root)?;
    let (parent, staging_leaf) =
        export_root.open_parent_for_mutation(Path::new(&stage.staging_name), false)?;
    let staging_directory = parent.open_directory(&staging_leaf)?;
    parent.rename_directory_no_replace_to_owned(
        &staging_leaf,
        staging_directory,
        &parent,
        &stage.final_name,
    )?;
    parent.sync_all()?;
    export_root.verify_root_binding()?;
    Ok(stage.export_root.join(stage.final_name))
}

pub(super) fn copy_recovery_conflict_to_export(
    project: &ProjectContext,
    conflict: &TransactionRecoveryConflict,
    export_directory: &Path,
) -> Result<()> {
    let source_root = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let relative = Path::new(conflict.path.as_str());
    let (source_parent, source_leaf) = source_root.open_parent(relative, false)?;
    let mut source = source_parent.open_file(&source_leaf)?;
    let source_hash = source.hash()?;
    let source_modified_at_utc = source_hash
        .metadata
        .modified()
        .map(crate::canonical_utc)
        .map_err(|error| crate::io_error(conflict.path.to_string(), error))?;
    if source_hash.object_id != conflict.current_hash
        || source_hash.metadata.len() != conflict.size_bytes
        || source_modified_at_utc != conflict.modified_at_utc
    {
        return Err(CheckPoError::WorkingTreeChanged(conflict.path.to_string()));
    }

    let export_root = crate::storage::AnchoredRoot::open(export_directory)?;
    let (destination_parent, destination_leaf) =
        export_root.open_parent_for_mutation(relative, true)?;
    let (temporary_leaf, mut output) =
        destination_parent.create_unique_temporary_file("recovery-export")?;
    let copy_result = (|| -> Result<()> {
        let copied = source.copy_and_hash_to(&mut output, &export_directory.join(relative))?;
        if copied.object_id != conflict.current_hash || copied.metadata.len() != conflict.size_bytes
        {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery export mismatch for {}",
                conflict.path
            )));
        }
        let modified = chrono::DateTime::parse_from_rfc3339(&conflict.modified_at_utc)
            .map_err(|error| CheckPoError::Corruption(error.to_string()))?
            .with_timezone(&chrono::Utc);
        output.set_mtime(modified.into())?;
        output.sync_all()?;
        let readback = output.hash()?;
        if readback.object_id != conflict.current_hash
            || readback.metadata.len() != conflict.size_bytes
        {
            return Err(CheckPoError::ObjectHashMismatch(format!(
                "recovery export readback mismatch for {}",
                conflict.path
            )));
        }
        source_parent.verify_file_binding(&source_leaf, &source)?;
        destination_parent.verify_file_binding(&temporary_leaf, &output)?;
        destination_parent.rename_no_replace_to(
            &temporary_leaf,
            &output,
            &destination_parent,
            &destination_leaf,
        )?;
        destination_parent.verify_file_binding(&destination_leaf, &output)?;
        destination_parent.sync_all()?;
        source_root.verify_root_binding()?;
        export_root.verify_root_binding()
    })();
    if let Err(error) = copy_result {
        let cleanup_leaf = if destination_parent
            .verify_file_binding(&destination_leaf, &output)
            .is_ok()
        {
            destination_leaf.as_os_str()
        } else {
            temporary_leaf.as_os_str()
        };
        let _ = destination_parent.unlink_file_if_bound(cleanup_leaf, output);
        return Err(error);
    }
    Ok(())
}
