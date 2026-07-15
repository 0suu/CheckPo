use crate::{CheckpointSummary, ProjectContext, Result, SnapshotId};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_CHECKPOINT_NAMES_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn apply_checkpoint_name_overrides(
    project: &ProjectContext,
    checkpoints: &mut [CheckpointSummary],
) -> Vec<String> {
    let (names, warnings) = read_checkpoint_name_overrides(project);
    if !names.is_empty() {
        for checkpoint in checkpoints.iter_mut() {
            if let Some(name) = names.get(checkpoint.checkpoint_id.as_str()) {
                checkpoint.name = name.clone();
            }
        }
    }
    attach_warnings(checkpoints, warnings.clone());
    warnings
}

pub(crate) fn read_checkpoint_name_overrides(
    project: &ProjectContext,
) -> (BTreeMap<String, String>, Vec<String>) {
    let path = crate::checkpoint_names_path(&project.repo_root);
    let value = match crate::storage::AnchoredRoot::open(&project.repo_root).and_then(|root| {
        let bytes = root.read_bytes_bounded_path(&path, MAX_CHECKPOINT_NAMES_FILE_BYTES)?;
        serde_json::from_slice::<Value>(&bytes).map_err(|error| crate::json_error(&path, error))
    }) {
        Ok(value) => value,
        Err(crate::CheckPoError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return (BTreeMap::new(), Vec::new());
        }
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
        if name.len() > crate::storage::merkle_codec::MAX_CHECKPOINT_NAME_BYTES {
            warnings.push(format!(
                "checkpoint display name for {checkpoint_id} exceeds {} bytes and was ignored",
                crate::storage::merkle_codec::MAX_CHECKPOINT_NAME_BYTES
            ));
            continue;
        }
        names.insert(checkpoint_id.clone(), name.to_string());
    }
    (names, warnings)
}

pub(crate) fn write_checkpoint_name_overrides(
    project: &ProjectContext,
    names: &BTreeMap<String, String>,
) -> Result<()> {
    crate::storage::AnchoredRoot::open(&project.repo_root)?
        .write_json_atomic(std::path::Path::new("refs/checkpoint_names.json"), names)
}

fn read_checkpoint_name_overrides_for_mutation(
    project: &ProjectContext,
) -> Result<BTreeMap<String, String>> {
    let (names, warnings) = read_checkpoint_name_overrides(project);
    if warnings.is_empty() {
        return Ok(names);
    }
    Err(crate::CheckPoError::Corruption(format!(
        "checkpoint display names cannot be modified until their metadata is repaired: {}",
        warnings.join("; ")
    )))
}

pub(crate) fn remove_checkpoint_name_override(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
) -> Result<Vec<String>> {
    let mut names = read_checkpoint_name_overrides_for_mutation(project)?;
    if names.remove(checkpoint_id.as_str()).is_some() {
        write_checkpoint_name_overrides(project, &names)?;
    }
    Ok(Vec::new())
}

fn attach_warnings(checkpoints: &mut [CheckpointSummary], warnings: Vec<String>) {
    if warnings.is_empty() {
        return;
    }
    if let Some(first) = checkpoints.first_mut() {
        first.warnings.extend(warnings);
    }
}
