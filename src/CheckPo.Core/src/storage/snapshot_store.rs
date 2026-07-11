use super::*;
use crate::TrackedUnityFilePath;
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

const SNAPSHOT_SCHEMA_VERSION_V1: u32 = 1;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotEnvelope {
    schema_version: u32,
}

// This DTO is the frozen decoder for canonical-json-v1. Keep its field names
// and shape aligned with the V1 encoder in canonical_snapshot_bytes.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotFileV1 {
    schema_version: u32,
    project_id: ProjectId,
    parent_snapshot_id: Option<SnapshotId>,
    created_at_utc: String,
    name: String,
    tool_version: String,
    tracked_roots: Vec<String>,
    files: Vec<SnapshotEntryV1>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotEntryV1 {
    path: crate::TrackedUnityFilePath,
    size_bytes: u64,
    modified_at_utc: String,
    content: SnapshotContentV1,
}

#[derive(serde::Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum SnapshotContentV1 {
    Whole { hash: ObjectId, size_bytes: u64 },
}

impl From<SnapshotFileV1> for SnapshotFile {
    fn from(value: SnapshotFileV1) -> Self {
        Self {
            schema_version: value.schema_version,
            project_id: value.project_id,
            parent_snapshot_id: value.parent_snapshot_id,
            created_at_utc: value.created_at_utc,
            name: value.name,
            tool_version: value.tool_version,
            tracked_roots: value.tracked_roots,
            files: value.files.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<SnapshotEntryV1> for crate::SnapshotEntry {
    fn from(value: SnapshotEntryV1) -> Self {
        Self {
            path: value.path,
            size_bytes: value.size_bytes,
            modified_at_utc: value.modified_at_utc,
            content: value.content.into(),
        }
    }
}

impl From<SnapshotContentV1> for crate::SnapshotContent {
    fn from(value: SnapshotContentV1) -> Self {
        match value {
            SnapshotContentV1::Whole { hash, size_bytes } => Self::Whole { hash, size_bytes },
        }
    }
}

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
    // SnapshotFile field order and serde names are the canonical-json-v1
    // encoder contract. The golden V1 test protects this exact byte sequence.
    serde_json::to_vec(snapshot).map_err(|error| CheckPoError::Unexpected(error.to_string()))
}

pub fn snapshot_id_from_bytes(bytes: &[u8]) -> SnapshotId {
    let hash = blake3::hash(bytes);
    SnapshotId::parse(hash.to_hex().as_ref()).expect("BLAKE3 produces lowercase 64 hex")
}

pub fn save_snapshot(repo_root: &Path, snapshot: &SnapshotFile) -> Result<SnapshotId> {
    let bytes = canonical_snapshot_bytes(snapshot)?;
    let snapshot_id = snapshot_id_from_bytes(&bytes);
    validate_snapshot_file(&snapshot_id, snapshot)?;
    let path = snapshot_path(repo_root, &snapshot_id);
    write_bytes_atomic(&path, &bytes)?;
    Ok(snapshot_id)
}

pub fn load_snapshot(repo_root: &Path, snapshot_id: &SnapshotId) -> Result<SnapshotFile> {
    load_snapshot_with_warnings(repo_root, snapshot_id).map(|(snapshot, _warnings)| snapshot)
}

pub(crate) fn load_snapshot_with_warnings(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
) -> Result<(SnapshotFile, Vec<String>)> {
    let path = snapshot_path(repo_root, snapshot_id);
    if !path.is_file() {
        return Err(CheckPoError::SnapshotNotFound(snapshot_id.to_string()));
    }
    let bytes = fs::read(&path).map_err(|error| io_error(&path, error))?;
    let actual = snapshot_id_from_bytes(&bytes);
    if &actual != snapshot_id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot filename digest mismatch: expected {snapshot_id}, got {actual}"
        )));
    }
    let envelope: SnapshotEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| json_error(&path, error))?;
    if envelope.schema_version > SNAPSHOT_SCHEMA_VERSION_V1 {
        crate::diagnostics::log_warning(
            "snapshot-load",
            &format!(
                "{snapshot_id} uses unsupported schema {}",
                envelope.schema_version
            ),
        );
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "snapshot schema".to_string(),
            found: envelope.schema_version,
            supported: SNAPSHOT_SCHEMA_VERSION_V1,
        });
    }
    if envelope.schema_version != SNAPSHOT_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: unsupported snapshot schema {}",
            envelope.schema_version
        )));
    }
    let snapshot = serde_json::from_slice::<SnapshotFileV1>(&bytes)
        .map(SnapshotFile::from)
        .map_err(|error| json_error(&path, error))?;
    validate_snapshot_file(snapshot_id, &snapshot)?;
    let canonical = canonical_snapshot_bytes(&snapshot)?;
    let mut warnings = Vec::new();
    if canonical != bytes {
        let warning = format!(
            "{snapshot_id}: snapshot bytes are valid V1 but differ from this CheckPo's canonical JSON output"
        );
        crate::diagnostics::log_warning("snapshot-load", &warning);
        warnings.push(warning);
    }
    Ok((snapshot, warnings))
}

pub fn load_project_snapshot(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
) -> Result<SnapshotFile> {
    load_project_snapshot_with_warnings(project, snapshot_id).map(|(snapshot, _warnings)| snapshot)
}

pub(crate) fn load_project_snapshot_with_warnings(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
) -> Result<(SnapshotFile, Vec<String>)> {
    let (snapshot, warnings) = load_snapshot_with_warnings(&project.repo_root, snapshot_id)?;
    if snapshot.project_id != project.project_id {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot project id does not match current project"
        )));
    }
    Ok((snapshot, warnings))
}

pub fn validate_snapshot_file(snapshot_id: &SnapshotId, snapshot: &SnapshotFile) -> Result<()> {
    if snapshot.schema_version > SNAPSHOT_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "snapshot schema".to_string(),
            found: snapshot.schema_version,
            supported: SNAPSHOT_SCHEMA_VERSION_V1,
        });
    }
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: unsupported snapshot schema {}",
            snapshot.schema_version
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
    let mut portable_paths = BTreeMap::<String, &TrackedUnityFilePath>::new();
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
        let portable_key = portable_path_key(&file.path);
        if let Some(existing) = portable_paths.insert(portable_key, &file.path) {
            if existing != &file.path {
                return Err(CheckPoError::Corruption(format!(
                    "{snapshot_id}: snapshot paths collide on a case/Unicode-normalization-insensitive filesystem: {existing} and {}",
                    file.path
                )));
            }
        }
        parse_rfc3339_snapshot_time(snapshot_id, "modifiedAtUtc", &file.modified_at_utc)?;
        if file.size_bytes != file.content_size_bytes() {
            return Err(CheckPoError::Corruption(format!(
                "{snapshot_id}: size mismatch for {}",
                file.path
            )));
        }
    }
    for (portable_path, original_path) in &portable_paths {
        for (index, byte) in portable_path.bytes().enumerate() {
            if byte != b'/' {
                continue;
            }
            let ancestor = &portable_path[..index];
            if let Some(existing) = portable_paths.get(ancestor) {
                return Err(CheckPoError::Corruption(format!(
                    "{snapshot_id}: snapshot contains a file/directory ancestor conflict: {existing} and {original_path}"
                )));
            }
        }
    }
    Ok(())
}

fn portable_path_key(path: &TrackedUnityFilePath) -> String {
    let lowered = path
        .as_str()
        .nfc()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    lowered.nfc().collect()
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
