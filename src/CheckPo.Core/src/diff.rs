use crate::{
    canonical_utc, load_project, load_project_snapshot, relative_path_from_project,
    scan_project_for_checkpoint, CheckPoError, DiffOptions, DiffResult, ObjectId, Result,
    SnapshotId, TrackedUnityFilePath,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub fn diff_checkpoint(project_path: impl AsRef<Path>, checkpoint_id: &str) -> Result<DiffResult> {
    diff_checkpoint_with_options(project_path, checkpoint_id, DiffOptions::default())
}

pub fn diff_checkpoint_with_options(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    options: DiffOptions,
) -> Result<DiffResult> {
    let project = load_project(project_path)?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    let snapshot = load_project_snapshot(&project, &snapshot_id)?;
    let progress = options.progress.as_deref().map(|f| f as &dyn Fn(_));
    let (working, _) =
        scan_project_for_checkpoint(&project, progress, options.cancellation.as_ref())?;
    let snapshot_map = snapshot
        .files
        .iter()
        .map(|file| (file.path.clone(), file.content_hash().clone()))
        .collect::<BTreeMap<_, _>>();
    let working_map = working
        .into_iter()
        .map(|file| (file.path, file.hash))
        .collect::<BTreeMap<_, _>>();
    Ok(compare_hash_maps(&snapshot_map, &working_map))
}

pub fn diff_checkpoint_metadata(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
) -> Result<DiffResult> {
    let project = load_project(project_path)?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    let snapshot = load_project_snapshot(&project, &snapshot_id)?;
    let snapshot_map = snapshot
        .files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                MetadataDiffEntry {
                    size_bytes: file.size_bytes,
                    modified_at_utc: file.modified_at_utc.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let (working_map, warnings) = scan_project_metadata(project.project_root.as_path())?;
    let mut diff = compare_metadata_maps(&snapshot_map, &working_map);
    diff.warnings = warnings;
    Ok(diff)
}

fn compare_hash_maps(
    snapshot_map: &BTreeMap<TrackedUnityFilePath, ObjectId>,
    working_map: &BTreeMap<TrackedUnityFilePath, ObjectId>,
) -> DiffResult {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut unchanged_count = 0_usize;
    let keys = snapshot_map
        .keys()
        .chain(working_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in keys {
        match (snapshot_map.get(&path), working_map.get(&path)) {
            (None, Some(_)) => added.push(path.to_string()),
            (Some(_), None) => deleted.push(path.to_string()),
            (Some(expected), Some(actual)) if expected != actual => modified.push(path.to_string()),
            (Some(_), Some(_)) => unchanged_count += 1,
            (None, None) => {}
        }
    }
    DiffResult {
        added,
        modified,
        deleted,
        unchanged_count,
        warnings: Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataDiffEntry {
    size_bytes: u64,
    modified_at_utc: String,
}

fn compare_metadata_maps(
    snapshot_map: &BTreeMap<TrackedUnityFilePath, MetadataDiffEntry>,
    working_map: &BTreeMap<TrackedUnityFilePath, MetadataDiffEntry>,
) -> DiffResult {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut unchanged_count = 0_usize;
    let keys = snapshot_map
        .keys()
        .chain(working_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in keys {
        match (snapshot_map.get(&path), working_map.get(&path)) {
            (None, Some(_)) => added.push(path.to_string()),
            (Some(_), None) => deleted.push(path.to_string()),
            (Some(expected), Some(actual)) if expected != actual => modified.push(path.to_string()),
            (Some(_), Some(_)) => unchanged_count += 1,
            (None, None) => {}
        }
    }
    DiffResult {
        added,
        modified,
        deleted,
        unchanged_count,
        warnings: Vec::new(),
    }
}

fn scan_project_metadata(
    project_root: &Path,
) -> Result<(
    BTreeMap<TrackedUnityFilePath, MetadataDiffEntry>,
    Vec<String>,
)> {
    let mut files = BTreeMap::new();
    let mut warnings = Vec::new();
    for root in ["Assets", "Packages", "ProjectSettings"] {
        let root_path = project_root.join(root);
        if !root_path.exists() {
            continue;
        }
        if !root_path.is_dir() {
            warnings.push(format!("{root}: tracked root is not a directory"));
            continue;
        }
        for entry in WalkDir::new(&root_path).follow_links(false).into_iter() {
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
            if entry.file_type().is_symlink() || entry.file_type().is_dir() {
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let full_path = entry.path().to_path_buf();
            let relative = match relative_path_from_project(project_root, &full_path) {
                Ok(relative) => relative,
                Err(error) => {
                    warnings.push(format!("{}: {error}", full_path.display()));
                    continue;
                }
            };
            if is_checkpo_temporary_file(entry.path()) {
                continue;
            }
            let path = match TrackedUnityFilePath::parse(&relative) {
                Ok(path) => path,
                Err(error) => {
                    warnings.push(format!("{relative}: {error}"));
                    continue;
                }
            };
            let leaf_metadata = match fs::symlink_metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!("{}: {error}", path.as_str()));
                    continue;
                }
            };
            if leaf_metadata.file_type().is_symlink() {
                warnings.push(format!(
                    "{}: symlink files are not supported",
                    path.as_str()
                ));
                continue;
            }
            let metadata = match fs::metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!("{}: {error}", path.as_str()));
                    continue;
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    warnings.push(format!("{}: {error}", path.as_str()));
                    continue;
                }
            };
            files.insert(
                path,
                MetadataDiffEntry {
                    size_bytes: metadata.len(),
                    modified_at_utc: canonical_utc(modified),
                },
            );
        }
    }
    if files.keys().any(|path| {
        let full_path = project_root.join(path.as_str());
        !full_path.starts_with(project_root)
    }) {
        return Err(CheckPoError::OutsideTrackedScope(
            project_root.display().to_string(),
        ));
    }
    Ok((files, warnings))
}

fn is_checkpo_temporary_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if name.starts_with(".checkpo-") && name.ends_with(".tmp") {
        return true;
    }
    if !name.starts_with('.') || !name.ends_with(".tmp") {
        return false;
    }
    let body = &name[1..name.len() - ".tmp".len()];
    let Some((_, suffix)) = body.rsplit_once('.') else {
        return false;
    };
    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}
