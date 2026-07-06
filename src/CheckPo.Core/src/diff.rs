use crate::{
    load_project, load_project_snapshot, scan_project_for_checkpoint, DiffOptions, DiffResult,
    ObjectId, Result, SnapshotId, TrackedUnityFilePath,
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
    }
}
