use crate::models::RepositoryTempFile;
use crate::storage::merkle_codec::{ChunkKind, Digest32, ManifestRef};
use crate::storage::snapshot_v2::{
    load_chunk, validate_manifest_cached, DecodedChunk, ManifestValidationCache,
};
use crate::{
    ensure_no_pending_transactions, io_error, load_project, object_path,
    relative_path_from_project, CheckPoError, InvalidManifestChunkLocation, InvalidObjectLocation,
    MissingBlobReference, ObjectId, OperationProgress, OrphanTempFile, Result, SkippedSnapshot,
    StorageGcPlan, StorageGcResult, StorageSummary, TempFileCleanupPlan, TempFileCleanupResult,
    TrackedUnityFilePath, UnreferencedBlob, UnreferencedManifestChunk,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn storage_summary(project_path: impl AsRef<Path>) -> crate::Result<StorageSummary> {
    let project = load_project(project_path)?;
    crate::storage_summary_from_index(&project)
}

pub fn analyze_gc(project_path: impl AsRef<Path>) -> Result<StorageGcPlan> {
    analyze_gc_with_progress_and_cancellation(project_path, None, None)
}

pub fn analyze_gc_with_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<StorageGcPlan> {
    let project = load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "storage-gc-analyze")?;
    Ok(analyze_gc_for_project(&project, progress, cancellation, Some(1_000))?.plan)
}

pub fn apply_gc_with_expected_plan(
    project_path: impl AsRef<Path>,
    expected_plan_id: &str,
) -> Result<StorageGcResult> {
    apply_gc_with_expected_plan_and_progress_and_cancellation(
        project_path,
        expected_plan_id,
        None,
        None,
    )
}

pub fn apply_gc_with_expected_plan_and_progress_and_cancellation(
    project_path: impl AsRef<Path>,
    expected_plan_id: &str,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<StorageGcResult> {
    validate_opaque_plan_id(expected_plan_id)?;
    apply_gc_internal(
        project_path.as_ref(),
        expected_plan_id,
        progress,
        cancellation,
    )
}

fn apply_gc_internal(
    project_path: &Path,
    expected_plan_id: &str,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<StorageGcResult> {
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = crate::acquire_project_repository_lock(&project, "storage-gc")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let analysis = analyze_gc_for_project(&project, progress, cancellation, None)?;
    if expected_plan_id != analysis.plan.plan_id {
        return Err(CheckPoError::WorkingTreeChanged(
            "storage GC targets changed after preview".to_string(),
        ));
    }
    let StorageGcAnalysis {
        mut plan,
        candidate_fingerprints,
    } = analysis;
    if plan.has_integrity_problems {
        return Err(crate::user_error(
            "storage gc cannot apply while missing objects, unsafe storage locations, or unreadable snapshots exist.",
        ));
    }
    // The delete phase is intentionally non-cancellable. Returning Cancelled
    // after deleting only a prefix would misrepresent a successfully modified
    // repository as an untouched cancellation.
    crate::ensure_not_cancelled(cancellation)?;

    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut deleted_blob_count = 0_usize;
    let mut deleted_bytes = 0_u64;
    let delete_total = plan.unreferenced_blobs.len();
    for (index, blob) in plan.unreferenced_blobs.iter().enumerate() {
        crate::report_operation_progress(
            progress,
            "gcDeletingObjects",
            index,
            delete_total,
            Some(blob.object_id.to_string()),
        );
        let expected_fingerprint = candidate_fingerprints
            .get(&blob.object_path)
            .ok_or_else(|| CheckPoError::Corruption("GC object fingerprint is missing".into()))?;
        if remove_repo_file_if_bound(
            &anchored_repo,
            &blob.object_path,
            blob.size_bytes,
            expected_fingerprint,
        )? {
            deleted_blob_count += 1;
            deleted_bytes += blob.size_bytes;
            remove_empty_repo_dirs_anchored(
                &anchored_repo,
                blob.object_path.parent(),
                Path::new("objects/loose"),
            )?;
        }
    }
    crate::report_operation_progress(
        progress,
        "gcDeletingObjects",
        delete_total,
        delete_total,
        None,
    );

    let mut deleted_manifest_chunk_count = 0_usize;
    let mut deleted_manifest_chunk_bytes = 0_u64;
    let manifest_delete_total = plan.unreferenced_manifest_chunks.len();
    for (index, chunk) in plan.unreferenced_manifest_chunks.iter().enumerate() {
        crate::report_operation_progress(
            progress,
            "gcDeletingManifestChunks",
            index,
            manifest_delete_total,
            Some(chunk.chunk_path.display().to_string()),
        );
        let stop_at = if chunk.chunk_path.starts_with("manifests/v2/nodes") {
            Path::new("manifests/v2/nodes")
        } else if chunk.chunk_path.starts_with("manifests/v2/leaves") {
            Path::new("manifests/v2/leaves")
        } else {
            return Err(CheckPoError::Corruption(format!(
                "manifest GC candidate is outside the manifest stores: {}",
                chunk.chunk_path.display()
            )));
        };
        let expected_fingerprint = candidate_fingerprints
            .get(&chunk.chunk_path)
            .ok_or_else(|| CheckPoError::Corruption("GC chunk fingerprint is missing".into()))?;
        if remove_repo_file_if_bound(
            &anchored_repo,
            &chunk.chunk_path,
            chunk.size_bytes,
            expected_fingerprint,
        )? {
            deleted_manifest_chunk_count += 1;
            deleted_manifest_chunk_bytes += chunk.size_bytes;
            remove_empty_repo_dirs_anchored(&anchored_repo, chunk.chunk_path.parent(), stop_at)?;
        }
    }
    crate::report_operation_progress(
        progress,
        "gcDeletingManifestChunks",
        manifest_delete_total,
        manifest_delete_total,
        None,
    );
    anchored_repo.verify_root_binding()?;
    truncate_gc_plan_details(&mut plan, 1_000);

    Ok(StorageGcResult {
        plan,
        applied: true,
        deleted_blob_count,
        deleted_manifest_chunk_count,
        deleted_manifest_chunk_bytes,
        deleted_bytes: deleted_bytes + deleted_manifest_chunk_bytes,
    })
}

pub fn analyze_orphan_temp_files(project_path: impl AsRef<Path>) -> Result<TempFileCleanupPlan> {
    let project = load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "temporary-file-analyze")?;
    Ok(analyze_orphan_temp_files_for_project(&project)?.plan)
}

pub fn cleanup_orphan_temp_files_with_expected_plan(
    project_path: impl AsRef<Path>,
    expected_plan_id: &str,
    options: crate::ApplyOptions,
) -> Result<TempFileCleanupResult> {
    validate_opaque_plan_id(expected_plan_id)?;
    cleanup_orphan_temp_files_internal(project_path.as_ref(), expected_plan_id, options)
}

fn cleanup_orphan_temp_files_internal(
    project_path: &Path,
    expected_plan_id: &str,
    options: crate::ApplyOptions,
) -> Result<TempFileCleanupResult> {
    if !options.yes {
        return Err(crate::user_error("temporary file cleanup requires --yes."));
    }
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = crate::acquire_project_repository_lock(&project, "temporary-file-cleanup")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let analysis = analyze_orphan_temp_files_for_project(&project)?;
    if expected_plan_id != analysis.plan.plan_id {
        return Err(CheckPoError::WorkingTreeChanged(
            "temporary file cleanup targets changed after preview".to_string(),
        ));
    }
    let TempFileCleanupAnalysis {
        plan,
        project_fingerprints,
        repository_fingerprints,
    } = analysis;
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut deleted_file_count = 0_usize;
    let mut deleted_bytes = 0_u64;
    let mut warnings = Vec::new();
    for file in &plan.files {
        let relative = Path::new(file.path.as_str());
        let expected_fingerprint = project_fingerprints.get(relative).ok_or_else(|| {
            CheckPoError::Corruption("project temporary file fingerprint is missing".into())
        })?;
        match remove_planned_temp_file_anchored(
            &anchored_project,
            relative,
            file.size_bytes,
            expected_fingerprint,
            |leaf| crate::is_checkpo_owned_temporary_file(Path::new(leaf)),
        )? {
            AnchoredTempRemoval::Removed => {
                deleted_file_count += 1;
                deleted_bytes += file.size_bytes;
            }
            AnchoredTempRemoval::Missing => {}
            AnchoredTempRemoval::Changed(reason) => warnings.push(format!(
                "{} was not deleted because it is no longer the planned CheckPo temporary file: {reason}",
                file.path
            )),
        }
    }
    for file in &plan.repository_files {
        if !is_repository_object_temp_file_name(&file.file_name) {
            warnings.push(format!(
                "{} was not deleted because it is not a CheckPo repository temporary file",
                file.file_name
            ));
            continue;
        }
        let relative = Path::new("tmp").join(&file.file_name);
        let expected_fingerprint = repository_fingerprints.get(&relative).ok_or_else(|| {
            CheckPoError::Corruption("repository temporary file fingerprint is missing".into())
        })?;
        match remove_planned_temp_file_anchored(
            &anchored_repo,
            &relative,
            file.size_bytes,
            expected_fingerprint,
            |leaf| {
                leaf.to_str()
                    .is_some_and(is_repository_object_temp_file_name)
            },
        )? {
            AnchoredTempRemoval::Removed => {
                deleted_file_count += 1;
                deleted_bytes += file.size_bytes;
            }
            AnchoredTempRemoval::Missing => {}
            AnchoredTempRemoval::Changed(reason) => warnings.push(format!(
                "{} was not deleted because it is no longer the planned repository temporary file: {reason}",
                file.file_name
            )),
        }
    }
    anchored_project.verify_root_binding()?;
    anchored_repo.verify_root_binding()?;
    Ok(TempFileCleanupResult {
        plan,
        deleted_file_count,
        deleted_bytes,
        warnings,
    })
}

struct TempFileCleanupAnalysis {
    plan: TempFileCleanupPlan,
    project_fingerprints: BTreeMap<PathBuf, String>,
    repository_fingerprints: BTreeMap<PathBuf, String>,
}

fn analyze_orphan_temp_files_for_project(
    project: &crate::ProjectContext,
) -> Result<TempFileCleanupAnalysis> {
    let inventory_head =
        crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for root in ["Assets", "Packages", "ProjectSettings"] {
        let root_path = project.project_root.as_path().join(root);
        if !root_path.exists() {
            continue;
        }
        if !root_path.is_dir() {
            warnings.push(format!("{root}: tracked root is not a directory"));
            continue;
        }
        let mut entries = WalkDir::new(&root_path).follow_links(false).into_iter();
        while let Some(entry) = entries.next() {
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
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if entry.file_type().is_dir() {
                        entries.skip_current_dir();
                    }
                    warnings.push(format!("{}: {error}", entry.path().display()));
                    continue;
                }
            };
            if crate::metadata_is_link_or_reparse(&metadata) {
                if entry.file_type().is_dir() || metadata.is_dir() {
                    entries.skip_current_dir();
                }
                warnings.push(format!(
                    "{}: symbolic links and reparse points are not supported",
                    entry.path().display()
                ));
                continue;
            }
            if !metadata.is_file() || !crate::is_checkpo_owned_temporary_file(entry.path()) {
                continue;
            }
            let relative =
                match relative_path_from_project(project.project_root.as_path(), entry.path()) {
                    Ok(relative) => relative,
                    Err(error) => {
                        warnings.push(format!("{}: {error}", entry.path().display()));
                        continue;
                    }
                };
            let path = match TrackedUnityFilePath::parse(&relative) {
                Ok(path) => path,
                Err(error) => {
                    warnings.push(format!("{relative}: {error}"));
                    continue;
                }
            };
            files.push(OrphanTempFile {
                path,
                size_bytes: metadata.len(),
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut repository_files = analyze_repository_temp_files(&project.repo_root, &mut warnings);
    repository_files.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    let total_bytes = files
        .iter()
        .map(|file| file.size_bytes)
        .chain(repository_files.iter().map(|file| file.size_bytes))
        .sum();
    let mut plan = TempFileCleanupPlan {
        schema_version: crate::TEMP_FILE_CLEANUP_PLAN_SCHEMA_VERSION,
        plan_id: String::new(),
        file_count: files.len() + repository_files.len(),
        total_bytes,
        files,
        repository_files,
        warnings,
    };
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut project_fingerprints = BTreeMap::new();
    for file in &plan.files {
        let relative = PathBuf::from(file.path.as_str());
        let fingerprint =
            strong_anchored_file_fingerprint(&anchored_project, &relative, file.size_bytes)?;
        project_fingerprints.insert(relative, fingerprint);
    }
    let mut repository_fingerprints = BTreeMap::new();
    for file in &plan.repository_files {
        let relative = Path::new("tmp").join(&file.file_name);
        let fingerprint =
            strong_anchored_file_fingerprint(&anchored_repo, &relative, file.size_bytes)?;
        repository_fingerprints.insert(relative, fingerprint);
    }
    anchored_project.verify_root_binding()?;
    anchored_repo.verify_root_binding()?;
    let current_inventory_head =
        crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    if current_inventory_head != inventory_head {
        return Err(CheckPoError::WorkingTreeChanged(
            "checkpoint inventory changed during temporary file analysis".to_string(),
        ));
    }
    plan.plan_id = temp_cleanup_plan_id(
        project,
        &inventory_head,
        &plan,
        &project_fingerprints,
        &repository_fingerprints,
    )?;
    Ok(TempFileCleanupAnalysis {
        plan,
        project_fingerprints,
        repository_fingerprints,
    })
}

fn analyze_repository_temp_files(
    repo_root: &Path,
    warnings: &mut Vec<String>,
) -> Vec<RepositoryTempFile> {
    let tmp_dir = repo_root.join("tmp");
    match fs::symlink_metadata(&tmp_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("{}: {error}", tmp_dir.display()));
            return Vec::new();
        }
        Ok(metadata)
            if metadata.file_type().is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            warnings.push(format!(
                "{}: repository temporary path is not a directory",
                tmp_dir.display()
            ));
            return Vec::new();
        }
    }

    let entries = match fs::read_dir(&tmp_dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!("{}: {error}", tmp_dir.display()));
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!("{}: {error}", tmp_dir.display()));
                continue;
            }
        };
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_repository_object_temp_file_name(&file_name) {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && !crate::metadata_is_link_or_reparse(&metadata) =>
            {
                metadata
            }
            Ok(_) => continue,
            Err(error) => {
                warnings.push(format!("{}: {error}", entry.path().display()));
                continue;
            }
        };
        files.push(RepositoryTempFile {
            file_name,
            size_bytes: metadata.len(),
        });
    }
    files
}

enum AnchoredTempRemoval {
    Removed,
    Missing,
    Changed(String),
}

fn remove_planned_temp_file_anchored(
    root: &crate::storage::AnchoredRoot,
    relative: &Path,
    expected_size: u64,
    expected_fingerprint: &str,
    name_is_owned: impl FnOnce(&std::ffi::OsStr) -> bool,
) -> Result<AnchoredTempRemoval> {
    let (parent, leaf) = match root.open_parent_for_mutation(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AnchoredTempRemoval::Missing)
        }
        Err(error) if planned_temp_target_changed(&error) => {
            return Ok(AnchoredTempRemoval::Changed(error.to_string()))
        }
        Err(error) => return Err(error),
    };
    if !name_is_owned(&leaf) {
        return Ok(AnchoredTempRemoval::Changed(
            "the file name no longer matches CheckPo's temporary-file format".to_string(),
        ));
    }
    let mut expected = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AnchoredTempRemoval::Missing)
        }
        Err(error) if planned_temp_target_changed(&error) => {
            return Ok(AnchoredTempRemoval::Changed(error.to_string()))
        }
        Err(error) => return Err(error),
    };
    let actual_size = expected.metadata()?.len();
    if actual_size != expected_size {
        return Ok(AnchoredTempRemoval::Changed(format!(
            "size changed from {expected_size} to {actual_size} bytes"
        )));
    }
    let actual_fingerprint = strong_open_file_fingerprint(&mut expected)?;
    if actual_fingerprint != expected_fingerprint {
        return Ok(AnchoredTempRemoval::Changed(
            "the file identity or version changed after preview".to_string(),
        ));
    }
    let parent_relative = relative.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "planned temporary file has no relative parent: {}",
            relative.display()
        ))
    })?;
    if let Err(error) = root.verify_parent_binding(parent_relative, &parent) {
        return match error {
            error @ (CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_)) => {
                Ok(AnchoredTempRemoval::Changed(error.to_string()))
            }
            error => Err(error),
        };
    }
    match parent.unlink_file_if_bound(&leaf, expected) {
        Ok(()) => {}
        Err(error @ (CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_))) => {
            return Ok(AnchoredTempRemoval::Changed(error.to_string()))
        }
        Err(error) => return Err(error),
    }
    parent.sync_all()?;
    Ok(AnchoredTempRemoval::Removed)
}

fn planned_temp_target_changed(error: &CheckPoError) -> bool {
    match error {
        CheckPoError::Corruption(_) | CheckPoError::WorkingTreeChanged(_) => true,
        CheckPoError::Io { source, .. } => {
            if source.kind() == std::io::ErrorKind::NotADirectory {
                return true;
            }
            #[cfg(unix)]
            return source
                .raw_os_error()
                .is_some_and(|code| code == libc::ELOOP || code == libc::ENOTDIR);
            #[cfg(not(unix))]
            false
        }
        _ => false,
    }
}

fn is_repository_object_temp_file_name(file_name: &str) -> bool {
    let Some(id) = file_name
        .strip_prefix("object-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_opaque_plan_id(value: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(crate::user_error("maintenance plan id is invalid"))
    }
}

fn strong_anchored_file_fingerprint(
    root: &crate::storage::AnchoredRoot,
    relative: &Path,
    expected_size: u64,
) -> Result<String> {
    let (parent, leaf) = root.open_parent(relative, false)?;
    let mut file = parent.open_file(&leaf)?;
    let actual_size = file.metadata()?.len();
    if actual_size != expected_size {
        return Err(CheckPoError::WorkingTreeChanged(format!(
            "planned file size changed during analysis: {} (expected {expected_size}, found {actual_size})",
            relative.display()
        )));
    }
    let fingerprint = strong_open_file_fingerprint(&mut file)?;
    let parent_relative = relative.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "planned file has no relative parent: {}",
            relative.display()
        ))
    })?;
    parent.verify_file_binding(&leaf, &file)?;
    root.verify_parent_binding(parent_relative, &parent)?;
    Ok(fingerprint)
}

fn strong_open_file_fingerprint(file: &mut crate::storage::AnchoredFile) -> Result<String> {
    if let Some(fingerprint) = file.fingerprint()? {
        return Ok(fingerprint);
    }
    Ok(format!("content-v1:{}", file.hash()?.object_id))
}

fn hash_plan_field(hasher: &mut blake3::Hasher, value: &[u8]) -> Result<()> {
    let length = u64::try_from(value.len())
        .map_err(|_| CheckPoError::Corruption("maintenance plan field is too large".into()))?;
    hasher.update(&length.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn hash_plan_path(hasher: &mut blake3::Hasher, path: &Path) -> Result<()> {
    let value = path.to_str().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "maintenance plan path is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    hash_plan_field(hasher, value.as_bytes())
}

fn temp_cleanup_plan_id(
    project: &crate::ProjectContext,
    inventory_head: &str,
    plan: &TempFileCleanupPlan,
    project_fingerprints: &BTreeMap<PathBuf, String>,
    repository_fingerprints: &BTreeMap<PathBuf, String>,
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"checkpo.temp-cleanup-plan.v1\0");
    hash_plan_field(&mut hasher, project.project_id.as_str().as_bytes())?;
    hash_plan_field(&mut hasher, inventory_head.as_bytes())?;
    for file in &plan.files {
        let relative = Path::new(file.path.as_str());
        hasher.update(b"p");
        hash_plan_path(&mut hasher, relative)?;
        hasher.update(&file.size_bytes.to_be_bytes());
        let fingerprint = project_fingerprints.get(relative).ok_or_else(|| {
            CheckPoError::Corruption("project temporary file fingerprint is missing".into())
        })?;
        hash_plan_field(&mut hasher, fingerprint.as_bytes())?;
    }
    for file in &plan.repository_files {
        let relative = Path::new("tmp").join(&file.file_name);
        hasher.update(b"r");
        hash_plan_path(&mut hasher, &relative)?;
        hasher.update(&file.size_bytes.to_be_bytes());
        let fingerprint = repository_fingerprints.get(&relative).ok_or_else(|| {
            CheckPoError::Corruption("repository temporary file fingerprint is missing".into())
        })?;
        hash_plan_field(&mut hasher, fingerprint.as_bytes())?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn storage_gc_plan_id(
    project: &crate::ProjectContext,
    inventory_head: &str,
    plan: &StorageGcPlan,
    candidate_fingerprints: &BTreeMap<PathBuf, String>,
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"checkpo.storage-gc-plan.v1\0");
    hash_plan_field(&mut hasher, project.project_id.as_str().as_bytes())?;
    hash_plan_field(&mut hasher, inventory_head.as_bytes())?;
    for blob in &plan.unreferenced_blobs {
        hasher.update(b"o");
        hash_plan_field(&mut hasher, blob.object_id.as_str().as_bytes())?;
        hash_plan_path(&mut hasher, &blob.object_path)?;
        hasher.update(&blob.size_bytes.to_be_bytes());
        let fingerprint = candidate_fingerprints
            .get(&blob.object_path)
            .ok_or_else(|| CheckPoError::Corruption("GC object fingerprint is missing".into()))?;
        hash_plan_field(&mut hasher, fingerprint.as_bytes())?;
    }
    for chunk in &plan.unreferenced_manifest_chunks {
        hasher.update(b"m");
        hash_plan_path(&mut hasher, &chunk.chunk_path)?;
        hasher.update(&chunk.size_bytes.to_be_bytes());
        let fingerprint = candidate_fingerprints
            .get(&chunk.chunk_path)
            .ok_or_else(|| CheckPoError::Corruption("GC chunk fingerprint is missing".into()))?;
        hash_plan_field(&mut hasher, fingerprint.as_bytes())?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

struct ReferencedObject {
    first_reference: MissingBlobReference,
    expected_size_bytes: u64,
}

struct StorageGcAnalysis {
    plan: StorageGcPlan,
    candidate_fingerprints: BTreeMap<PathBuf, String>,
}

const MAX_MANIFEST_VALIDATION_CACHE_CHUNKS: usize = 100_000;
const MAX_MANIFEST_VALIDATION_CACHE_BYTES: usize = 64 * 1024 * 1024;

fn analyze_gc_for_project(
    project: &crate::ProjectContext,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&crate::CancellationToken>,
    detail_limit: Option<usize>,
) -> Result<StorageGcAnalysis> {
    let inventory_head =
        crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    let mut referenced = BTreeMap::<ObjectId, ReferencedObject>::new();
    let mut reference_integrity_problems = BTreeMap::<ObjectId, String>::new();
    let mut referenced_manifest_chunks = BTreeSet::<ManifestRef>::new();
    let mut missing_references = Vec::new();
    let mut skipped_snapshots = Vec::new();
    let mut checkpoint_count = 0_usize;
    validate_snapshot_root_inventory(&project.repo_root)?;
    let snapshot_ids = crate::storage::validate_physical_snapshot_inventory(
        &project.repo_root,
        &project.project_id,
    )?;
    let snapshot_total = snapshot_ids.len();
    let manifest_source = crate::storage::RepositoryManifestSource::new(&project.repo_root)?;
    let mut manifest_validation_cache = ManifestValidationCache::default();
    for (snapshot_index, snapshot_id) in snapshot_ids.into_iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        crate::report_operation_progress(
            progress,
            "gcReadingSnapshots",
            snapshot_index,
            snapshot_total,
            Some(snapshot_id.to_string()),
        );
        match inspect_and_mark_snapshot(
            project,
            &snapshot_id,
            &manifest_source,
            &mut manifest_validation_cache,
            &mut referenced_manifest_chunks,
            &mut referenced,
            &mut reference_integrity_problems,
        ) {
            Ok(()) => {
                checkpoint_count += 1;
            }
            Err(error) => skipped_snapshots.push(SkippedSnapshot {
                checkpoint_id: snapshot_id,
                reason: error.to_string(),
            }),
        }
        if manifest_validation_cache.len() >= MAX_MANIFEST_VALIDATION_CACHE_CHUNKS
            || manifest_validation_cache.estimated_heap_bytes()
                >= MAX_MANIFEST_VALIDATION_CACHE_BYTES
        {
            manifest_validation_cache = ManifestValidationCache::default();
        }
    }
    crate::report_operation_progress(
        progress,
        "gcReadingSnapshots",
        snapshot_total,
        snapshot_total,
        None,
    );
    let referenced_total = referenced.len();
    for (index, (object_id, reference)) in referenced.iter().enumerate() {
        crate::ensure_not_cancelled(cancellation)?;
        crate::report_operation_progress(
            progress,
            "gcCheckingReferences",
            index,
            referenced_total,
            Some(object_id.to_string()),
        );
        let path = object_path(&project.repo_root, object_id);
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) =>
            {
                if metadata.len() != reference.expected_size_bytes {
                    reference_integrity_problems
                        .entry(object_id.clone())
                        .or_insert_with(|| {
                            format!(
                                "reachable object size mismatch: expected {} bytes, found {} bytes",
                                reference.expected_size_bytes,
                                metadata.len()
                            )
                        });
                }
            }
            _ => missing_references.push(reference.first_reference.clone()),
        }
    }
    crate::report_operation_progress(
        progress,
        "gcCheckingReferences",
        referenced_total,
        referenced_total,
        None,
    );

    let ObjectInventory {
        objects,
        mut invalid_locations,
    } = enumerate_loose_objects(&project.repo_root, progress, cancellation)?;
    let object_file_count = objects.len() + invalid_locations.len();
    for (object_id, reason) in reference_integrity_problems {
        let path = object_path(&project.repo_root, &object_id);
        invalid_locations.push(InvalidObjectLocation {
            object_path: repo_relative_path(&project.repo_root, &path)?,
            reason,
        });
    }
    let mut unreferenced_blobs = Vec::new();
    for (object_id, object_path) in objects {
        if referenced.contains_key(&object_id) {
            continue;
        }
        let full_path = project.repo_root.join(&object_path);
        let size_bytes = fs::metadata(&full_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        unreferenced_blobs.push(UnreferencedBlob {
            object_id,
            object_path,
            size_bytes,
        });
    }
    let unreferenced_logical_bytes = unreferenced_blobs.iter().map(|blob| blob.size_bytes).sum();

    let ManifestChunkInventory {
        chunks: manifest_chunks,
        file_count: manifest_chunk_file_count,
        invalid_locations: invalid_manifest_chunk_locations,
    } = enumerate_manifest_chunks(&project.repo_root, progress, cancellation)?;
    let mut unreferenced_manifest_chunks = Vec::new();
    for (reference, (chunk_path, size_bytes)) in manifest_chunks {
        if !referenced_manifest_chunks.contains(&reference) {
            unreferenced_manifest_chunks.push(UnreferencedManifestChunk {
                chunk_path,
                size_bytes,
            });
        }
    }
    unreferenced_manifest_chunks.sort_by(|left, right| left.chunk_path.cmp(&right.chunk_path));
    let unreferenced_manifest_chunk_bytes = unreferenced_manifest_chunks
        .iter()
        .map(|chunk| chunk.size_bytes)
        .sum();
    let has_integrity_problems = !missing_references.is_empty()
        || !invalid_locations.is_empty()
        || !invalid_manifest_chunk_locations.is_empty()
        || !skipped_snapshots.is_empty();

    let mut plan = StorageGcPlan {
        schema_version: crate::STORAGE_GC_PLAN_SCHEMA_VERSION,
        plan_id: String::new(),
        checkpoint_count,
        object_file_count,
        referenced_blob_count: referenced.len(),
        unreferenced_blob_count: unreferenced_blobs.len(),
        unreferenced_logical_bytes,
        manifest_chunk_file_count,
        referenced_manifest_chunk_count: referenced_manifest_chunks.len(),
        unreferenced_manifest_chunk_count: unreferenced_manifest_chunks.len(),
        unreferenced_manifest_chunk_bytes,
        unreferenced_blobs,
        unreferenced_manifest_chunks,
        missing_references,
        invalid_object_locations: invalid_locations,
        invalid_manifest_chunk_locations,
        skipped_snapshots,
        has_integrity_problems,
        details_truncated: false,
    };
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut candidate_fingerprints = BTreeMap::new();
    for blob in &plan.unreferenced_blobs {
        let fingerprint =
            strong_anchored_file_fingerprint(&anchored_repo, &blob.object_path, blob.size_bytes)?;
        candidate_fingerprints.insert(blob.object_path.clone(), fingerprint);
    }
    for chunk in &plan.unreferenced_manifest_chunks {
        let fingerprint =
            strong_anchored_file_fingerprint(&anchored_repo, &chunk.chunk_path, chunk.size_bytes)?;
        candidate_fingerprints.insert(chunk.chunk_path.clone(), fingerprint);
    }
    anchored_repo.verify_root_binding()?;
    let current_inventory_head =
        crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    if current_inventory_head != inventory_head {
        return Err(CheckPoError::WorkingTreeChanged(
            "checkpoint inventory changed during storage GC analysis".to_string(),
        ));
    }
    plan.plan_id = storage_gc_plan_id(project, &inventory_head, &plan, &candidate_fingerprints)?;
    if let Some(limit) = detail_limit {
        truncate_gc_plan_details(&mut plan, limit);
    }
    Ok(StorageGcAnalysis {
        plan,
        candidate_fingerprints,
    })
}

fn truncate_gc_plan_details(plan: &mut StorageGcPlan, limit: usize) {
    let truncated = plan.unreferenced_blobs.len() > limit
        || plan.unreferenced_manifest_chunks.len() > limit
        || plan.missing_references.len() > limit
        || plan.invalid_object_locations.len() > limit
        || plan.invalid_manifest_chunk_locations.len() > limit
        || plan.skipped_snapshots.len() > limit;
    plan.unreferenced_blobs.truncate(limit);
    plan.unreferenced_manifest_chunks.truncate(limit);
    plan.missing_references.truncate(limit);
    plan.invalid_object_locations.truncate(limit);
    plan.invalid_manifest_chunk_locations.truncate(limit);
    plan.skipped_snapshots.truncate(limit);
    plan.details_truncated = truncated;
}

fn inspect_and_mark_snapshot(
    project: &crate::ProjectContext,
    snapshot_id: &crate::SnapshotId,
    source: &crate::storage::RepositoryManifestSource<'_>,
    validation_cache: &mut ManifestValidationCache,
    referenced_manifest_chunks: &mut BTreeSet<ManifestRef>,
    referenced_objects: &mut BTreeMap<ObjectId, ReferencedObject>,
    reference_integrity_problems: &mut BTreeMap<ObjectId, String>,
) -> Result<()> {
    let root = crate::storage::load_snapshot_root_header(&project.repo_root, snapshot_id)?;
    validate_manifest_cached(source, root.manifest, root.summary, validation_cache)
        .map_err(|error| CheckPoError::Corruption(format!("{snapshot_id}: {error}")))?;
    if root.project_id != project.project_id {
        return Err(CheckPoError::Corruption(
            "snapshot project id does not match repository project id".to_string(),
        ));
    }

    let mut pending = root.manifest.into_iter().collect::<Vec<_>>();
    while let Some(reference) = pending.pop() {
        if referenced_manifest_chunks.contains(&reference) {
            continue;
        }
        let decoded = load_chunk(source, reference)
            .map_err(|error| CheckPoError::Corruption(format!("{snapshot_id}: {error}")))?;
        referenced_manifest_chunks.insert(reference);
        match decoded {
            DecodedChunk::Node(node, _) => {
                pending.extend(node.children.into_iter().map(|child| child.child));
            }
            DecodedChunk::Leaf(leaf, _) => {
                for entry in leaf.entries {
                    let path = TrackedUnityFilePath::parse(&entry.exact_path)?;
                    let object_id = ObjectId::from_digest_bytes(*entry.object_id.as_bytes());
                    let expected_size_bytes = entry.size_bytes;
                    match referenced_objects.entry(object_id.clone()) {
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            slot.insert(ReferencedObject {
                                first_reference: MissingBlobReference {
                                    checkpoint_id: snapshot_id.clone(),
                                    path,
                                    object_id,
                                },
                                expected_size_bytes,
                            });
                        }
                        std::collections::btree_map::Entry::Occupied(slot)
                            if slot.get().expected_size_bytes != expected_size_bytes =>
                        {
                            reference_integrity_problems
                                .entry(object_id)
                                .or_insert_with(|| {
                                    format!(
                                        "object has conflicting expected sizes {} and {} across checkpoints",
                                        slot.get().expected_size_bytes,
                                        expected_size_bytes
                                    )
                                });
                        }
                        std::collections::btree_map::Entry::Occupied(_) => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_snapshot_root_inventory(repo_root: &Path) -> Result<()> {
    let snapshots_root = crate::snapshots_dir(repo_root);
    let root_metadata =
        fs::symlink_metadata(&snapshots_root).map_err(|error| io_error(&snapshots_root, error))?;
    if crate::metadata_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "snapshot root directory is unsafe: {}",
            snapshots_root.display()
        )));
    }
    for first in fs::read_dir(&snapshots_root).map_err(|error| io_error(&snapshots_root, error))? {
        let first = first.map_err(|error| io_error(&snapshots_root, error))?;
        validate_snapshot_shard_directory(&first.path(), 0)?;
        for second in fs::read_dir(first.path()).map_err(|error| io_error(first.path(), error))? {
            let second = second.map_err(|error| io_error(first.path(), error))?;
            validate_snapshot_shard_directory(&second.path(), 1)?;
            for entry in
                fs::read_dir(second.path()).map_err(|error| io_error(second.path(), error))?
            {
                let entry = entry.map_err(|error| io_error(second.path(), error))?;
                let path = entry.path();
                let metadata =
                    fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
                if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                    return Err(CheckPoError::Corruption(format!(
                        "snapshot root entry is not a regular no-follow file: {}",
                        path.display()
                    )));
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    return Err(CheckPoError::Corruption(format!(
                        "snapshot root filename is not valid UTF-8: {}",
                        path.display()
                    )));
                };
                let Some(id_text) = name.strip_suffix(".root") else {
                    return Err(CheckPoError::Corruption(format!(
                        "invalid snapshot root filename: {}",
                        path.display()
                    )));
                };
                let id = crate::SnapshotId::parse(id_text).map_err(|_| {
                    CheckPoError::Corruption(format!(
                        "invalid snapshot root filename: {}",
                        path.display()
                    ))
                })?;
                if crate::snapshot_path(repo_root, &id) != path {
                    return Err(CheckPoError::Corruption(format!(
                        "snapshot root is stored outside its canonical shard path: {}",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_snapshot_shard_directory(path: &Path, shard_index: usize) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return Err(CheckPoError::Corruption(format!(
            "snapshot shard name is not valid UTF-8: {}",
            path.display()
        )));
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() || !is_lower_hex(name, 2)
    {
        return Err(CheckPoError::Corruption(format!(
            "invalid level {} snapshot shard directory: {}",
            shard_index + 1,
            path.display()
        )));
    }
    Ok(())
}

struct ManifestChunkInventory {
    chunks: BTreeMap<ManifestRef, (PathBuf, u64)>,
    file_count: usize,
    invalid_locations: Vec<InvalidManifestChunkLocation>,
}

fn enumerate_manifest_chunks(
    repo_root: &Path,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<ManifestChunkInventory> {
    let mut chunks = BTreeMap::new();
    let mut file_count = 0_usize;
    let mut invalid_locations = Vec::new();
    let mut completed = 0_usize;
    for (kind, store_root) in [
        (
            ChunkKind::Node,
            crate::storage::manifest_nodes_dir(repo_root),
        ),
        (
            ChunkKind::Leaf,
            crate::storage::manifest_leaves_dir(repo_root),
        ),
    ] {
        let root_metadata =
            fs::symlink_metadata(&store_root).map_err(|error| io_error(&store_root, error))?;
        if crate::metadata_is_link_or_reparse(&root_metadata) || !root_metadata.is_dir() {
            invalid_locations.push(InvalidManifestChunkLocation {
                chunk_path: repo_relative_path(repo_root, &store_root)?,
                reason: "manifest chunk store is not a regular no-follow directory".to_string(),
            });
            continue;
        }
        let mut entries = WalkDir::new(&store_root).follow_links(false).into_iter();
        while let Some(entry) = entries.next() {
            crate::ensure_not_cancelled(cancellation)?;
            let entry = entry.map_err(|error| {
                let path = error
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| store_root.clone());
                io_error(path, std::io::Error::other(error))
            })?;
            if entry.depth() == 0 {
                continue;
            }
            completed += 1;
            crate::report_operation_progress(
                progress,
                "gcEnumeratingManifestChunks",
                completed,
                0,
                Some(entry.path().display().to_string()),
            );
            let path = entry.path();
            let relative = repo_relative_path(repo_root, path)?;
            let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
            if metadata.is_file() {
                file_count += 1;
            }
            if crate::metadata_is_link_or_reparse(&metadata) {
                if metadata.is_dir() {
                    entries.skip_current_dir();
                }
                invalid_locations.push(InvalidManifestChunkLocation {
                    chunk_path: relative,
                    reason: "manifest chunk storage symlinks and reparse points are not supported"
                        .to_string(),
                });
                continue;
            }
            if entry.depth() == 1 {
                let valid = metadata.is_dir()
                    && path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| is_lower_hex(value, 2));
                if !valid {
                    if metadata.is_dir() {
                        entries.skip_current_dir();
                    }
                    invalid_locations.push(InvalidManifestChunkLocation {
                        chunk_path: relative,
                        reason: format!("invalid level {} manifest shard directory", entry.depth()),
                    });
                }
                continue;
            }
            if entry.depth() != 2 || !metadata.is_file() {
                if metadata.is_dir() {
                    entries.skip_current_dir();
                }
                invalid_locations.push(InvalidManifestChunkLocation {
                    chunk_path: relative,
                    reason: "manifest chunk is outside the canonical one-level shard layout"
                        .to_string(),
                });
                continue;
            }
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                invalid_locations.push(InvalidManifestChunkLocation {
                    chunk_path: relative,
                    reason: "manifest chunk filename is not valid UTF-8".to_string(),
                });
                continue;
            };
            let Some(digest) = parse_lower_hex_digest(name) else {
                invalid_locations.push(InvalidManifestChunkLocation {
                    chunk_path: relative,
                    reason: "manifest chunk filename is not a 64-character lowercase hex digest"
                        .to_string(),
                });
                continue;
            };
            let expected_path = match kind {
                ChunkKind::Node => crate::storage::manifest_node_path(repo_root, name),
                ChunkKind::Leaf => crate::storage::manifest_leaf_path(repo_root, name),
                ChunkKind::Root => unreachable!(),
            };
            if expected_path != path {
                invalid_locations.push(InvalidManifestChunkLocation {
                    chunk_path: relative,
                    reason: "manifest chunk is stored outside its digest shard path".to_string(),
                });
                continue;
            }
            chunks.insert(
                ManifestRef {
                    kind,
                    id: Digest32::from_bytes(digest),
                },
                (relative, metadata.len()),
            );
        }
    }
    Ok(ManifestChunkInventory {
        chunks,
        file_count,
        invalid_locations,
    })
}

fn parse_lower_hex_digest(value: &str) -> Option<[u8; 32]> {
    if !is_lower_hex(value, 64) {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (index, output) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *output = u8::from_str_radix(&value[offset..offset + 2], 16).ok()?;
    }
    Some(bytes)
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct ObjectInventory {
    objects: BTreeMap<ObjectId, PathBuf>,
    invalid_locations: Vec<InvalidObjectLocation>,
}

fn enumerate_loose_objects(
    repo_root: &Path,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<ObjectInventory> {
    let loose_root = repo_root.join("objects").join("loose");
    let mut objects = BTreeMap::new();
    let mut invalid_locations = Vec::new();
    if !loose_root.exists() {
        return Ok(ObjectInventory {
            objects,
            invalid_locations,
        });
    }
    let mut entries = WalkDir::new(&loose_root).follow_links(false).into_iter();
    let mut completed = 0_usize;
    while let Some(entry) = entries.next() {
        crate::ensure_not_cancelled(cancellation)?;
        let entry = entry.map_err(|error| {
            let path = error
                .path()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| loose_root.clone());
            io_error(path, std::io::Error::other(error))
        })?;
        completed += 1;
        crate::report_operation_progress(
            progress,
            "gcEnumeratingObjects",
            completed,
            0,
            Some(entry.path().display().to_string()),
        );
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&metadata) {
            if metadata.is_dir() {
                entries.skip_current_dir();
            }
            invalid_locations.push(InvalidObjectLocation {
                object_path: repo_relative_path(repo_root, entry.path())?,
                reason: "object storage symlinks and reparse points are not supported.".to_string(),
            });
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if entry.path().extension().and_then(|value| value.to_str()) == Some("tmp") {
            continue;
        }
        let relative = repo_relative_path(repo_root, entry.path())?;
        match crate::object_id_from_loose_relative_path(&relative) {
            Ok(object_id) => {
                objects.insert(object_id, relative);
            }
            Err(reason) => invalid_locations.push(InvalidObjectLocation {
                object_path: relative,
                reason,
            }),
        }
    }
    Ok(ObjectInventory {
        objects,
        invalid_locations,
    })
}

fn remove_repo_file_if_bound(
    repo: &crate::storage::AnchoredRoot,
    relative: &Path,
    expected_size: u64,
    expected_fingerprint: &str,
) -> Result<bool> {
    remove_repo_file_if_bound_inner(repo, relative, expected_size, expected_fingerprint, || {})
}

fn remove_repo_file_if_bound_inner(
    repo: &crate::storage::AnchoredRoot,
    relative: &Path,
    expected_size: u64,
    expected_fingerprint: &str,
    before_unlink: impl FnOnce(),
) -> Result<bool> {
    let (parent, leaf) = match repo.open_parent_for_mutation(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let mut file = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    let actual_size = file.metadata()?.len();
    if actual_size != expected_size {
        return Err(CheckPoError::WorkingTreeChanged(format!(
            "GC target changed before deletion: {} (expected {} bytes, found {} bytes)",
            relative.display(),
            expected_size,
            actual_size
        )));
    }
    if strong_open_file_fingerprint(&mut file)? != expected_fingerprint {
        return Err(CheckPoError::WorkingTreeChanged(format!(
            "GC target identity or version changed before deletion: {}",
            relative.display()
        )));
    }
    let parent_relative = relative.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "GC target has no repository-relative parent: {}",
            relative.display()
        ))
    })?;
    before_unlink();
    repo.verify_parent_binding(parent_relative, &parent)?;
    parent.unlink_file_if_bound(&leaf, file)?;
    parent.sync_all()?;
    Ok(true)
}

fn remove_empty_repo_dirs_anchored(
    repo: &crate::storage::AnchoredRoot,
    mut current: Option<&Path>,
    stop_at: &Path,
) -> Result<()> {
    while let Some(relative) = current {
        if relative == stop_at {
            break;
        }
        let (parent, leaf) = match repo.open_parent_for_mutation(relative, false) {
            Ok(value) => value,
            Err(CheckPoError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                break
            }
            Err(error) => return Err(error),
        };
        let parent_relative = relative.parent().ok_or_else(|| {
            CheckPoError::Corruption(format!(
                "GC shard has no repository-relative parent: {}",
                relative.display()
            ))
        })?;
        repo.verify_parent_binding(parent_relative, &parent)?;
        match parent.unlink_dir(&leaf) {
            Ok(()) => {
                parent.sync_all()?;
                current = relative.parent();
            }
            Err(CheckPoError::Io { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn repo_relative_path(repo_root: &Path, full_path: &Path) -> Result<PathBuf> {
    full_path
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .map_err(|_| CheckPoError::OutsideTrackedScope(full_path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_temp_delete_rechecks_size_before_unlink() {
        let temporary = tempfile::tempdir().unwrap();
        let root_path = temporary.path().join("project");
        let relative = Path::new("Assets/.checkpo-0123456789abcdef0123456789abcdef.tmp");
        fs::create_dir_all(root_path.join("Assets")).unwrap();
        fs::write(root_path.join(relative), b"changed").unwrap();
        let anchored = crate::storage::AnchoredRoot::open(&root_path).unwrap();

        let result = remove_planned_temp_file_anchored(&anchored, relative, 4, "unused", |leaf| {
            crate::is_checkpo_owned_temporary_file(Path::new(leaf))
        })
        .unwrap();

        assert!(matches!(result, AnchoredTempRemoval::Changed(_)));
        assert_eq!(fs::read(root_path.join(relative)).unwrap(), b"changed");
    }

    #[cfg(unix)]
    #[test]
    fn anchored_temp_delete_rejects_swapped_parent_without_touching_outside_file() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root_path = temporary.path().join("project");
        let original_assets = root_path.join("Assets-original");
        let outside = temporary.path().join("outside");
        let file_name = ".checkpo-0123456789abcdef0123456789abcdef.tmp";
        fs::create_dir_all(root_path.join("Assets")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root_path.join("Assets").join(file_name), b"repo").unwrap();
        fs::write(outside.join(file_name), b"outside").unwrap();
        let anchored = crate::storage::AnchoredRoot::open(&root_path).unwrap();
        fs::rename(root_path.join("Assets"), &original_assets).unwrap();
        symlink(&outside, root_path.join("Assets")).unwrap();

        let result = remove_planned_temp_file_anchored(
            &anchored,
            &Path::new("Assets").join(file_name),
            4,
            "unused",
            |leaf| crate::is_checkpo_owned_temporary_file(Path::new(leaf)),
        )
        .unwrap();

        assert!(matches!(result, AnchoredTempRemoval::Changed(_)));
        assert_eq!(fs::read(original_assets.join(file_name)).unwrap(), b"repo");
        assert_eq!(fs::read(outside.join(file_name)).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn anchored_gc_delete_rejects_parent_swap_without_touching_outside_file() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let repo_path = temporary.path().join("repo");
        let shard = repo_path.join("objects/loose/aa");
        let original_shard = repo_path.join("objects/loose/aa-original");
        let outside = temporary.path().join("outside");
        let object_name = "aa00000000000000000000000000000000000000000000000000000000000000";
        fs::create_dir_all(&shard).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(shard.join(object_name), b"repo").unwrap();
        fs::write(outside.join(object_name), b"outside").unwrap();
        let anchored = crate::storage::AnchoredRoot::open(&repo_path).unwrap();
        let relative = Path::new("objects/loose/aa").join(object_name);
        let fingerprint = strong_anchored_file_fingerprint(&anchored, &relative, 4).unwrap();

        let result = remove_repo_file_if_bound_inner(&anchored, &relative, 4, &fingerprint, || {
            fs::rename(&shard, &original_shard).unwrap();
            symlink(&outside, &shard).unwrap();
        });

        assert!(result.is_err());
        assert_eq!(fs::read(outside.join(object_name)).unwrap(), b"outside");
        assert_eq!(fs::read(original_shard.join(object_name)).unwrap(), b"repo");
    }
}
