use crate::{
    init_repo_layout, load_repo_config, now_utc_string, read_json, repo_root, write_json_atomic,
    CheckPoError, ProjectContext, ProjectId, ProjectLocationStatus, ProjectMarkerFile, ProjectRoot,
    ProjectView, ProjectWarning, ProjectWarningKind, RegistryFile, RegistryProjectEntry, Result,
    StorageRoot, TrackedUnityFilePath,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MARKER_DIR: &str = ".checkpo";
const MARKER_FILE: &str = "project.json";

pub fn init_project(project_path: impl AsRef<Path>) -> Result<ProjectView> {
    init_project_internal(project_path.as_ref(), InitMode::Normal, None)
}

pub fn init_project_with_storage_root(
    project_path: impl AsRef<Path>,
    storage_root_path: impl AsRef<Path>,
) -> Result<ProjectView> {
    init_project_internal(
        project_path.as_ref(),
        InitMode::Normal,
        Some(storage_root_path.as_ref()),
    )
}

pub fn start_as_separate_project(
    project_path: impl AsRef<Path>,
    options: crate::ApplyOptions,
) -> Result<ProjectView> {
    require_start_as_separate_confirmation(project_path.as_ref(), options)?;
    init_project_internal(project_path.as_ref(), InitMode::ForceNewProject, None)
}

pub fn start_as_separate_project_with_storage_root(
    project_path: impl AsRef<Path>,
    storage_root_path: impl AsRef<Path>,
    options: crate::ApplyOptions,
) -> Result<ProjectView> {
    require_start_as_separate_confirmation(project_path.as_ref(), options)?;
    init_project_internal(
        project_path.as_ref(),
        InitMode::ForceNewProject,
        Some(storage_root_path.as_ref()),
    )
}

fn require_start_as_separate_confirmation(
    project_path: &Path,
    options: crate::ApplyOptions,
) -> Result<()> {
    if !options.yes {
        return Err(crate::user_error(
            "starting as a separate project requires --yes.",
        ));
    }
    normalize_existing_dir(project_path)?;
    Ok(())
}

pub fn load_project(project_path: impl AsRef<Path>) -> Result<ProjectContext> {
    let loaded = load_project_registration(project_path.as_ref())?;
    if loaded.registry_entry_needs_project_root_refresh
        && loaded.location_status == ProjectLocationStatus::MovedFromMissingOrDifferentMarker
    {
        update_registry(
            &loaded.project_id,
            &loaded.project_root,
            &loaded.storage_root,
        )?;
    }
    Ok(ProjectContext {
        project_id: loaded.project_id,
        project_root: ProjectRoot::new(loaded.project_root),
        storage_root: StorageRoot::new(loaded.storage_root),
        repo_root: loaded.repo_root,
        location_status: loaded.location_status,
        warnings: loaded.warnings,
    })
}

pub fn load_project_view(project_path: impl AsRef<Path>) -> Result<ProjectView> {
    project_view(&load_project(project_path)?)
}

pub fn confirm_project_location(project_path: impl AsRef<Path>) -> Result<ProjectView> {
    let loaded = load_project_registration(project_path.as_ref())?;
    if loaded.location_status == ProjectLocationStatus::Current {
        return project_view(&ProjectContext {
            project_id: loaded.project_id,
            project_root: ProjectRoot::new(loaded.project_root),
            storage_root: StorageRoot::new(loaded.storage_root),
            repo_root: loaded.repo_root,
            location_status: loaded.location_status,
            warnings: Vec::new(),
        });
    }
    update_registry(
        &loaded.project_id,
        &loaded.project_root,
        &loaded.storage_root,
    )?;
    project_view(&ProjectContext {
        project_id: loaded.project_id,
        project_root: ProjectRoot::new(loaded.project_root),
        storage_root: StorageRoot::new(loaded.storage_root),
        repo_root: loaded.repo_root,
        location_status: ProjectLocationStatus::Current,
        warnings: Vec::new(),
    })
}

struct LoadedProjectRegistration {
    project_id: ProjectId,
    project_root: PathBuf,
    storage_root: PathBuf,
    repo_root: PathBuf,
    location_status: ProjectLocationStatus,
    warnings: Vec<ProjectWarning>,
    registry_entry_needs_project_root_refresh: bool,
}

fn load_project_registration(project_path: &Path) -> Result<LoadedProjectRegistration> {
    let project_root = normalize_existing_dir(project_path)?;
    validate_unity_project_root(&project_root)?;
    let marker_path = marker_path(&project_root);
    match fs::symlink_metadata(&marker_path) {
        Ok(metadata) if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(CheckPoError::InvalidProject(format!(
                "CheckPo marker is not a regular file: {}",
                marker_path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CheckPoError::InvalidProject(format!(
                "CheckPo marker was not found: {}",
                marker_path.display()
            )))
        }
        Err(error) => return Err(crate::io_error(&marker_path, error)),
    }
    let marker = load_project_marker(&marker_path)?;
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
    let (location_status, warnings) =
        project_location_status_and_warnings(&project_root, &marker.project_id, entry);
    let storage_root = normalize_existing_dir_or_create_parent(&entry.storage_root_path)?;
    let repo_root = repo_root(&storage_root, &marker.project_id);
    ensure_repo_outside_tracked_roots(&project_root, &repo_root)?;
    load_repo_config(&repo_root, &marker.project_id)?;
    crate::validate_repository_layout_no_follow(&repo_root)?;
    Ok(LoadedProjectRegistration {
        registry_entry_needs_project_root_refresh: registry_entry_needs_project_root_refresh(
            entry,
            &project_root,
        ),
        project_id: marker.project_id,
        project_root,
        storage_root,
        repo_root,
        location_status,
        warnings,
    })
}

pub fn project_view(context: &ProjectContext) -> Result<ProjectView> {
    Ok(ProjectView {
        project_id: context.project_id.to_string(),
        project_root_path: context.project_root.as_path().to_path_buf(),
        storage_root_path: context.storage_root.as_path().to_path_buf(),
        project_name: context
            .project_root
            .as_path()
            .file_name()
            .map(|value| value.to_string_lossy().to_string()),
        unity_version: read_unity_version(context.project_root.as_path()).ok(),
        location_status: context.location_status,
        warnings: context.warnings.clone(),
    })
}

pub fn marker_path(project_root: &Path) -> PathBuf {
    project_root.join(MARKER_DIR).join(MARKER_FILE)
}

pub fn registry_path() -> Result<PathBuf> {
    if let Some(base) = test_or_custom_data_dir() {
        return Ok(base.join("registry.json"));
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| CheckPoError::User("Could not resolve user data dir.".into()))?;
    Ok(base.join("CheckPo").join("registry.json"))
}

pub fn default_storage_root() -> Result<PathBuf> {
    if let Some(base) = test_or_custom_data_dir() {
        return Ok(base);
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| CheckPoError::User("Could not resolve user data dir.".into()))?;
    Ok(base.join("CheckPo"))
}

fn test_or_custom_data_dir() -> Option<PathBuf> {
    std::env::var_os("CHECKPO_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn validate_unity_project_root(project_root: &Path) -> Result<()> {
    for required_dir in ["Assets", "ProjectSettings"] {
        let path = project_root.join(required_dir);
        let valid = fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata))
            .unwrap_or(false);
        if !valid {
            return Err(CheckPoError::InvalidProject(format!(
                "missing or unsafe {required_dir}/"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitMode {
    Normal,
    ForceNewProject,
}

fn init_project_internal(
    project_path: &Path,
    mode: InitMode,
    requested_storage_root: Option<&Path>,
) -> Result<ProjectView> {
    let project_root = normalize_existing_dir(project_path)?;
    validate_unity_project_root(&project_root)?;
    let marker_path = marker_path(&project_root);
    let existing_marker = match fs::symlink_metadata(&marker_path) {
        Ok(metadata) if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {
            Some(load_project_marker(&marker_path)?)
        }
        Ok(_) => {
            return Err(CheckPoError::InvalidProject(format!(
                "CheckPo marker is not a regular file: {}",
                marker_path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(crate::io_error(&marker_path, error)),
    };
    let marker = if mode == InitMode::ForceNewProject {
        new_project_marker()
    } else if let Some(existing_marker) = existing_marker.as_ref() {
        existing_marker.clone()
    } else {
        new_project_marker()
    };
    let registry_lock = acquire_registry_lock()?;
    let registry = load_registry()?;
    if mode == InitMode::ForceNewProject {
        let existing_marker = load_project_marker(&marker_path).map_err(|error| match error {
            CheckPoError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                crate::user_error(
                    "starting as a separate project requires an existing CheckPo marker.",
                )
            }
            error => error,
        })?;
        let entry = registry
            .projects
            .get(existing_marker.project_id.as_str())
            .ok_or_else(|| {
                CheckPoError::InvalidProject(
                    "Storage registry entry was not found for this project. Run init again."
                        .to_string(),
                )
            })?;
        let (status, _) =
            project_location_status_and_warnings(&project_root, &existing_marker.project_id, entry);
        if status != ProjectLocationStatus::CopiedSuspected {
            return Err(crate::user_error(
                "starting as a separate project is only allowed for a copied project.",
            ));
        }
        let old_storage_root = normalize_existing_dir_or_create_parent(&entry.storage_root_path)?;
        let old_context = ProjectContext {
            project_id: existing_marker.project_id.clone(),
            project_root: ProjectRoot::new(project_root.clone()),
            repo_root: repo_root(&old_storage_root, &existing_marker.project_id),
            storage_root: StorageRoot::new(old_storage_root),
            location_status: status,
            warnings: Vec::new(),
        };
        crate::ensure_no_pending_transactions(&old_context)?;
        crate::ensure_no_unresolved_transaction_quarantines(&old_context)?;
    }
    if mode == InitMode::Normal
        && registered_project_root_conflict(
            &project_root,
            &marker.project_id,
            registry.projects.get(marker.project_id.as_str()),
        )
        .is_some()
    {
        return Err(copied_project_error(&project_root));
    }

    let storage_root = match registry.projects.get(marker.project_id.as_str()) {
        Some(entry) => normalize_existing_dir_or_create_parent(&entry.storage_root_path)?,
        None => match requested_storage_root {
            Some(storage_root_path) => normalize_existing_dir_or_create_parent(storage_root_path)?,
            None => default_storage_root()?,
        },
    };
    let storage_root = crate::create_absolute_dir_all_no_follow(&storage_root)?;
    let planned_repo_root = repo_root(&storage_root, &marker.project_id);
    ensure_repo_outside_tracked_roots(&project_root, &planned_repo_root)?;
    let repo_root = init_repo_layout(&storage_root, &marker.project_id)?;
    update_registry_locked(
        &registry_lock,
        registry,
        &marker.project_id,
        &project_root,
        &storage_root,
    )?;
    let marker_directory = marker_path.parent().ok_or_else(|| {
        CheckPoError::InvalidProject(format!("invalid marker path: {}", marker_path.display()))
    })?;
    crate::create_dir_all_no_follow(&project_root, marker_directory)?;
    if mode == InitMode::ForceNewProject || existing_marker.is_none() {
        write_json_atomic(&marker_path, &marker)?;
    }
    let context = ProjectContext {
        project_id: marker.project_id,
        project_root: ProjectRoot::new(project_root),
        storage_root: StorageRoot::new(storage_root),
        repo_root,
        location_status: ProjectLocationStatus::Current,
        warnings: Vec::new(),
    };
    project_view(&context)
}

fn new_project_marker() -> ProjectMarkerFile {
    ProjectMarkerFile {
        schema_version: 1,
        project_id: ProjectId::parse(&Uuid::new_v4().simple().to_string())
            .expect("UUID simple string is a valid project id"),
        created_at_utc: now_utc_string(),
    }
}

pub(crate) fn load_registry() -> Result<RegistryFile> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(RegistryFile {
            schema_version: 1,
            projects: BTreeMap::new(),
        });
    }
    let registry: RegistryFile = read_json(&path)?;
    if registry.schema_version != 1 {
        return Err(CheckPoError::InvalidProject(
            "Unsupported storage registry schema.".to_string(),
        ));
    }
    for project_id in registry.projects.keys() {
        ProjectId::parse(project_id)?;
    }
    Ok(registry)
}

pub(crate) fn load_project_marker(path: &Path) -> Result<ProjectMarkerFile> {
    let parent = path.parent().ok_or_else(|| {
        CheckPoError::InvalidProject(format!("invalid marker path: {}", path.display()))
    })?;
    crate::ensure_regular_directory_no_follow(parent).map_err(|_| {
        CheckPoError::InvalidProject(format!(
            "CheckPo marker directory is unsafe: {}",
            parent.display()
        ))
    })?;
    crate::ensure_regular_file_no_follow(path).map_err(|_| {
        CheckPoError::InvalidProject(format!(
            "CheckPo marker is not a regular file: {}",
            path.display()
        ))
    })?;
    let marker: ProjectMarkerFile = match read_json(path) {
        Err(CheckPoError::Json { source, .. }) if source.to_string().contains("invalid id:") => {
            return Err(CheckPoError::InvalidId(source.to_string()));
        }
        result => result?,
    };
    if marker.schema_version != 1 {
        return Err(CheckPoError::InvalidProject(
            "Unsupported project marker schema. Migration is intentionally not supported."
                .to_string(),
        ));
    }
    Ok(marker)
}

pub(crate) fn ensure_repo_outside_tracked_roots(
    project_root: &Path,
    repo_root: &Path,
) -> Result<()> {
    let repo_abs = normalize_path_for_check(repo_root)?;
    for root in ["Assets", "Packages", "ProjectSettings"] {
        let tracked = project_root.join(root);
        if !tracked.exists() {
            continue;
        }
        let tracked_abs = tracked
            .canonicalize()
            .map_err(|error| crate::io_error(&tracked, error))?;
        if repo_abs.starts_with(&tracked_abs) {
            return Err(crate::user_error(format!(
                "checkpoint repository must not be inside tracked Unity folder: {}",
                repo_abs.display()
            )));
        }
    }
    Ok(())
}

fn normalize_path_for_check(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .map_err(|error| crate::io_error(path, error));
    }
    let parent = path
        .parent()
        .ok_or_else(|| CheckPoError::InvalidProject(format!("invalid path: {}", path.display())))?;
    let parent_abs = normalize_path_for_check(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| CheckPoError::InvalidProject(format!("invalid path: {}", path.display())))?;
    Ok(parent_abs.join(name))
}

pub(crate) fn update_registry(
    project_id: &ProjectId,
    project_root: &Path,
    storage_root: &Path,
) -> Result<()> {
    let registry_lock = acquire_registry_lock()?;
    let registry = load_registry()?;
    update_registry_locked(
        &registry_lock,
        registry,
        project_id,
        project_root,
        storage_root,
    )
}

pub(crate) fn update_registry_locked(
    _lock: &RegistryLock,
    mut registry: RegistryFile,
    project_id: &ProjectId,
    project_root: &Path,
    storage_root: &Path,
) -> Result<()> {
    let path = registry_path()?;
    registry.projects.insert(
        project_id.as_str().to_string(),
        RegistryProjectEntry {
            storage_root_path: storage_root.to_path_buf(),
            last_project_root_path: project_root.to_path_buf(),
            project_name: project_root
                .file_name()
                .map(|value| value.to_string_lossy().to_string()),
            updated_at_utc: now_utc_string(),
        },
    );
    write_json_atomic(&path, &registry)
}

fn registered_project_root_conflict(
    project_root: &Path,
    project_id: &ProjectId,
    entry: Option<&RegistryProjectEntry>,
) -> Option<PathBuf> {
    let entry = entry?;
    let Ok(registered_root) = entry.last_project_root_path.canonicalize() else {
        return None;
    };
    if registered_root.as_path() == project_root {
        return None;
    }
    previous_marker_has_same_project_id(&registered_root, project_id).then_some(registered_root)
}

pub(crate) fn project_location_status_and_warnings(
    project_root: &Path,
    project_id: &ProjectId,
    entry: &RegistryProjectEntry,
) -> (ProjectLocationStatus, Vec<ProjectWarning>) {
    let previous_project_root_path = entry.last_project_root_path.clone();
    let previous_path_exists = previous_project_root_path.exists();
    let same_project_root = previous_project_root_path
        .canonicalize()
        .map(|registered_root| registered_root == project_root)
        .unwrap_or_else(|_| previous_project_root_path == project_root);
    if same_project_root {
        return (ProjectLocationStatus::Current, Vec::new());
    }
    let previous_marker_has_same_project_id =
        previous_marker_has_same_project_id(&previous_project_root_path, project_id);
    let location_status = if previous_marker_has_same_project_id {
        ProjectLocationStatus::CopiedSuspected
    } else {
        ProjectLocationStatus::MovedFromMissingOrDifferentMarker
    };

    let message = if previous_marker_has_same_project_id {
        format!(
            "This project id is already registered for {}. This may be a copied Unity project; initialize it as a separate CheckPo project before using restore/discard.",
            previous_project_root_path.display()
        )
    } else {
        format!(
            "This project appears to have moved from {}. The storage registry will be refreshed to the current path.",
            previous_project_root_path.display()
        )
    };

    let kind = if previous_marker_has_same_project_id {
        ProjectWarningKind::CopiedProjectSuspected
    } else {
        ProjectWarningKind::ProjectMoved
    };

    (
        location_status,
        vec![ProjectWarning {
            kind,
            message,
            location_status,
            previous_project_root_path,
            current_project_root_path: project_root.to_path_buf(),
            previous_path_exists,
            previous_marker_has_same_project_id,
            requires_user_decision: location_status == ProjectLocationStatus::CopiedSuspected,
            destructive_operations_allowed: location_status
                != ProjectLocationStatus::CopiedSuspected,
        }],
    )
}

fn registry_entry_needs_project_root_refresh(
    entry: &RegistryProjectEntry,
    project_root: &Path,
) -> bool {
    entry.last_project_root_path != project_root
}

fn previous_marker_has_same_project_id(path: &Path, project_id: &ProjectId) -> bool {
    let marker_path = marker_path(path);
    load_project_marker(&marker_path)
        .map(|marker| marker.project_id == *project_id)
        .unwrap_or(false)
}

pub fn ensure_project_location_allows_mutation(project: &ProjectContext) -> Result<()> {
    if project.location_status == ProjectLocationStatus::CopiedSuspected {
        return Err(copied_project_error(project.project_root.as_path()));
    }
    Ok(())
}

pub(crate) fn ensure_project_parent_is_safe(
    project: &ProjectContext,
    path: &TrackedUnityFilePath,
) -> Result<()> {
    let mut current = project.project_root.as_path().to_path_buf();
    let segments = path.as_str().split('/').collect::<Vec<_>>();
    for segment in segments.iter().take(segments.len().saturating_sub(1)) {
        current.push(segment);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(crate::io_error(&current, error)),
        };
        let file_type = metadata.file_type();
        if crate::metadata_is_link_or_reparse(&metadata) || !file_type.is_dir() {
            return Err(CheckPoError::InvalidTrackedPath(format!(
                "{} contains unsafe parent component: {}",
                path,
                current.display()
            )));
        }
    }
    Ok(())
}

fn copied_project_error(project_root: &Path) -> CheckPoError {
    CheckPoError::CopiedProjectSuspected(format!(
        "This Unity project appears to be a copy of another CheckPo project: {}. Choose 'use this location' or 'start as a separate project' before changing checkpoints.",
        project_root.display()
    ))
}

pub(crate) type RegistryLock = crate::storage::FileLock;

pub(crate) fn acquire_registry_lock() -> Result<RegistryLock> {
    let path = registry_path()?.with_extension("lock");
    crate::storage::FileLock::acquire(&path, "project-registry")
}

pub(crate) fn normalize_existing_dir(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|error| crate::io_error(path, error))?;
    if !canonical.is_dir() {
        return Err(CheckPoError::InvalidProject(format!(
            "not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn normalize_existing_dir_or_create_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        normalize_existing_dir(path)
    } else {
        normalize_path(path)
    }
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| crate::io_error(".", error))
    }
}

fn read_unity_version(project_root: &Path) -> std::result::Result<String, std::io::Error> {
    let path = project_root
        .join("ProjectSettings")
        .join("ProjectVersion.txt");
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix("m_EditorVersion:"))
        .map(str::trim)
        .unwrap_or("")
        .to_string())
}
