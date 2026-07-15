use crate::storage::merkle_codec::ManifestRef;
use crate::storage::snapshot_v2::{
    load_chunk, validate_manifest_cached, DecodedChunk, ManifestChunkSource,
    ManifestValidationCache,
};
use crate::{
    list_snapshot_ids, load_project, load_repo_config, object_path, read_latest_snapshot_id,
    report_operation_progress, snapshots_dir, CancellationToken, ObjectId, OperationProgress,
    Result, SnapshotFile, SnapshotId, TrackedUnityFilePath, VerificationResult,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const MAX_VERIFICATION_ERRORS: usize = 1024;
const MAX_MANIFEST_VALIDATION_CACHE_CHUNKS: usize = 100_000;
const MAX_MANIFEST_VALIDATION_CACHE_BYTES: usize = 64 * 1024 * 1024;

pub fn verify_project(project_path: impl AsRef<Path>, full: bool) -> Result<VerificationResult> {
    verify_project_with_progress_and_cancellation(project_path, full, None, None)
}

pub fn verify_project_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<VerificationResult> {
    let project = load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "verify-project")?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut result = VerificationResult {
        is_valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    match load_repo_config(&project.repo_root, &project.project_id) {
        Ok(_) => {}
        Err(error) => push_verification_error(&mut result, error.to_string()),
    }
    warn_invalid_snapshot_root_entries(&project.repo_root, &mut result)?;
    if full {
        if let Err(error) = crate::storage::validate_physical_snapshot_inventory(
            &project.repo_root,
            &project.project_id,
        ) {
            push_verification_error(&mut result, error.to_string());
        }
    }
    let snapshot_ids = list_snapshot_ids(&project.repo_root)?;
    let snapshot_total = snapshot_ids.len();
    let manifest_source = crate::storage::RepositoryManifestSource::new(&project.repo_root)?;
    let mut manifest_validation_cache = ManifestValidationCache::default();
    let mut verified_manifest_chunks = BTreeSet::new();
    let mut referenced_objects = BTreeMap::new();
    for (index, snapshot_id) in snapshot_ids.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        match collect_snapshot_references(
            &project,
            snapshot_id,
            &manifest_source,
            &mut manifest_validation_cache,
            &mut verified_manifest_chunks,
            &mut referenced_objects,
            cancellation,
            &mut result,
        ) {
            Ok(()) => {}
            Err(crate::CheckPoError::Cancelled) => return Err(crate::CheckPoError::Cancelled),
            Err(error) => push_verification_error(&mut result, error.to_string()),
        }
        if manifest_validation_cache.len() >= MAX_MANIFEST_VALIDATION_CACHE_CHUNKS
            || manifest_validation_cache.estimated_heap_bytes()
                >= MAX_MANIFEST_VALIDATION_CACHE_BYTES
        {
            manifest_validation_cache = ManifestValidationCache::default();
        }
        report_operation_progress(
            progress,
            "verifySnapshots",
            index + 1,
            snapshot_total,
            Some(snapshot_id.to_string()),
        );
    }
    let mut anchored_object_shards = HashMap::new();
    verify_referenced_objects(
        &anchored_repo,
        &project.repo_root,
        &referenced_objects,
        full,
        progress,
        cancellation,
        &mut anchored_object_shards,
        &mut result,
    )?;
    match read_latest_snapshot_id(&project.repo_root) {
        Ok(Some(latest)) => {
            if let Err(error) =
                crate::storage::load_snapshot_root_header(&project.repo_root, &latest)
            {
                push_verification_error(
                    &mut result,
                    format!("refs/latest points to invalid snapshot {latest}: {error}"),
                );
            }
        }
        Ok(None) => {}
        Err(error) => push_verification_error(&mut result, error.to_string()),
    }
    anchored_repo.verify_root_binding()?;
    crate::ensure_not_cancelled(cancellation)?;
    result.is_valid = result.errors.is_empty();
    Ok(result)
}

#[derive(Clone)]
struct ReferencedObject {
    expected_size_bytes: u64,
    first_snapshot_id: SnapshotId,
    first_path: TrackedUnityFilePath,
}

#[allow(clippy::too_many_arguments)]
fn collect_snapshot_references<S: ManifestChunkSource>(
    project: &crate::ProjectContext,
    snapshot_id: &SnapshotId,
    source: &S,
    validation_cache: &mut ManifestValidationCache,
    verified_manifest_chunks: &mut BTreeSet<ManifestRef>,
    referenced_objects: &mut BTreeMap<ObjectId, ReferencedObject>,
    cancellation: Option<&CancellationToken>,
    result: &mut VerificationResult,
) -> Result<()> {
    let root = crate::storage::load_snapshot_root_header(&project.repo_root, snapshot_id)?;
    validate_manifest_cached(source, root.manifest, root.summary, validation_cache)
        .map_err(|error| crate::CheckPoError::Corruption(format!("{snapshot_id}: {error}")))?;
    if root.project_id != project.project_id {
        push_verification_error(
            result,
            format!("{snapshot_id}: snapshot project id does not match current project"),
        );
    }
    collect_manifest_references(
        source,
        root.manifest,
        snapshot_id,
        verified_manifest_chunks,
        referenced_objects,
        cancellation,
        result,
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_manifest_references<S: ManifestChunkSource>(
    source: &S,
    root: Option<ManifestRef>,
    snapshot_id: &SnapshotId,
    verified_manifest_chunks: &mut BTreeSet<ManifestRef>,
    referenced_objects: &mut BTreeMap<ObjectId, ReferencedObject>,
    cancellation: Option<&CancellationToken>,
    result: &mut VerificationResult,
) -> Result<()> {
    let mut pending = root.into_iter().collect::<Vec<_>>();
    while let Some(reference) = pending.pop() {
        crate::ensure_not_cancelled(cancellation)?;
        if verified_manifest_chunks.contains(&reference) {
            continue;
        }
        let decoded = load_chunk(source, reference)
            .map_err(|error| crate::CheckPoError::Corruption(format!("{snapshot_id}: {error}")))?;
        verified_manifest_chunks.insert(reference);
        match decoded {
            DecodedChunk::Node(node, _) => {
                pending.extend(node.children.into_iter().map(|child| child.child));
            }
            DecodedChunk::Leaf(leaf, _) => {
                for (index, entry) in leaf.entries.into_iter().enumerate() {
                    if index.is_multiple_of(256) {
                        crate::ensure_not_cancelled(cancellation)?;
                    }
                    let path = TrackedUnityFilePath::parse(&entry.exact_path)?;
                    let object_id = ObjectId::from_digest_bytes(*entry.object_id.as_bytes());
                    match referenced_objects.entry(object_id.clone()) {
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            slot.insert(ReferencedObject {
                                expected_size_bytes: entry.size_bytes,
                                first_snapshot_id: snapshot_id.clone(),
                                first_path: path,
                            });
                        }
                        std::collections::btree_map::Entry::Occupied(slot)
                            if slot.get().expected_size_bytes != entry.size_bytes =>
                        {
                            push_verification_error(
                                result,
                                format!(
                                    "{snapshot_id}: object {object_id} has conflicting expected sizes: {} in snapshot {}, {} for {path}",
                                    slot.get().expected_size_bytes,
                                    slot.get().first_snapshot_id,
                                    entry.size_bytes,
                                ),
                            );
                        }
                        std::collections::btree_map::Entry::Occupied(_) => {}
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_referenced_objects(
    anchored_repo: &crate::storage::AnchoredRoot,
    repo_root: &Path,
    referenced_objects: &BTreeMap<ObjectId, ReferencedObject>,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    anchored_shards: &mut HashMap<PathBuf, crate::storage::AnchoredParent>,
    result: &mut VerificationResult,
) -> Result<()> {
    let total = referenced_objects.len();
    for (index, (object_id, reference)) in referenced_objects.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        verify_referenced_object(
            anchored_repo,
            repo_root,
            object_id,
            reference,
            full,
            cancellation,
            anchored_shards,
            result,
        )?;
        report_operation_progress(
            progress,
            "verifyObjects",
            index + 1,
            total,
            Some(reference.first_path.to_string()),
        );
        crate::ensure_not_cancelled(cancellation)?;
    }
    verify_anchored_shard_bindings(anchored_repo, anchored_shards)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_referenced_object(
    anchored_repo: &crate::storage::AnchoredRoot,
    repo_root: &Path,
    object_id: &ObjectId,
    reference: &ReferencedObject,
    full: bool,
    cancellation: Option<&CancellationToken>,
    anchored_shards: &mut HashMap<PathBuf, crate::storage::AnchoredParent>,
    result: &mut VerificationResult,
) -> Result<()> {
    let object = object_path(repo_root, object_id);
    match verify_object_with_cancellation(
        anchored_repo,
        repo_root,
        &object,
        object_id,
        reference.expected_size_bytes,
        full,
        cancellation,
        anchored_shards,
    ) {
        Ok(()) => {}
        Err(crate::CheckPoError::Cancelled) => return Err(crate::CheckPoError::Cancelled),
        Err(error) => push_verification_error(
            result,
            format!(
                "{}: object {object_id} for {}: {error}",
                reference.first_snapshot_id, reference.first_path
            ),
        ),
    }
    Ok(())
}

pub fn verify_checkpoint(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    full: bool,
) -> Result<VerificationResult> {
    verify_checkpoint_with_progress_and_cancellation(project_path, checkpoint_id, full, None, None)
}

pub fn verify_checkpoint_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<VerificationResult> {
    let project = load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "verify-checkpoint")?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let snapshot_id = SnapshotId::parse(checkpoint_id)?;
    let mut result = VerificationResult {
        is_valid: true,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    let mut anchored_object_shards = HashMap::new();
    let mut verified_objects = HashMap::new();
    match crate::storage::load_project_snapshot_with_warnings(&project, &snapshot_id) {
        Ok((snapshot, warnings)) => {
            result.warnings.extend(warnings);
            let mut state = VerifyObjectState {
                anchored_repo: &anchored_repo,
                result: &mut result,
                anchored_shards: &mut anchored_object_shards,
                verified_objects: &mut verified_objects,
            };
            verify_snapshot(
                &project.repo_root,
                &snapshot_id,
                &snapshot,
                full,
                progress,
                cancellation,
                &mut state,
            )?
        }
        Err(error) => push_verification_error(&mut result, error.to_string()),
    }
    verify_anchored_shard_bindings(&anchored_repo, &anchored_object_shards)?;
    crate::ensure_not_cancelled(cancellation)?;
    result.is_valid = result.errors.is_empty();
    Ok(result)
}

struct VerifyObjectState<'a> {
    anchored_repo: &'a crate::storage::AnchoredRoot,
    result: &'a mut VerificationResult,
    anchored_shards: &'a mut HashMap<PathBuf, crate::storage::AnchoredParent>,
    verified_objects: &'a mut HashMap<ObjectId, VerifiedObject>,
}

struct VerifiedObject {
    expected_size_bytes: u64,
    first_snapshot_id: SnapshotId,
}

fn verify_snapshot(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    snapshot: &SnapshotFile,
    full: bool,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    state: &mut VerifyObjectState<'_>,
) -> Result<()> {
    for (index, file) in snapshot.files.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        if let Err(error) = TrackedUnityFilePath::parse(file.path.as_str()) {
            push_verification_error(state.result, error.to_string());
        }
        if let Err(error) = ObjectId::parse(file.content_hash().as_str()) {
            push_verification_error(state.result, error.to_string());
        }
        if let Some(verified) = state.verified_objects.get(file.content_hash()) {
            if verified.expected_size_bytes != file.size_bytes {
                push_verification_error(
                    state.result,
                    format!(
                        "{snapshot_id}: object {} has conflicting expected sizes: {} in snapshot {}, {} for {}",
                        file.content_hash(),
                        verified.expected_size_bytes,
                        verified.first_snapshot_id,
                        file.size_bytes,
                        file.path
                    ),
                );
            }
            report_operation_progress(
                progress,
                "verifyObjects",
                index + 1,
                snapshot.files.len(),
                Some(file.path.to_string()),
            );
            continue;
        }
        state.verified_objects.insert(
            file.content_hash().clone(),
            VerifiedObject {
                expected_size_bytes: file.size_bytes,
                first_snapshot_id: snapshot_id.clone(),
            },
        );
        let object = object_path(repo_root, file.content_hash());
        match verify_object_with_cancellation(
            state.anchored_repo,
            repo_root,
            &object,
            file.content_hash(),
            file.size_bytes,
            full,
            cancellation,
            state.anchored_shards,
        ) {
            Ok(()) => {}
            Err(crate::CheckPoError::Cancelled) => return Err(crate::CheckPoError::Cancelled),
            Err(error) => push_verification_error(
                state.result,
                format!(
                    "{snapshot_id}: object {} for {}: {error}",
                    file.content_hash(),
                    file.path
                ),
            ),
        }
        report_operation_progress(
            progress,
            "verifyObjects",
            index + 1,
            snapshot.files.len(),
            Some(file.path.to_string()),
        );
        crate::ensure_not_cancelled(cancellation)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_object_with_cancellation(
    anchored_repo: &crate::storage::AnchoredRoot,
    repo_root: &Path,
    path: &Path,
    expected: &ObjectId,
    size_bytes: u64,
    full: bool,
    cancellation: Option<&CancellationToken>,
    anchored_shards: &mut HashMap<PathBuf, crate::storage::AnchoredParent>,
) -> Result<()> {
    crate::ensure_not_cancelled(cancellation)?;
    let relative = path.strip_prefix(repo_root).map_err(|_| {
        crate::CheckPoError::Corruption(format!(
            "object path is outside anchored repository {}: {}",
            repo_root.display(),
            path.display()
        ))
    })?;
    let parent_relative = relative.parent().ok_or_else(|| {
        crate::CheckPoError::Corruption(format!("invalid object path: {}", path.display()))
    })?;
    let leaf = relative.file_name().ok_or_else(|| {
        crate::CheckPoError::Corruption(format!("invalid object path: {}", path.display()))
    })?;
    let parent = match anchored_shards.entry(parent_relative.to_path_buf()) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(anchored_repo.open_directory(parent_relative, false)?)
        }
    };
    let mut input = parent.open_file(leaf)?;
    let (actual_size, actual_hash) = if full {
        let hashed = input.hash_with_cancellation(cancellation)?;
        (hashed.metadata.len(), Some(hashed.object_id))
    } else {
        (input.metadata()?.len(), None)
    };
    parent.verify_file_binding(leaf, &input)?;
    crate::ensure_not_cancelled(cancellation)?;

    if actual_size != size_bytes {
        return Err(crate::CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            path.display(),
            size_bytes,
            actual_size
        )));
    }
    if actual_hash
        .as_ref()
        .is_some_and(|actual| actual != expected)
    {
        let actual = actual_hash.expect("checked as present");
        return Err(crate::CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected,
            actual
        )));
    }
    Ok(())
}

fn verify_anchored_shard_bindings(
    anchored_repo: &crate::storage::AnchoredRoot,
    anchored_shards: &HashMap<PathBuf, crate::storage::AnchoredParent>,
) -> Result<()> {
    for (relative, parent) in anchored_shards {
        anchored_repo.verify_parent_binding(relative, parent)?;
    }
    anchored_repo.verify_root_binding()
}

fn push_verification_error(result: &mut VerificationResult, error: String) {
    if result.errors.len() < MAX_VERIFICATION_ERRORS {
        result.errors.push(error);
    } else if result.errors.len() == MAX_VERIFICATION_ERRORS {
        result.errors.push(format!(
            "verification stopped collecting detailed errors after {MAX_VERIFICATION_ERRORS} entries"
        ));
    }
}

fn warn_invalid_snapshot_root_entries(
    repo_root: &Path,
    result: &mut VerificationResult,
) -> Result<()> {
    let dir = snapshots_dir(repo_root);
    if !dir.exists() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
        let entry = entry.map_err(|error| {
            crate::CheckPoError::Corruption(format!(
                "could not inspect snapshot root directory {}: {error}",
                dir.display()
            ))
        })?;
        if entry.path() == dir || entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(&dir).map_err(|_| {
            crate::CheckPoError::Corruption(format!(
                "snapshot root entry escaped its directory: {}",
                path.display()
            ))
        })?;
        let parts = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => value.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>();
        let valid = if parts.len() == 3
            && path.extension().and_then(|value| value.to_str()) == Some("root")
        {
            path.file_stem()
                .and_then(|value| value.to_str())
                .and_then(|stem| SnapshotId::parse(stem).ok())
                .is_some_and(|id| parts[0] == &id.as_str()[0..2] && parts[1] == &id.as_str()[2..4])
        } else {
            false
        };
        if !valid {
            result.warnings.push(format!(
                "ignored invalid snapshot root entry: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn held_object_verification_rejects_replaced_repository_root() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let old_repo = temp.path().join("old-repo");
        std::fs::create_dir(&repo).unwrap();
        let expected = crate::hash_bytes(b"payload");
        let object = object_path(&repo, &expected);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, b"payload").unwrap();
        let anchored_repo = crate::storage::AnchoredRoot::open(&repo).unwrap();

        std::fs::rename(&repo, &old_repo).unwrap();
        let replacement_object = object_path(&repo, &expected);
        std::fs::create_dir_all(replacement_object.parent().unwrap()).unwrap();
        std::fs::write(&replacement_object, b"payload").unwrap();

        let mut anchored_shards = HashMap::new();
        verify_object_with_cancellation(
            &anchored_repo,
            &repo,
            &replacement_object,
            &expected,
            7,
            true,
            None,
            &mut anchored_shards,
        )
        .unwrap();
        let error = verify_anchored_shard_bindings(&anchored_repo, &anchored_shards).unwrap_err();

        assert!(matches!(error, crate::CheckPoError::WorkingTreeChanged(_)));
    }
}
