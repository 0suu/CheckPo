use crate::{CheckpointSummary, ProjectContext, Result, SnapshotId};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn apply_checkpoint_name_overrides(
    project: &ProjectContext,
    checkpoints: &mut [CheckpointSummary],
) {
    let (names, warnings) = read_checkpoint_name_overrides(project);
    if !names.is_empty() {
        for checkpoint in checkpoints.iter_mut() {
            if let Some(name) = names.get(checkpoint.checkpoint_id.as_str()) {
                checkpoint.name = name.clone();
            }
        }
    }
    attach_warnings(checkpoints, warnings);
}

pub(crate) fn read_checkpoint_name_overrides(
    project: &ProjectContext,
) -> (BTreeMap<String, String>, Vec<String>) {
    let path = crate::checkpoint_names_path(&project.repo_root);
    if !path.exists() {
        return (BTreeMap::new(), Vec::new());
    }
    let value = match crate::read_json::<Value>(&path) {
        Ok(value) => value,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!(
                    "checkpoint display names could not be loaded and were ignored: {error}"
                )],
            );
        }
    };
    let Some(object) = value.as_object() else {
        return (
            BTreeMap::new(),
            vec![format!(
                "{} is not a checkpoint display name map and was ignored",
                path.display()
            )],
        );
    };
    let mut names = BTreeMap::new();
    let mut warnings = Vec::new();
    for (checkpoint_id, value) in object {
        if let Err(error) = SnapshotId::parse(checkpoint_id) {
            warnings.push(format!(
                "checkpoint display name entry has an invalid id and was ignored: {error}"
            ));
            continue;
        }
        let Some(name) = value
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            warnings.push(format!(
                "checkpoint display name for {checkpoint_id} is empty or invalid and was ignored"
            ));
            continue;
        };
        names.insert(checkpoint_id.clone(), name.to_string());
    }
    (names, warnings)
}

pub(crate) fn write_checkpoint_name_overrides(
    project: &ProjectContext,
    names: &BTreeMap<String, String>,
) -> Result<()> {
    crate::write_json_atomic(&crate::checkpoint_names_path(&project.repo_root), names)
}

pub(crate) fn remove_checkpoint_name_override(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
) -> Result<Vec<String>> {
    let (mut names, warnings) = read_checkpoint_name_overrides(project);
    if names.remove(checkpoint_id.as_str()).is_some() {
        write_checkpoint_name_overrides(project, &names)?;
    }
    Ok(warnings)
}

fn attach_warnings(checkpoints: &mut [CheckpointSummary], warnings: Vec<String>) {
    if warnings.is_empty() {
        return;
    }
    if let Some(first) = checkpoints.first_mut() {
        first.warnings.extend(warnings);
    }
}
