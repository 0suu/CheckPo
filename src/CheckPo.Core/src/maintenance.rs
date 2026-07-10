use crate::{
    acquire_repository_lock, ensure_no_pending_transactions, io_error, list_snapshot_ids,
    load_project, load_snapshot, object_path, relative_path_from_project, sync_parent_dir,
    CheckPoError, InvalidObjectLocation, MissingBlobReference, ObjectId, OrphanTempFile, Result,
    SkippedSnapshot, StorageGcPlan, StorageGcResult, StorageSummary, TempFileCleanupPlan,
    TempFileCleanupResult, TrackedUnityFilePath, UnreferencedBlob,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

pub fn storage_summary(project_path: impl AsRef<Path>) -> crate::Result<StorageSummary> {
    let project = load_project(project_path)?;
    crate::storage_summary_from_index(&project)
}

pub fn analyze_gc(project_path: impl AsRef<Path>) -> Result<StorageGcPlan> {
    let project = load_project(project_path)?;
    analyze_gc_for_project(&project)
}

pub fn apply_gc(project_path: impl AsRef<Path>) -> Result<StorageGcResult> {
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "storage-gc")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let plan = analyze_gc_for_project(&project)?;
    if plan.has_integrity_problems {
        return Err(crate::user_error(
            "storage gc cannot apply while missing objects, invalid object locations, or unreadable snapshots exist.",
        ));
    }

    let loose_root = project.repo_root.join("objects").join("loose");
    let mut deleted_blob_count = 0_usize;
    let mut deleted_bytes = 0_u64;
    for blob in &plan.unreferenced_blobs {
        let path = safe_repo_relative_file(&project.repo_root, &blob.object_path)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_parent_dir(&path)?;
                deleted_blob_count += 1;
                deleted_bytes += blob.size_bytes;
                remove_empty_object_dirs(path.parent(), &loose_root)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&path, error)),
        }
    }

    Ok(StorageGcResult {
        plan,
        applied: true,
        deleted_blob_count,
        deleted_bytes,
    })
}

pub fn analyze_orphan_temp_files(project_path: impl AsRef<Path>) -> Result<TempFileCleanupPlan> {
    let project = load_project(project_path)?;
    analyze_orphan_temp_files_for_project(&project)
}

pub fn cleanup_orphan_temp_files(
    project_path: impl AsRef<Path>,
    options: crate::ApplyOptions,
) -> Result<TempFileCleanupResult> {
    if !options.yes {
        return Err(crate::user_error("temporary file cleanup requires --yes."));
    }
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "temporary-file-cleanup")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let plan = analyze_orphan_temp_files_for_project(&project)?;
    let mut deleted_file_count = 0_usize;
    let mut deleted_bytes = 0_u64;
    let mut warnings = Vec::new();
    for file in &plan.files {
        crate::project::ensure_project_parent_is_safe(&project, &file.path)?;
        let path = file.path.to_project_path(project.project_root.as_path());
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && crate::is_checkpo_owned_temporary_file(&path) =>
            {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        sync_parent_dir(&path)?;
                        deleted_file_count += 1;
                        deleted_bytes += metadata.len();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error(&path, error)),
                }
            }
            Ok(_) => warnings.push(format!(
                "{} was not deleted because it is no longer a CheckPo temporary file",
                file.path
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&path, error)),
        }
    }
    Ok(TempFileCleanupResult {
        plan,
        deleted_file_count,
        deleted_bytes,
        warnings,
    })
}

fn analyze_orphan_temp_files_for_project(
    project: &crate::ProjectContext,
) -> Result<TempFileCleanupPlan> {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for root in ["Assets", "Packages", "ProjectSettings"] {
        let root_path = project.project_root.as_path().join(root);
        if !root_path.exists() {
            continue;
        }
        if !root_path.is_dir() {
            warnings.push(format!("{root}: tracked root is not a directory"));
            continue;
        }
        for entry in WalkDir::new(&root_path).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let path = error
                        .path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| root.to_string());
                    warnings.push(format!("{path}: {error}"));
                    continue;
                }
            };
            if !entry.file_type().is_file() || !crate::is_checkpo_owned_temporary_file(entry.path())
            {
                continue;
            }
            let relative =
                match relative_path_from_project(project.project_root.as_path(), entry.path()) {
                    Ok(relative) => relative,
                    Err(error) => {
                        warnings.push(format!("{}: {error}", entry.path().display()));
                        continue;
                    }
                };
            let path = match TrackedUnityFilePath::parse(&relative) {
                Ok(path) => path,
                Err(error) => {
                    warnings.push(format!("{relative}: {error}"));
                    continue;
                }
            };
            let metadata = match fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!("{relative}: {error}"));
                    continue;
                }
            };
            files.push(OrphanTempFile {
                path,
                size_bytes: metadata.len(),
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let total_bytes = files.iter().map(|file| file.size_bytes).sum();
    Ok(TempFileCleanupPlan {
        file_count: files.len(),
        total_bytes,
        files,
        warnings,
    })
}

fn analyze_gc_for_project(project: &crate::ProjectContext) -> Result<StorageGcPlan> {
    let mut referenced = BTreeSet::new();
    let mut missing_references = Vec::new();
    let mut skipped_snapshots = Vec::new();
    let mut checkpoint_count = 0_usize;
    for snapshot_id in list_snapshot_ids(&project.repo_root)? {
        match load_snapshot(&project.repo_root, &snapshot_id) {
            Ok(snapshot) if snapshot.project_id == project.project_id => {
                checkpoint_count += 1;
                for file in snapshot.files {
                    let object_id = file.content_hash().clone();
                    if !object_path(&project.repo_root, &object_id).is_file() {
                        missing_references.push(MissingBlobReference {
                            checkpoint_id: snapshot_id.clone(),
                            path: file.path.clone(),
                            object_id: object_id.clone(),
                        });
                    }
                    referenced.insert(object_id);
                }
            }
            Ok(_) => skipped_snapshots.push(SkippedSnapshot {
                checkpoint_id: snapshot_id,
                reason: "snapshot project id does not match repository project id.".to_string(),
            }),
            Err(error) => skipped_snapshots.push(SkippedSnapshot {
                checkpoint_id: snapshot_id,
                reason: error.to_string(),
            }),
        }
    }

    let ObjectInventory {
        objects,
        invalid_locations,
    } = enumerate_loose_objects(&project.repo_root)?;
    let object_file_count = objects.len() + invalid_locations.len();
    let mut unreferenced_blobs = Vec::new();
    for (object_id, object_path) in objects {
        if referenced.contains(&object_id) {
            continue;
        }
        let full_path = project.repo_root.join(&object_path);
        let size_bytes = fs::metadata(&full_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        unreferenced_blobs.push(UnreferencedBlob {
            object_id,
            object_path,
            size_bytes,
        });
    }
    let unreferenced_logical_bytes = unreferenced_blobs.iter().map(|blob| blob.size_bytes).sum();
    let has_integrity_problems = !missing_references.is_empty()
        || !invalid_locations.is_empty()
        || !skipped_snapshots.is_empty();

    Ok(StorageGcPlan {
        checkpoint_count,
        object_file_count,
        referenced_blob_count: referenced.len(),
        unreferenced_blob_count: unreferenced_blobs.len(),
        unreferenced_logical_bytes,
        unreferenced_blobs,
        missing_references,
        invalid_object_locations: invalid_locations,
        skipped_snapshots,
        has_integrity_problems,
    })
}

struct ObjectInventory {
    objects: BTreeMap<ObjectId, PathBuf>,
    invalid_locations: Vec<InvalidObjectLocation>,
}

fn enumerate_loose_objects(repo_root: &Path) -> Result<ObjectInventory> {
    let loose_root = repo_root.join("objects").join("loose");
    let mut objects = BTreeMap::new();
    let mut invalid_locations = Vec::new();
    if !loose_root.exists() {
        return Ok(ObjectInventory {
            objects,
            invalid_locations,
        });
    }
    for entry in WalkDir::new(&loose_root).follow_links(false) {
        let entry = entry.map_err(|error| {
            let path = error
                .path()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| loose_root.clone());
            io_error(path, std::io::Error::other(error))
        })?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| io_error(entry.path(), error))?;
        if metadata.file_type().is_symlink() {
            invalid_locations.push(InvalidObjectLocation {
                object_path: repo_relative_path(repo_root, entry.path())?,
                reason: "object storage symlinks are not supported.".to_string(),
            });
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if entry.path().extension().and_then(|value| value.to_str()) == Some("tmp") {
            continue;
        }
        let relative = repo_relative_path(repo_root, entry.path())?;
        match crate::object_id_from_loose_relative_path(&relative) {
            Ok(object_id) => {
                objects.insert(object_id, relative);
            }
            Err(reason) => invalid_locations.push(InvalidObjectLocation {
                object_path: relative,
                reason,
            }),
        }
    }
    Ok(ObjectInventory {
        objects,
        invalid_locations,
    })
}

fn safe_repo_relative_file(repo_root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(crate::user_error(format!(
            "invalid repository-relative path: {}",
            relative.display()
        )));
    }
    let path = repo_root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(crate::user_error(format!(
            "repository path is not a regular file: {}",
            path.display()
        )));
    }
    let canonical_repo_root = repo_root
        .canonicalize()
        .map_err(|error| io_error(repo_root, error))?;
    let parent = path
        .parent()
        .ok_or_else(|| crate::user_error(format!("invalid repository path: {}", path.display())))?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| io_error(parent, error))?;
    if !canonical_parent.starts_with(canonical_repo_root) {
        return Err(crate::user_error(format!(
            "repository path escapes through symlink: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn remove_empty_object_dirs(mut current: Option<&Path>, stop_at: &Path) -> Result<()> {
    while let Some(path) = current {
        if path == stop_at {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => {
                sync_parent_dir(path)?;
                current = path.parent();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                break;
            }
            Err(error) => return Err(io_error(path, error)),
        }
    }
    Ok(())
}

fn repo_relative_path(repo_root: &Path, full_path: &Path) -> Result<PathBuf> {
    full_path
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .map_err(|_| CheckPoError::OutsideTrackedScope(full_path.display().to_string()))
}
