use crate::{
    acquire_project_location_lock, acquire_registry_lock, ensure_repo_outside_project,
    load_project_marker, load_registry, load_repo_config, marker_path, normalize_existing_dir,
    repo_root, update_registry_locked, validate_unity_project_root, CheckPoError, ProjectContext,
    ProjectRoot, ProjectView, Result, StorageRoot,
};
use std::path::Path;

pub fn set_project_storage_root(
    project_path: impl AsRef<Path>,
    storage_root_path: impl AsRef<Path>,
) -> Result<ProjectView> {
    let project_root = normalize_existing_dir(project_path.as_ref())?;
    validate_unity_project_root(&project_root)?;
    let marker_path = marker_path(&project_root);
    if !marker_path.is_file() {
        return Err(CheckPoError::InvalidProject(format!(
            "CheckPo marker was not found: {}",
            marker_path.display()
        )));
    }
    let marker = load_project_marker(&marker_path)?;
    let _location_lock = acquire_project_location_lock(&marker.project_id, "storage-root-set")?;
    let marker = load_project_marker(&marker_path)?;
    let registry_lock = acquire_registry_lock()?;
    let registry = load_registry()?;
    let entry = registry
        .projects
        .get(marker.project_id.as_str())
        .ok_or_else(|| {
            CheckPoError::InvalidProject(
                "Storage registry entry was not found for this project. Run init again."
                    .to_string(),
            )
        })?;
    let (location_status, _) =
        crate::project_location_status_and_warnings(&project_root, &marker.project_id, entry);
    if location_status == crate::ProjectLocationStatus::CopiedSuspected {
        return Err(CheckPoError::CopiedProjectSuspected(
            "This Unity project appears to be a copy of another CheckPo project. Choose 'use this location' or 'start as a separate project' before changing storage settings.".to_string(),
        ));
    }

    let storage_root = normalize_existing_dir(storage_root_path.as_ref())?;
    let project_id = marker.project_id;
    let old_storage_root = normalize_existing_dir(&entry.storage_root_path).ok();
    let old_repo_root = old_storage_root
        .as_ref()
        .map(|storage_root| repo_root(storage_root, &project_id))
        .filter(|repo_root| repo_root.exists());
    let new_repo_root = repo_root(&storage_root, &project_id);
    ensure_repo_outside_project(&project_root, &new_repo_root)?;
    load_repo_config(&new_repo_root, &project_id).map_err(|error| {
        crate::user_error(format!(
            "checkpoint repository was not found at {}. Move the existing repository there before changing the storage folder: {error}",
            new_repo_root.display()
        ))
    })?;

    let context = ProjectContext {
        project_id,
        project_root: ProjectRoot::new(project_root),
        storage_root: StorageRoot::new(storage_root),
        repo_root: new_repo_root,
        location_status,
        warnings: Vec::new(),
    };
    let _repo_locks = lock_storage_root_repositories(old_repo_root.as_deref(), &context.repo_root)?;
    ensure_repo_outside_project(context.project_root.as_path(), &context.repo_root)?;
    load_repo_config(&context.repo_root, &context.project_id)?;
    crate::validate_repository_layout_no_follow(&context.repo_root)?;
    if let (Some(old_storage_root), Some(old_repo_root)) = (old_storage_root, old_repo_root) {
        let old_context = ProjectContext {
            project_id: context.project_id.clone(),
            project_root: context.project_root.clone(),
            storage_root: StorageRoot::new(old_storage_root),
            repo_root: old_repo_root,
            location_status: context.location_status,
            warnings: Vec::new(),
        };
        ensure_repo_outside_project(old_context.project_root.as_path(), &old_context.repo_root)?;
        load_repo_config(&old_context.repo_root, &old_context.project_id)?;
        crate::validate_repository_layout_no_follow(&old_context.repo_root)?;
        crate::ensure_no_pending_transactions(&old_context)?;
        crate::ensure_no_unresolved_transaction_quarantines(&old_context)?;
    }
    crate::ensure_no_pending_transactions(&context)?;
    crate::ensure_no_unresolved_transaction_quarantines(&context)?;
    update_registry_locked(
        &registry_lock,
        registry,
        &context.project_id,
        context.project_root.as_path(),
        context.storage_root.as_path(),
    )?;
    crate::project_view(&context)
}

fn lock_storage_root_repositories(
    old_repo_root: Option<&Path>,
    new_repo_root: &Path,
) -> Result<Vec<crate::RepositoryLock>> {
    let mut repo_roots = Vec::new();
    if let Some(old_repo_root) = old_repo_root {
        repo_roots.push(old_repo_root.to_path_buf());
    }
    if !repo_roots
        .iter()
        .any(|repo_root| repo_root == new_repo_root)
    {
        repo_roots.push(new_repo_root.to_path_buf());
    }
    repo_roots.sort();
    let mut locks = Vec::new();
    for repo_root in repo_roots {
        locks.push(crate::acquire_repository_lock(
            &repo_root,
            "storage-root-set",
        )?);
    }
    Ok(locks)
}
