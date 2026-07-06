use super::*;

pub fn read_latest_snapshot_id(repo_root: &Path) -> Result<Option<SnapshotId>> {
    let path = refs_latest_path(repo_root);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|error| io_error(&path, error))?;
    Ok(Some(SnapshotId::parse(text.trim())?))
}

pub fn write_latest_snapshot_id(repo_root: &Path, snapshot_id: &SnapshotId) -> Result<()> {
    write_text_atomic(&refs_latest_path(repo_root), snapshot_id.as_str())
}

pub fn canonical_snapshot_bytes(snapshot: &SnapshotFile) -> Result<Vec<u8>> {
    serde_json::to_vec(snapshot).map_err(|error| CheckPoError::Unexpected(error.to_string()))
}

pub fn snapshot_id_from_bytes(bytes: &[u8]) -> SnapshotId {
    let hash = blake3::hash(bytes);
    SnapshotId::parse(hash.to_hex().as_ref()).expect("BLAKE3 produces lowercase 64 hex")
}

pub fn save_snapshot(repo_root: &Path, snapshot: &SnapshotFile) -> Result<SnapshotId> {
    let bytes = canonical_snapshot_bytes(snapshot)?;
    let snapshot_id = snapshot_id_from_bytes(&bytes);
    let path = snapshot_path(repo_root, &snapshot_id);
    write_bytes_atomic(&path, &bytes)?;
    Ok(snapshot_id)
}

pub fn load_snapshot(repo_root: &Path, snapshot_id: &SnapshotId) -> Result<SnapshotFile> {
    let path = snapshot_path(repo_root, snapshot_id);
    if !path.is_file() {
        return Err(CheckPoError::SnapshotNotFound(snapshot_id.to_string()));
    }
    let bytes = fs::read(&path).map_err(|error| io_error(&path, error))?;
    let snapshot: SnapshotFile =
        serde_json::from_slice(&bytes).map_err(|error| json_error(&path, error))?;
    validate_snapshot_file(snapshot_id, &snapshot)?;
    let actual = snapshot_id_from_bytes(&canonical_snapshot_bytes(&snapshot)?);
    if &actual != snapshot_id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot filename digest mismatch: expected {snapshot_id}, got {actual}"
        )));
    }
    Ok(snapshot)
}

pub fn load_project_snapshot(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
) -> Result<SnapshotFile> {
    let snapshot = load_snapshot(&project.repo_root, snapshot_id)?;
    if snapshot.project_id != project.project_id {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot project id does not match current project"
        )));
    }
    Ok(snapshot)
}

pub fn validate_snapshot_file(snapshot_id: &SnapshotId, snapshot: &SnapshotFile) -> Result<()> {
    if snapshot.schema_version != 1 {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: unsupported snapshot schema"
        )));
    }
    parse_rfc3339_snapshot_time(snapshot_id, "createdAtUtc", &snapshot.created_at_utc)?;
    if snapshot.name.trim().is_empty() {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot name is empty"
        )));
    }
    if snapshot.tracked_roots != ["Assets", "Packages", "ProjectSettings"] {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot tracked roots are invalid"
        )));
    }
    for window in snapshot.files.windows(2) {
        if window[0].path == window[1].path {
            return Err(CheckPoError::Corruption(format!(
                "{snapshot_id}: snapshot contains duplicate paths"
            )));
        }
        if window[0].path > window[1].path {
            return Err(CheckPoError::Corruption(format!(
                "{snapshot_id}: snapshot files are not sorted"
            )));
        }
    }
    for file in &snapshot.files {
        parse_rfc3339_snapshot_time(snapshot_id, "modifiedAtUtc", &file.modified_at_utc)?;
        if file.size_bytes != file.content_size_bytes() {
            return Err(CheckPoError::Corruption(format!(
                "{snapshot_id}: size mismatch for {}",
                file.path
            )));
        }
    }
    Ok(())
}

fn parse_rfc3339_snapshot_time(snapshot_id: &SnapshotId, field: &str, value: &str) -> Result<()> {
    DateTime::parse_from_rfc3339(value).map_err(|_| {
        CheckPoError::Corruption(format!("{snapshot_id}: invalid {field}: {value}"))
    })?;
    Ok(())
}

pub fn list_snapshot_ids(repo_root: &Path) -> Result<Vec<SnapshotId>> {
    let dir = snapshots_dir(repo_root);
    let mut ids = Vec::new();
    if !dir.exists() {
        return Ok(ids);
    }
    for entry in fs::read_dir(&dir).map_err(|error| io_error(&dir, error))? {
        let entry = entry.map_err(|error| io_error(&dir, error))?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|v| v.to_str()) else {
            continue;
        };
        if let Ok(id) = SnapshotId::parse(stem) {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}
