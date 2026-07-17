#[cfg(debug_assertions)]
use super::chunk_store::store_built_manifest;
use super::chunk_store::{
    publish_snapshot_root, publish_snapshot_root_profiled, store_built_manifest_profiled_batched,
    RepositoryManifestSource,
};
use super::merkle_codec::{
    decode_root, encode_root, root_id, verify_root_id, Digest32, SnapshotRoot, Timestamp,
    MAX_ROOT_BYTES,
};
use super::snapshot_v2::{
    build_manifest, manifest_iter, portable_path_key_v1, validate_manifest_cached, BuiltManifest,
    ManifestEntry, ManifestValidationCache, SnapshotV2Error,
};
use super::*;
use crate::{SnapshotContent, SnapshotEntry, TrackedUnityFilePath};
use std::collections::{BTreeMap, BTreeSet};

const SNAPSHOT_SCHEMA_VERSION_V2: u32 = 2;
pub(crate) const MAX_SNAPSHOT_FILE_BYTES: u64 = MAX_ROOT_BYTES as u64;

pub(crate) struct PreparedSnapshot {
    pub(crate) snapshot_id: SnapshotId,
    root_bytes: Vec<u8>,
    manifest: BuiltManifest,
}

pub(crate) struct SnapshotRootHeader {
    pub(crate) project_id: ProjectId,
    pub(crate) parent_snapshot_id: Option<SnapshotId>,
    pub(crate) created_at_utc: String,
    pub(crate) name: String,
    pub(crate) tool_version: String,
    pub(crate) manifest: Option<super::merkle_codec::ManifestRef>,
    pub(crate) summary: super::merkle_codec::ManifestSummary,
}

pub fn read_latest_snapshot_id(repo_root: &Path) -> Result<Option<SnapshotId>> {
    let path = refs_latest_path(repo_root);
    let bytes = match AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, 128) {
        Ok(bytes) => bytes,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| CheckPoError::Corruption("refs/latest is not valid UTF-8".to_string()))?;
    Ok(Some(SnapshotId::parse(text.trim())?))
}

pub fn write_latest_snapshot_id(repo_root: &Path, snapshot_id: &SnapshotId) -> Result<()> {
    AnchoredRoot::open(repo_root)?
        .write_bytes_atomic(Path::new("refs/latest"), snapshot_id.as_str().as_bytes())
}

pub fn canonical_snapshot_bytes(snapshot: &SnapshotFile) -> Result<Vec<u8>> {
    Ok(prepare_snapshot(snapshot)?.root_bytes)
}

pub fn snapshot_id_from_bytes(bytes: &[u8]) -> SnapshotId {
    SnapshotId::from_digest_bytes(*root_id(bytes).as_bytes())
}

pub(crate) fn prepare_snapshot(snapshot: &SnapshotFile) -> Result<PreparedSnapshot> {
    let validation_id = SnapshotId::from_digest_bytes([0; 32]);
    validate_snapshot_file(&validation_id, snapshot)?;
    let entries = snapshot
        .files
        .iter()
        .map(snapshot_entry_to_manifest)
        .collect::<Result<Vec<_>>>()?;
    let manifest = build_manifest(entries).map_err(snapshot_v2_error)?;
    let root = SnapshotRoot {
        project_id: snapshot.project_id.uuid_bytes(),
        parent: snapshot
            .parent_snapshot_id
            .as_ref()
            .map(|parent| Digest32::from_bytes(parent.digest_bytes())),
        created: parse_timestamp(&validation_id, "createdAtUtc", &snapshot.created_at_utc)?,
        checkpoint_name: snapshot.name.clone(),
        tool_version: snapshot.tool_version.clone(),
        manifest: manifest.root,
        summary: manifest.summary,
    };
    let root_bytes = encode_root(&root).map_err(codec_error)?;
    let snapshot_id = snapshot_id_from_bytes(&root_bytes);
    Ok(PreparedSnapshot {
        snapshot_id,
        root_bytes,
        manifest,
    })
}

#[cfg(debug_assertions)]
pub(crate) fn store_prepared_snapshot_chunks(
    repo_root: &Path,
    prepared: &PreparedSnapshot,
) -> Result<()> {
    store_built_manifest(repo_root, &prepared.manifest)
}

pub(crate) fn store_prepared_snapshot_chunks_profiled_batched(
    repo_root: &Path,
    prepared: &PreparedSnapshot,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: &mut AnchoredParentSyncBatch,
    known_durable: &BTreeSet<super::merkle_codec::ManifestRef>,
) -> Result<()> {
    store_built_manifest_profiled_batched(
        repo_root,
        &prepared.manifest,
        recorder,
        sync_batch,
        known_durable,
    )
}

pub(crate) fn publish_prepared_snapshot_root(
    repo_root: &Path,
    prepared: &PreparedSnapshot,
) -> Result<()> {
    publish_snapshot_root(repo_root, &prepared.snapshot_id, &prepared.root_bytes)
}

pub(crate) fn publish_prepared_snapshot_root_profiled(
    repo_root: &Path,
    prepared: &PreparedSnapshot,
    recorder: &crate::checkpoint_metrics::ArtifactIoRecorder,
) -> Result<()> {
    publish_snapshot_root_profiled(
        repo_root,
        &prepared.snapshot_id,
        &prepared.root_bytes,
        Some(recorder),
    )
}

#[cfg(debug_assertions)]
pub(crate) fn save_snapshot(repo_root: &Path, snapshot: &SnapshotFile) -> Result<SnapshotId> {
    let prepared = prepare_snapshot(snapshot)?;
    store_prepared_snapshot_chunks(repo_root, &prepared)?;
    publish_prepared_snapshot_root(repo_root, &prepared)?;
    Ok(prepared.snapshot_id)
}

pub fn load_snapshot(repo_root: &Path, snapshot_id: &SnapshotId) -> Result<SnapshotFile> {
    load_snapshot_with_warnings(repo_root, snapshot_id).map(|(snapshot, _warnings)| snapshot)
}

pub(crate) fn load_snapshot_with_warnings(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
) -> Result<(SnapshotFile, Vec<String>)> {
    let bytes = read_snapshot_bytes(repo_root, snapshot_id)?;
    let snapshot = decode_snapshot_root_bytes(repo_root, snapshot_id, &bytes)?;
    Ok((snapshot, Vec::new()))
}

pub(crate) fn decode_snapshot_root_bytes(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    bytes: &[u8],
) -> Result<SnapshotFile> {
    decode_snapshot_root_bytes_with_manifest_references(repo_root, snapshot_id, bytes)
        .map(|(snapshot, _)| snapshot)
}

pub(crate) fn decode_snapshot_root_bytes_with_manifest_references(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    bytes: &[u8],
) -> Result<(SnapshotFile, BTreeSet<super::merkle_codec::ManifestRef>)> {
    let root = decode_snapshot_root_header(snapshot_id, bytes)?;
    let source = RepositoryManifestSource::new(repo_root)?;
    let mut validation_cache = ManifestValidationCache::default();
    validate_manifest_cached(&source, root.manifest, root.summary, &mut validation_cache)
        .map_err(snapshot_v2_error)?;
    let validated_references = validation_cache.validated_references();
    let mut files = manifest_iter(&source, root.manifest)
        .map(|entry| manifest_entry_to_snapshot(snapshot_id, entry.map_err(snapshot_v2_error)?))
        .collect::<Result<Vec<_>>>()?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let snapshot = SnapshotFile {
        schema_version: SNAPSHOT_SCHEMA_VERSION_V2,
        project_id: root.project_id,
        parent_snapshot_id: root.parent_snapshot_id,
        created_at_utc: root.created_at_utc,
        name: root.name,
        tool_version: root.tool_version,
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files,
    };
    validate_snapshot_file(snapshot_id, &snapshot)?;
    Ok((snapshot, validated_references))
}

pub(crate) fn load_snapshot_root_header(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
) -> Result<SnapshotRootHeader> {
    let bytes = read_snapshot_bytes(repo_root, snapshot_id)?;
    decode_snapshot_root_header(snapshot_id, &bytes)
}

fn decode_snapshot_root_header(
    snapshot_id: &SnapshotId,
    bytes: &[u8],
) -> Result<SnapshotRootHeader> {
    verify_root_id(Digest32::from_bytes(snapshot_id.digest_bytes()), bytes).map_err(codec_error)?;
    let root = decode_root(bytes).map_err(codec_error)?;
    if encode_root(&root).map_err(codec_error)? != bytes {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot root is not canonically encoded"
        )));
    }
    Ok(SnapshotRootHeader {
        project_id: ProjectId::from_uuid_bytes(root.project_id),
        parent_snapshot_id: root
            .parent
            .map(|parent| SnapshotId::from_digest_bytes(*parent.as_bytes())),
        created_at_utc: format_timestamp(snapshot_id, "createdAtUtc", root.created)?,
        name: root.checkpoint_name,
        tool_version: root.tool_version,
        manifest: root.manifest,
        summary: root.summary,
    })
}

pub(crate) fn read_snapshot_bytes(repo_root: &Path, snapshot_id: &SnapshotId) -> Result<Vec<u8>> {
    let path = snapshot_path(repo_root, snapshot_id);
    match AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, MAX_SNAPSHOT_FILE_BYTES) {
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(CheckPoError::SnapshotNotFound(snapshot_id.to_string()))
        }
        result => result,
    }
}

pub fn load_project_snapshot(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
) -> Result<SnapshotFile> {
    load_project_snapshot_with_warnings(project, snapshot_id).map(|(snapshot, _warnings)| snapshot)
}

pub(crate) fn load_project_snapshot_with_manifest_references(
    project: &ProjectContext,
    snapshot_id: &SnapshotId,
) -> Result<(SnapshotFile, BTreeSet<super::merkle_codec::ManifestRef>)> {
    let bytes = read_snapshot_bytes(&project.repo_root, snapshot_id)?;
    let (snapshot, references) = decode_snapshot_root_bytes_with_manifest_references(
        &project.repo_root,
        snapshot_id,
        &bytes,
    )?;
    if snapshot.project_id != project.project_id {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot project id does not match current project"
        )));
    }
    Ok((snapshot, references))
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
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION_V2 {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "snapshot schema".to_string(),
            found: snapshot.schema_version,
            supported: SNAPSHOT_SCHEMA_VERSION_V2,
        });
    }
    parse_timestamp(snapshot_id, "createdAtUtc", &snapshot.created_at_utc)?;
    if snapshot.name.trim().is_empty() || snapshot.name.trim() != snapshot.name {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot name is empty or not trimmed"
        )));
    }
    if snapshot.tracked_roots != ["Assets", "Packages", "ProjectSettings"] {
        return Err(CheckPoError::Corruption(format!(
            "{snapshot_id}: snapshot tracked roots are invalid"
        )));
    }
    let mut portable_paths = BTreeMap::<Vec<u8>, &TrackedUnityFilePath>::new();
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
        let portable_key = portable_path_key_v1(file.path.as_str()).map_err(snapshot_v2_error)?;
        if let Some(existing) = portable_paths.insert(portable_key, &file.path) {
            if existing != &file.path {
                return Err(CheckPoError::Corruption(format!(
                    "{snapshot_id}: snapshot paths collide on a case/Unicode-normalization-insensitive filesystem: {existing} and {}",
                    file.path
                )));
            }
        }
        parse_timestamp(snapshot_id, "modifiedAtUtc", &file.modified_at_utc)?;
        if file.size_bytes != file.content_size_bytes() {
            return Err(CheckPoError::Corruption(format!(
                "{snapshot_id}: size mismatch for {}",
                file.path
            )));
        }
    }
    for (portable_path, original_path) in &portable_paths {
        for (index, byte) in portable_path.iter().enumerate() {
            if *byte != b'/' {
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

pub fn list_snapshot_ids(repo_root: &Path) -> Result<Vec<SnapshotId>> {
    let dir = snapshots_dir(repo_root);
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(&dir, error)),
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err(CheckPoError::Corruption(format!(
                "snapshot root directory is unsafe: {}",
                dir.display()
            )))
        }
        Ok(_) => {}
    }
    let mut ids = Vec::new();
    for first in sorted_directory_entries(&dir)? {
        let first_path = first.path();
        let first_name = first.file_name().to_string_lossy().to_string();
        if !is_lower_hex_shard(&first_name) {
            continue;
        }
        ensure_regular_directory_no_follow(&first_path)?;
        for second in sorted_directory_entries(&first_path)? {
            let second_path = second.path();
            let second_name = second.file_name().to_string_lossy().to_string();
            if !is_lower_hex_shard(&second_name) {
                continue;
            }
            ensure_regular_directory_no_follow(&second_path)?;
            for entry in sorted_directory_entries(&second_path)? {
                let path = entry.path();
                let metadata =
                    fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
                if metadata_is_link_or_reparse(&metadata) {
                    return Err(CheckPoError::Corruption(format!(
                        "snapshot root entry is a link or reparse point: {}",
                        path.display()
                    )));
                }
                if !metadata.is_file()
                    || path.extension().and_then(|value| value.to_str()) != Some("root")
                {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let Ok(id) = SnapshotId::parse(stem) else {
                    continue;
                };
                if id.as_str()[0..2] == first_name && id.as_str()[2..4] == second_name {
                    ids.push(id);
                }
            }
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn snapshot_entry_to_manifest(entry: &SnapshotEntry) -> Result<ManifestEntry> {
    Ok(ManifestEntry {
        exact_path: entry.path.to_string(),
        size_bytes: entry.size_bytes,
        modified: parse_timestamp(
            &SnapshotId::from_digest_bytes([0; 32]),
            "modifiedAtUtc",
            &entry.modified_at_utc,
        )?,
        object_id: Digest32::from_bytes(entry.content_hash().digest_bytes()),
    })
}

fn manifest_entry_to_snapshot(
    snapshot_id: &SnapshotId,
    entry: ManifestEntry,
) -> Result<SnapshotEntry> {
    let path = TrackedUnityFilePath::parse(&entry.exact_path)?;
    Ok(SnapshotEntry {
        path,
        size_bytes: entry.size_bytes,
        modified_at_utc: format_timestamp(snapshot_id, "modifiedAtUtc", entry.modified)?,
        content: SnapshotContent::Whole {
            hash: ObjectId::from_digest_bytes(*entry.object_id.as_bytes()),
            size_bytes: entry.size_bytes,
        },
    })
}

fn parse_timestamp(snapshot_id: &SnapshotId, field: &str, value: &str) -> Result<Timestamp> {
    let parsed = DateTime::parse_from_rfc3339(value).map_err(|_| {
        CheckPoError::Corruption(format!("{snapshot_id}: invalid {field}: {value}"))
    })?;
    Timestamp::new(parsed.timestamp(), parsed.timestamp_subsec_nanos()).map_err(codec_error)
}

fn format_timestamp(snapshot_id: &SnapshotId, field: &str, value: Timestamp) -> Result<String> {
    let value = DateTime::<Utc>::from_timestamp(value.unix_seconds, value.nanoseconds).ok_or_else(
        || {
            CheckPoError::Corruption(format!(
                "{snapshot_id}: {field} is outside the supported timestamp range"
            ))
        },
    )?;
    Ok(canonical_utc(value))
}

fn codec_error(error: super::merkle_codec::CodecError) -> CheckPoError {
    CheckPoError::Corruption(error.to_string())
}

fn snapshot_v2_error(error: SnapshotV2Error) -> CheckPoError {
    CheckPoError::Corruption(error.to_string())
}

fn sorted_directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| io_error(path, error))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| io_error(path, error))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn is_lower_hex_shard(value: &str) -> bool {
    value.len() == 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
