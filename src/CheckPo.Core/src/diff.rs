use crate::{
    load_project, load_project_snapshot, scan_project_for_checkpoint_with_baseline, DiffOptions,
    DiffResult, Result, SnapshotId, TrackedUnityFilePath,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub fn diff_checkpoint(project_path: impl AsRef<Path>, checkpoint_id: &str) -> Result<DiffResult> {
    diff_checkpoint_with_options(project_path, checkpoint_id, DiffOptions::default())
}

pub fn diff_checkpoint_with_options(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    options: DiffOptions,
) -> Result<DiffResult> {
    let project = load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "checkpoint-diff")?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    let snapshot = load_project_snapshot(&project, &snapshot_id)?;
    let progress = options.progress.as_deref().map(|f| f as &dyn Fn(_));
    let (working, warnings, incomplete) = scan_project_for_checkpoint_with_baseline(
        &project,
        Some(&snapshot),
        progress,
        options.cancellation.as_ref(),
    )?;
    let snapshot_map = snapshot
        .files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                (file.content_hash().clone(), file.modified_at_utc.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let working_map = working
        .into_iter()
        .map(|file| (file.path, (file.hash, file.modified_at_utc)))
        .collect::<BTreeMap<_, _>>();
    let mut diff = compare_maps(&snapshot_map, &working_map, incomplete);
    diff.warnings = warnings
        .iter()
        .map(crate::scanner::format_scan_warning)
        .collect();
    Ok(diff)
}

pub fn diff_checkpoint_metadata(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
) -> Result<DiffResult> {
    diff_checkpoint_metadata_with_cancellation(project_path, checkpoint_id, None)
}

pub fn diff_checkpoint_metadata_with_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<DiffResult> {
    crate::ensure_not_cancelled(cancellation)?;
    let project = load_project(project_path)?;
    let _lock =
        crate::acquire_project_repository_shared_lock(&project, "checkpoint-metadata-diff")?;
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
    let (working, warnings, incomplete) =
        crate::scanner::scan_project_metadata(project.project_root.as_path(), cancellation)?;
    let working_map = working
        .into_iter()
        .map(|file| {
            (
                file.path,
                MetadataDiffEntry {
                    size_bytes: file.size_bytes,
                    modified_at_utc: file.modified_at_utc,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut diff = compare_maps(&snapshot_map, &working_map, incomplete);
    diff.warnings = warnings
        .iter()
        .map(crate::scanner::format_scan_warning)
        .collect();
    Ok(diff)
}

fn compare_maps<T: PartialEq>(
    snapshot_map: &BTreeMap<TrackedUnityFilePath, T>,
    working_map: &BTreeMap<TrackedUnityFilePath, T>,
    incomplete: bool,
) -> DiffResult {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut unknown = Vec::new();
    let mut unchanged_count = 0_usize;
    let keys = snapshot_map
        .keys()
        .chain(working_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in keys {
        match (snapshot_map.get(&path), working_map.get(&path)) {
            (None, Some(_)) => added.push(path.to_string()),
            (Some(_), None) if incomplete => unknown.push(path.to_string()),
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
        unknown,
        unchanged_count,
        complete: !incomplete,
        warnings: Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataDiffEntry {
    size_bytes: u64,
    modified_at_utc: String,
}
