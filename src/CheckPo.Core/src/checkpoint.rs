use crate::{
    acquire_project_repository_lock, ensure_no_pending_transactions, load_project,
    load_project_snapshot, now_utc_string, object_path, prepare_snapshot,
    publish_prepared_snapshot_root, publish_prepared_snapshot_root_profiled,
    put_object_from_anchored_file_with_known_hash_profiled_batched, report_operation_progress,
    scan_project_for_checkpoint, scan_project_for_checkpoint_with_baseline,
    store_prepared_snapshot_chunks_profiled_batched, verify_stored_object_profiled,
    write_latest_snapshot_id, CheckPoError, CheckpointDeleteResult, CheckpointListResult,
    CheckpointSummary, CreateCheckpointOptions, CreateJournalHandle, FileFingerprintUpdate, Result,
    ScannedFile, SnapshotContent, SnapshotEntry, SnapshotFile, SnapshotId,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use uuid::Uuid;

pub fn create_checkpoint(
    project_path: impl AsRef<Path>,
    name: &str,
    options: CreateCheckpointOptions,
) -> Result<CheckpointSummary> {
    Ok(create_checkpoint_internal(project_path.as_ref(), name, options, false)?.summary)
}

pub fn create_checkpoint_profiled(
    project_path: impl AsRef<Path>,
    name: &str,
    options: CreateCheckpointOptions,
) -> Result<crate::ProfiledCheckpointResult> {
    create_checkpoint_internal(project_path.as_ref(), name, options, true)
}

#[derive(Default)]
struct CheckpointIoRecorders {
    loose_objects: crate::checkpoint_metrics::ArtifactIoRecorder,
    manifest_chunks: crate::checkpoint_metrics::ArtifactIoRecorder,
    snapshot_root: crate::checkpoint_metrics::ArtifactIoRecorder,
}

fn create_checkpoint_internal(
    project_path: &Path,
    name: &str,
    options: CreateCheckpointOptions,
    profiling: bool,
) -> Result<crate::ProfiledCheckpointResult> {
    let total_started = profiling.then(Instant::now);
    let setup_started = profiling.then(Instant::now);
    let mut metrics = crate::CheckpointCreateMetrics::default();
    let io_recorders = profiling.then(CheckpointIoRecorders::default);
    if name.trim().is_empty() {
        return Err(crate::user_error(
            "checkpoint create requires --name <name>.",
        ));
    }
    if name.trim().len() > crate::storage::merkle_codec::MAX_CHECKPOINT_NAME_BYTES {
        return Err(crate::user_error(format!(
            "checkpoint name must be at most {} UTF-8 bytes.",
            crate::storage::merkle_codec::MAX_CHECKPOINT_NAME_BYTES
        )));
    }
    let project = if options.init_if_needed {
        crate::init_project(project_path)?;
        load_project(project_path)?
    } else {
        load_project(project_path)?
    };
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_project_repository_lock(&project, "checkpoint-create")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    crate::ensure_not_cancelled(options.cancellation.as_ref())?;
    metrics.setup_micros = elapsed_micros(setup_started);
    let progress = options.progress.as_deref().map(|f| f as &dyn Fn(_));
    let baseline_started = profiling.then(Instant::now);
    let (parent_snapshot_id, parent_snapshot, parent_manifest_references, latest_warning) =
        latest_checkpoint_for_create(&project)?;
    metrics.baseline_load_micros = elapsed_micros(baseline_started);
    let scan_started = profiling.then(Instant::now);
    let (scanned, scan_warnings, incomplete) = if profiling {
        let (scanned, warnings, incomplete, scan_metrics) =
            crate::scanner::scan_project_for_checkpoint_with_baseline_profiled(
                &project,
                parent_snapshot.as_ref(),
                progress,
                options.cancellation.as_ref(),
            )?;
        metrics.scan = scan_metrics;
        (scanned, warnings, incomplete)
    } else {
        match parent_snapshot.as_ref() {
            Some(parent_snapshot) => scan_project_for_checkpoint_with_baseline(
                &project,
                Some(parent_snapshot),
                progress,
                options.cancellation.as_ref(),
            )?,
            None => scan_project_for_checkpoint(&project, progress, options.cancellation.as_ref())?,
        }
    };
    metrics.scan_total_micros = elapsed_micros(scan_started);
    if incomplete {
        return Err(crate::user_error(format!(
            "checkpoint was not created because some tracked files could not be read: {}",
            scan_warnings
                .iter()
                .map(crate::scanner::format_scan_warning)
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    report_operation_progress(progress, "storeCheckpoint", 0, scanned.len(), None);
    let mut newly_stored_bytes = 0_u64;
    let mut newly_verified_objects = BTreeMap::new();
    let mut files = Vec::with_capacity(scanned.len());
    let object_preload_started = profiling.then(Instant::now);
    let known_parent_objects = parent_snapshot
        .as_ref()
        .into_iter()
        .flat_map(|snapshot| snapshot.files.iter())
        .map(|file| file.content_hash().clone())
        .collect::<BTreeSet<_>>();
    let preloaded_objects =
        preload_available_current_objects(&project.repo_root, &scanned, &known_parent_objects)?;
    metrics.object_preload_micros = elapsed_micros(object_preload_started);
    let anchored_project = crate::storage::AnchoredRoot::open(project.project_root.as_path())?;
    let PreloadedObjects {
        available: available_previous_objects,
        integrity_updates: preloaded_integrity_updates,
        sync_batch: mut object_sync_batch,
    } = preloaded_objects;
    let object_store_started = profiling.then(Instant::now);
    let mut grouped_files = BTreeMap::<crate::ObjectId, Vec<&ScannedFile>>::new();
    for file in &scanned {
        grouped_files
            .entry(file.hash.clone())
            .or_default()
            .push(file);
    }
    for files_with_same_hash in grouped_files.values_mut() {
        files_with_same_hash.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    }
    let object_jobs = grouped_files.into_iter().collect::<Vec<_>>();
    let object_store_parallelism = object_store_parallelism();
    metrics.object_store_parallelism = object_store_parallelism;
    let recorder = io_recorders
        .as_ref()
        .map(|recorders| &recorders.loose_objects);
    let mut completed_files = 0_usize;
    for jobs in object_jobs.chunks(object_store_parallelism) {
        if let Err(error) = crate::ensure_not_cancelled(options.cancellation.as_ref()) {
            object_sync_batch.flush()?;
            return Err(error);
        }
        let outcomes = jobs
            .par_iter()
            .map(|(object_id, grouped)| {
                let mut local_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
                let result = (|| {
                    crate::ensure_not_cancelled(options.cancellation.as_ref())?;
                    let representative = grouped.first().copied().ok_or_else(|| {
                        CheckPoError::Unexpected("empty object group".to_string())
                    })?;
                    let already_available = available_previous_objects.contains(object_id);
                    let created = if already_available {
                        false
                    } else {
                        match put_object_from_anchored_file_with_known_hash_profiled_batched(
                            &project.repo_root,
                            &anchored_project,
                            Path::new(representative.path.as_str()),
                            &representative.full_path,
                            object_id,
                            representative.size_bytes,
                            recorder,
                            &mut local_sync_batch,
                            options.cancellation.as_ref(),
                        ) {
                            Ok(created) => created,
                            Err(crate::CheckPoError::ObjectHashMismatch(_)) => {
                                return Err(crate::CheckPoError::WorkingTreeChanged(
                                    representative.path.to_string(),
                                ))
                            }
                            Err(error) => return Err(error),
                        }
                    };
                    Ok((
                        object_id.clone(),
                        representative.size_bytes,
                        created,
                        grouped.len(),
                        representative.path.to_string(),
                    ))
                })();
                (result, local_sync_batch)
            })
            .collect::<Vec<_>>();
        let mut first_error = None;
        for (outcome, local_batch) in outcomes {
            object_sync_batch.merge(local_batch)?;
            let (object_id, size_bytes, created, group_len, current_item) = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if created {
                newly_stored_bytes = newly_stored_bytes.saturating_add(size_bytes);
                match newly_verified_objects.insert(object_id.clone(), size_bytes) {
                    Some(existing) if existing != size_bytes => {
                        return Err(CheckPoError::Corruption(format!(
                            "object {object_id} has conflicting verified sizes {existing} and {size_bytes}"
                        )))
                    }
                    _ => {}
                }
            }
            completed_files = completed_files.saturating_add(group_len);
            report_operation_progress(
                progress,
                "storeCheckpoint",
                completed_files,
                scanned.len(),
                Some(current_item),
            );
        }
        if let Some(error) = first_error {
            object_sync_batch.flush()?;
            return Err(error);
        }
    }
    if let Err(error) = anchored_project.verify_root_binding() {
        object_sync_batch.flush()?;
        return Err(error);
    }
    files.extend(scanned.iter().map(|file| SnapshotEntry {
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        modified_at_utc: file.modified_at_utc.clone(),
        content: SnapshotContent::Whole {
            hash: file.hash.clone(),
            size_bytes: file.size_bytes,
        },
    }));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    metrics.object_store_micros = elapsed_micros(object_store_started);
    if let Err(error) = crate::ensure_not_cancelled(options.cancellation.as_ref()) {
        object_sync_batch.flush()?;
        return Err(error);
    }
    report_operation_progress(progress, "writeCheckpointMetadata", 0, 1, None);
    let created_at_utc = now_utc_string();
    let snapshot = SnapshotFile {
        schema_version: 2,
        project_id: project.project_id.clone(),
        parent_snapshot_id,
        created_at_utc: created_at_utc.clone(),
        name: name.trim().to_string(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        tracked_roots: vec![
            "Assets".to_string(),
            "Packages".to_string(),
            "ProjectSettings".to_string(),
        ],
        files,
    };
    // Open a current index before publishing the new snapshot. Once the snapshot is
    // visible, the old generation is intentionally reported as stale.
    let snapshot_index = crate::open_index_connection(&project);
    let manifest_build_started = profiling.then(Instant::now);
    let prepared = match prepare_snapshot(&snapshot) {
        Ok(prepared) => prepared,
        Err(error) => {
            object_sync_batch.flush()?;
            return Err(error);
        }
    };
    metrics.manifest_build_micros = elapsed_micros(manifest_build_started);
    let manifest_store_started = profiling.then(Instant::now);
    let mut manifest_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    if let Err(error) = store_prepared_snapshot_chunks_profiled_batched(
        &project.repo_root,
        &prepared,
        io_recorders
            .as_ref()
            .map(|recorders| &recorders.manifest_chunks),
        &mut manifest_sync_batch,
        &parent_manifest_references,
    ) {
        object_sync_batch.flush()?;
        return Err(error);
    }
    metrics.manifest_store_micros = elapsed_micros(manifest_store_started);
    report_operation_progress(progress, "writeCheckpointMetadata", 1, 1, None);
    if let Err(error) = crate::ensure_not_cancelled(options.cancellation.as_ref()) {
        object_sync_batch.flush()?;
        manifest_sync_batch.flush()?;
        return Err(error);
    }

    let object_sync_directory_count = object_sync_batch.total_count();
    let manifest_sync_directory_count = manifest_sync_batch.total_count();
    let total_sync_directory_count =
        object_sync_directory_count.saturating_add(manifest_sync_directory_count);
    let already_synced_directory_count = object_sync_batch
        .completed_count()
        .saturating_add(manifest_sync_batch.completed_count());
    report_operation_progress(
        progress,
        "syncCheckpoint",
        already_synced_directory_count,
        total_sync_directory_count,
        None,
    );
    let durability_barrier_started = profiling.then(Instant::now);
    object_sync_batch.flush_with_progress(
        io_recorders
            .as_ref()
            .map(|recorders| &recorders.loose_objects),
        |completed, _| {
            report_operation_progress(
                progress,
                "syncCheckpoint",
                completed,
                total_sync_directory_count,
                None,
            );
            Ok(())
        },
    )?;
    manifest_sync_batch.flush_with_progress(
        io_recorders
            .as_ref()
            .map(|recorders| &recorders.manifest_chunks),
        |completed, _| {
            report_operation_progress(
                progress,
                "syncCheckpoint",
                object_sync_directory_count.saturating_add(completed),
                total_sync_directory_count,
                None,
            );
            Ok(())
        },
    )?;
    metrics.durability_barrier_micros = elapsed_micros(durability_barrier_started);
    report_operation_progress(
        progress,
        "syncCheckpoint",
        total_sync_directory_count,
        total_sync_directory_count,
        None,
    );
    crate::ensure_not_cancelled(options.cancellation.as_ref())?;

    report_operation_progress(
        progress,
        "readbackCheckpoint",
        0,
        newly_verified_objects.len(),
        None,
    );
    let object_readback_started = profiling.then(Instant::now);
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let newly_verified_object_entries = newly_verified_objects.iter().collect::<Vec<_>>();
    let mut object_integrity_updates = preloaded_integrity_updates;
    object_integrity_updates.reserve(newly_verified_objects.len());
    let mut readback_completed = 0_usize;
    for chunk in newly_verified_object_entries.chunks(256) {
        let updates = chunk
            .par_iter()
            .map(|(object_id, size_bytes)| {
                crate::ensure_not_cancelled(options.cancellation.as_ref())?;
                let fingerprint = verify_stored_object_profiled(
                    &anchored_repo,
                    &project.repo_root,
                    object_id,
                    **size_bytes,
                    io_recorders
                        .as_ref()
                        .map(|recorders| &recorders.loose_objects),
                )?;
                Ok(
                    fingerprint.map(|fingerprint| crate::ObjectIntegrityFingerprintUpdate {
                        object_id: (*object_id).clone(),
                        size_bytes: **size_bytes,
                        fingerprint,
                    }),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        object_integrity_updates.extend(updates.into_iter().flatten());
        readback_completed = readback_completed.saturating_add(chunk.len());
        report_operation_progress(
            progress,
            "readbackCheckpoint",
            readback_completed,
            newly_verified_objects.len(),
            None,
        );
    }
    anchored_repo.verify_root_binding()?;
    metrics.object_readback_micros = elapsed_micros(object_readback_started);
    report_operation_progress(
        progress,
        "readbackCheckpoint",
        newly_verified_objects.len(),
        newly_verified_objects.len(),
        None,
    );

    let object_cache_started = profiling.then(Instant::now);
    let object_cache_warning =
        crate::refresh_object_integrity_fingerprints(&project.repo_root, &object_integrity_updates)
            .err()
            .map(|error| format!("Object integrity fingerprint update failed: {error}"));
    metrics.object_integrity_cache_update_micros = elapsed_micros(object_cache_started);
    // New object sources were copied and hashed from anchored handles. Cached
    // objects intentionally preserve the scan-time snapshot if the working tree
    // changes later; the subsequent diff reports that change instead of making
    // checkpoint publication depend on a second full-tree pass.
    anchored_project.verify_root_binding()?;
    crate::ensure_not_cancelled(options.cancellation.as_ref())?;
    let snapshot_id = prepared.snapshot_id.clone();
    report_operation_progress(progress, "commitCheckpoint", 0, 1, None);
    let root_commit_started = profiling.then(Instant::now);
    let mut create_journal = CreateJournalHandle::prepare(
        &project,
        snapshot_id.clone(),
        snapshot.parent_snapshot_id.clone(),
    )?;
    if let Some(recorders) = io_recorders.as_ref() {
        publish_prepared_snapshot_root_profiled(
            &project.repo_root,
            &prepared,
            &recorders.snapshot_root,
        )?;
    } else {
        publish_prepared_snapshot_root(&project.repo_root, &prepared)?;
    }
    create_journal.mark_root_published()?;
    create_journal.update_inventory(&project)?;
    write_latest_snapshot_id(&project.repo_root, &snapshot_id)?;
    create_journal.mark_latest_updated()?;
    metrics.root_journal_ref_commit_micros = elapsed_micros(root_commit_started);
    report_operation_progress(progress, "commitCheckpoint", 1, 1, None);
    let mut warnings = scan_warnings
        .iter()
        .map(crate::scanner::format_scan_warning)
        .collect::<Vec<_>>();
    if let Some(warning) = latest_warning {
        crate::diagnostics::log_warning("checkpoint-create", &warning);
        warnings.push(warning);
    }
    if let Some(warning) = object_cache_warning {
        warnings.push(warning);
    }
    let snapshot_index_started = profiling.then(Instant::now);
    match snapshot_index {
        Ok(index) => {
            if let Err(error) = crate::index_snapshot_with_index_connection(
                &index,
                &project,
                &snapshot_id,
                &snapshot,
            ) {
                warnings.push(format!("SQLite index update failed: {error}"));
            }
        }
        Err(error) => {
            if crate::storage::inventory_snapshot_count(&project.repo_root, &project.project_id)?
                == 1
            {
                if let Err(rebuild_error) =
                    crate::rebuild_index_for_project_unlocked(&project, None, None)
                {
                    warnings.push(format!("SQLite index rebuild failed: {rebuild_error}"));
                }
            } else {
                warnings.push(format!("SQLite index update failed: {error}"));
            }
        }
    }
    metrics.snapshot_index_update_micros = elapsed_micros(snapshot_index_started);
    let journal_commit_started = profiling.then(Instant::now);
    if let Some(warning) = create_journal.commit() {
        warnings.push(warning);
    }
    metrics.root_journal_ref_commit_micros = metrics
        .root_journal_ref_commit_micros
        .saturating_add(elapsed_micros(journal_commit_started));
    let fingerprint_update_started = profiling.then(Instant::now);
    if let Err(error) = refresh_fingerprints_after_checkpoint(&project, &scanned) {
        warnings.push(format!("SQLite fingerprint update failed: {error}"));
    }
    metrics.file_fingerprint_update_micros = elapsed_micros(fingerprint_update_started);
    report_operation_progress(progress, "complete", 1, 1, None);
    let summary = CheckpointSummary {
        checkpoint_id: snapshot_id,
        name: snapshot.name,
        created_at_utc,
        file_count: snapshot.files.len(),
        logical_size_bytes: snapshot.files.iter().map(|file| file.size_bytes).sum(),
        newly_stored_bytes,
        warnings,
    };
    metrics.io = crate::CheckpointIoMetrics {
        loose_objects: io_recorders
            .as_ref()
            .map(|recorders| recorders.loose_objects.snapshot())
            .unwrap_or_default(),
        manifest_chunks: io_recorders
            .as_ref()
            .map(|recorders| recorders.manifest_chunks.snapshot())
            .unwrap_or_default(),
        snapshot_root: io_recorders
            .as_ref()
            .map(|recorders| recorders.snapshot_root.snapshot())
            .unwrap_or_default(),
    };
    metrics.total_micros = elapsed_micros(total_started);
    let attributed = metrics
        .setup_micros
        .saturating_add(metrics.baseline_load_micros)
        .saturating_add(metrics.scan_total_micros)
        .saturating_add(metrics.object_preload_micros)
        .saturating_add(metrics.object_store_micros)
        .saturating_add(metrics.object_integrity_cache_update_micros)
        .saturating_add(metrics.manifest_build_micros)
        .saturating_add(metrics.manifest_store_micros)
        .saturating_add(metrics.durability_barrier_micros)
        .saturating_add(metrics.object_readback_micros)
        .saturating_add(metrics.root_journal_ref_commit_micros)
        .saturating_add(metrics.snapshot_index_update_micros)
        .saturating_add(metrics.file_fingerprint_update_micros);
    metrics.unattributed_micros = metrics.total_micros.saturating_sub(attributed);
    Ok(crate::ProfiledCheckpointResult {
        summary,
        create_metrics: metrics,
    })
}

fn elapsed_micros(started: Option<Instant>) -> u64 {
    started
        .map(|started| crate::checkpoint_metrics::duration_micros(started.elapsed()))
        .unwrap_or_default()
}

struct PreloadedObjects {
    available: BTreeSet<crate::ObjectId>,
    integrity_updates: Vec<crate::ObjectIntegrityFingerprintUpdate>,
    sync_batch: crate::storage::AnchoredParentSyncBatch,
}

fn preload_available_current_objects(
    repo_root: &Path,
    scanned: &[ScannedFile],
    known_durable: &BTreeSet<crate::ObjectId>,
) -> Result<PreloadedObjects> {
    let mut expected_sizes = BTreeMap::new();
    for file in scanned {
        match expected_sizes.insert(file.hash.clone(), file.size_bytes) {
            Some(existing) if existing != file.size_bytes => {
                return Err(CheckPoError::Corruption(format!(
                    "object {} has conflicting expected sizes",
                    file.hash
                )))
            }
            _ => {}
        }
    }
    let mut objects_by_shard =
        BTreeMap::<PathBuf, Vec<(std::ffi::OsString, crate::ObjectId, u64)>>::new();
    for (object_id, expected_size) in expected_sizes {
        let object = object_path(repo_root, &object_id);
        let relative = object.strip_prefix(repo_root).map_err(|_| {
            CheckPoError::Corruption(format!(
                "object path is outside repository {}: {}",
                repo_root.display(),
                object.display()
            ))
        })?;
        let shard = relative.parent().ok_or_else(|| {
            CheckPoError::Corruption(format!("invalid object path: {}", object.display()))
        })?;
        let leaf = relative.file_name().ok_or_else(|| {
            CheckPoError::Corruption(format!("invalid object path: {}", object.display()))
        })?;
        objects_by_shard
            .entry(shard.to_path_buf())
            .or_default()
            .push((leaf.to_os_string(), object_id, expected_size));
    }
    let cached = crate::load_object_integrity_fingerprints(repo_root).unwrap_or_default();
    let anchored_repo = crate::storage::AnchoredRoot::open(repo_root)?;
    let verified = objects_by_shard
        .into_par_iter()
        .map(|(shard_relative, objects)| {
            let mut local_sync_batch = crate::storage::AnchoredParentSyncBatch::new();
            let parent = match anchored_repo.open_directory(&shard_relative, false) {
                Ok(parent) => parent,
                Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                    return Ok((Vec::new(), local_sync_batch))
                }
                Err(CheckPoError::Io { source, .. })
                    if unsafe_directory_component_error(&source) =>
                {
                    return Err(CheckPoError::Corruption(format!(
                        "unsafe object shard directory: {}",
                        repo_root.join(&shard_relative).display()
                    )))
                }
                Err(error) => return Err(error),
            };
            let mut outcomes = Vec::with_capacity(objects.len());
            for (leaf, object_id, expected_size) in objects {
                let metadata = match parent.inspect_metadata_no_follow(&leaf) {
                    Ok(metadata) => metadata,
                    Err(CheckPoError::Io { source, .. })
                        if source.kind() == ErrorKind::NotFound =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if metadata.is_link || !metadata.is_regular {
                    return Err(CheckPoError::Corruption(format!(
                        "unsafe loose object: {}",
                        parent.display_path().join(&leaf).display()
                    )));
                }
                if metadata.size_bytes != expected_size {
                    continue;
                }
                let cache_matches = metadata.fingerprint.as_deref().is_some_and(|fingerprint| {
                    cached.get(&object_id).is_some_and(|record| {
                        record.size_bytes == expected_size && record.fingerprint == fingerprint
                    })
                });
                let object_is_known_durable = known_durable.contains(&object_id);
                if cache_matches && object_is_known_durable {
                    outcomes.push((object_id, None));
                    continue;
                }
                let mut opened = match parent.open_file(&leaf) {
                    Ok(opened) => opened,
                    Err(CheckPoError::Io { source, .. })
                        if source.kind() == ErrorKind::NotFound =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let update = if cache_matches {
                    None
                } else {
                    let hashed = opened.hash()?;
                    if hashed.object_id != object_id || hashed.metadata.len() != expected_size {
                        continue;
                    }
                    opened.fingerprint()?.map(|fingerprint| {
                        crate::ObjectIntegrityFingerprintUpdate {
                            object_id: object_id.clone(),
                            size_bytes: expected_size,
                            fingerprint,
                        }
                    })
                };
                parent.verify_file_binding(&leaf, &opened)?;
                if !object_is_known_durable {
                    opened.sync_all()?;
                    anchored_repo.defer_directory_chain(&shard_relative, &mut local_sync_batch)?;
                }
                outcomes.push((object_id, update));
            }
            anchored_repo.verify_parent_binding(&shard_relative, &parent)?;
            Ok((outcomes, local_sync_batch))
        })
        .collect::<Result<Vec<_>>>()?;
    anchored_repo.verify_root_binding()?;
    let mut available = BTreeSet::new();
    let mut updates = Vec::new();
    let mut sync_batch = crate::storage::AnchoredParentSyncBatch::new();
    for (outcomes, local_batch) in verified {
        sync_batch.merge(local_batch)?;
        for (object_id, update) in outcomes {
            available.insert(object_id);
            if let Some(update) = update {
                updates.push(update);
            }
        }
    }
    Ok(PreloadedObjects {
        available,
        integrity_updates: updates,
        sync_batch,
    })
}

fn unsafe_directory_component_error(error: &std::io::Error) -> bool {
    if error.kind() == ErrorKind::NotADirectory {
        return true;
    }
    #[cfg(unix)]
    {
        error
            .raw_os_error()
            .is_some_and(|code| code == libc::ELOOP || code == libc::ENOTDIR)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// These values form one validated baseline and must be returned together; a
// tuple keeps the call site from accidentally mixing fields from another load.
#[allow(clippy::type_complexity)]
fn latest_checkpoint_for_create(
    project: &crate::ProjectContext,
) -> Result<(
    Option<SnapshotId>,
    Option<SnapshotFile>,
    BTreeSet<crate::storage::merkle_codec::ManifestRef>,
    Option<String>,
)> {
    let Some(snapshot_id) = crate::read_latest_snapshot_id(&project.repo_root)? else {
        if crate::storage::inventory_snapshot_count(&project.repo_root, &project.project_id)? == 0 {
            return Ok((None, None, BTreeSet::new(), None));
        }
        return Err(CheckPoError::Corruption(
            "refs/latest is missing while published checkpoints exist; repair the reference before creating another checkpoint"
                .to_string(),
        ));
    };
    let (snapshot, manifest_references) =
        crate::storage::load_project_snapshot_with_manifest_references(project, &snapshot_id)?;
    Ok((Some(snapshot_id), Some(snapshot), manifest_references, None))
}

fn object_store_parallelism() -> usize {
    std::env::var("CHECKPO_OBJECT_WRITE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=16).contains(value))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|value| value.get())
                .unwrap_or(1)
                .min(8)
        })
}

pub fn list_checkpoints(project_path: impl AsRef<Path>) -> Result<Vec<CheckpointSummary>> {
    let mut project = load_project(&project_path)?;
    if project.location_status != crate::ProjectLocationStatus::CopiedSuspected {
        recover_checkpoint_deletions(&project_path)?;
        project = load_project(project_path)?;
    }
    list_checkpoints_for_project(&project)
}

pub fn list_checkpoints_for_project(
    project: &crate::ProjectContext,
) -> Result<Vec<CheckpointSummary>> {
    Ok(list_checkpoints_with_warnings_for_project(project)?.checkpoints)
}

pub fn list_checkpoints_with_warnings_for_project(
    project: &crate::ProjectContext,
) -> Result<CheckpointListResult> {
    let _lock = crate::acquire_project_repository_shared_lock(project, "checkpoint-list")?;
    let mut checkpoints = crate::list_checkpoint_summaries_from_index(project)?;
    let mut warnings = Vec::new();
    warnings.extend(crate::apply_checkpoint_name_overrides(
        project,
        &mut checkpoints,
    ));
    warnings.sort();
    warnings.dedup();
    Ok(CheckpointListResult {
        checkpoints,
        warnings,
    })
}

pub fn rename_checkpoint(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
    name: &str,
) -> Result<CheckpointSummary> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::user_error(
            "checkpoint rename requires --name <name>.",
        ));
    }
    if name.len() > crate::storage::merkle_codec::MAX_CHECKPOINT_NAME_BYTES {
        return Err(crate::user_error(format!(
            "checkpoint name must be at most {} UTF-8 bytes.",
            crate::storage::merkle_codec::MAX_CHECKPOINT_NAME_BYTES
        )));
    }
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_project_repository_lock(&project, "checkpoint-rename")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    let id = SnapshotId::parse(checkpoint_id)?;
    let snapshot = load_project_snapshot(&project, &id)?;
    let (mut names, warnings) = crate::read_checkpoint_name_overrides(&project);
    if !warnings.is_empty() {
        return Err(crate::CheckPoError::Corruption(format!(
            "checkpoint display names cannot be modified until their metadata is repaired: {}",
            warnings.join("; ")
        )));
    }
    if name == snapshot.name {
        names.remove(id.as_str());
    } else {
        names.insert(id.to_string(), name.to_string());
    }
    crate::write_checkpoint_name_overrides(&project, &names)?;
    Ok(summary_from_snapshot(
        id,
        &snapshot,
        name.to_string(),
        Vec::new(),
    ))
}

pub fn delete_checkpoint(
    project_path: impl AsRef<Path>,
    checkpoint_id: &str,
) -> Result<CheckpointDeleteResult> {
    let project = load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_project_repository_lock(&project, "checkpoint-delete")?;
    ensure_no_pending_transactions(&project)?;
    crate::ensure_no_unresolved_transaction_quarantines(&project)?;
    // Full physical/inventory reconciliation is intentionally reserved for
    // verify, GC, and index rebuild. Doing it before every deletion makes a
    // sequence of deletions quadratic in the history length. The deletion is
    // instead bound to the current content-addressed inventory head and to the
    // held snapshot root below.
    let inventory_head_before =
        crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
    let id = SnapshotId::parse(checkpoint_id)?;
    let path = crate::snapshot_path(&project.repo_root, &id);
    let snapshot_relative = path.strip_prefix(&project.repo_root).map_err(|_| {
        CheckPoError::Unexpected(format!(
            "checkpoint root is outside its repository: {}",
            path.display()
        ))
    })?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut anchored_snapshot = match anchored_repo.open_file(snapshot_relative) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Err(crate::CheckPoError::SnapshotNotFound(id.to_string()))
        }
        Err(error) => return Err(error),
    };
    let snapshot_bytes =
        anchored_snapshot.read_bounded(crate::storage::merkle_codec::MAX_ROOT_BYTES as u64)?;
    if crate::snapshot_id_from_bytes(&snapshot_bytes) != id {
        return Err(CheckPoError::Corruption(format!(
            "checkpoint root digest does not match {id}"
        )));
    }
    let deleted_snapshot =
        crate::storage::decode_snapshot_root_bytes(&project.repo_root, &id, &snapshot_bytes)?;
    if deleted_snapshot.project_id != project.project_id {
        return Err(CheckPoError::Corruption(
            "checkpoint belongs to another project".to_string(),
        ));
    }
    let update_index = matches!(
        crate::checkpoint_index_status(&project),
        Ok(status) if status.state == crate::CheckpointIndexState::Current
    );
    let (_, name_warnings) = crate::read_checkpoint_name_overrides(&project);
    if !name_warnings.is_empty() {
        return Err(crate::CheckPoError::Corruption(format!(
            "checkpoint display names cannot be modified until their metadata is repaired: {}",
            name_warnings.join("; ")
        )));
    }
    let old_latest = crate::read_latest_snapshot_id(&project.repo_root)?;
    let (remaining_checkpoint_count, new_latest) = crate::storage::project_snapshot_removal(
        &project.repo_root,
        &project.project_id,
        &id,
        old_latest.as_ref(),
        deleted_snapshot.parent_snapshot_id.as_ref(),
    )?;
    let transaction_id = Uuid::new_v4().simple().to_string();
    let transaction_relative = checkpoint_deletion_transaction_relative(&transaction_id);
    let transaction_dir = checkpoint_deletion_root(&project.repo_root).join(&transaction_id);
    let mut journal = CheckpointDeletionJournal {
        schema_version: CHECKPOINT_DELETION_JOURNAL_SCHEMA_VERSION,
        transaction_id,
        checkpoint_id: id.clone(),
        old_latest,
        new_latest,
        remaining_checkpoint_count,
        update_index,
        inventory_head_before,
        state: CheckpointDeletionState::Prepared,
        created_at_utc: now_utc_string(),
    };
    let initial_journal_bytes = serde_json::to_vec(&journal)
        .map_err(|error| crate::json_error(transaction_dir.join("journal.json"), error))?;
    crate::storage::prepare_and_publish_journal_transaction(
        &project.repo_root,
        crate::storage::JournalFamily::CheckpointDelete,
        &journal.transaction_id,
        &initial_journal_bytes,
    )?;
    let transaction_anchor = anchored_repo.open_directory(&transaction_relative, false)?;
    anchored_repo.verify_parent_binding(&transaction_relative, &transaction_anchor)?;
    let (snapshot_parent, snapshot_leaf) =
        anchored_repo.open_parent_for_mutation(snapshot_relative, false)?;
    let staged_relative = transaction_relative.join("snapshot.root");
    let (staged_parent, staged_leaf) =
        anchored_repo.open_parent_for_mutation(&staged_relative, false)?;
    let mut staged_file = staged_parent.create_new_file(&staged_leaf)?;
    staged_file
        .write_all(&snapshot_bytes)
        .map_err(|error| crate::io_error(&staged_relative, error))?;
    staged_file.sync_all()?;
    staged_parent.verify_file_binding(&staged_leaf, &staged_file)?;
    staged_parent.sync_all()?;
    let staged_readback = anchored_repo.read_bytes_bounded_path(
        &transaction_dir.join("snapshot.root"),
        crate::storage::merkle_codec::MAX_ROOT_BYTES as u64,
    )?;
    if staged_readback != snapshot_bytes
        || crate::snapshot_id_from_bytes(&staged_readback) != journal.checkpoint_id
    {
        return Err(CheckPoError::Corruption(format!(
            "staged checkpoint readback does not match {}",
            journal.checkpoint_id
        )));
    }
    anchored_repo.verify_root_binding()?;
    journal.state = CheckpointDeletionState::Staged;
    write_checkpoint_deletion_journal(&project.repo_root, &transaction_dir, &journal)?;
    snapshot_parent.unlink_file_if_bound(&snapshot_leaf, anchored_snapshot)?;
    snapshot_parent.sync_all()?;
    drop(staged_file);
    drop(staged_parent);
    drop(transaction_anchor);
    let warnings = finish_checkpoint_deletion(
        &project,
        &anchored_repo,
        &transaction_dir,
        &mut journal,
        &deleted_snapshot,
    )?;
    Ok(CheckpointDeleteResult {
        deleted_checkpoint_id: id,
        deleted_snapshot_path: path,
        remaining_checkpoint_count,
        warnings,
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointDeletionJournal {
    schema_version: u32,
    transaction_id: String,
    checkpoint_id: SnapshotId,
    old_latest: Option<SnapshotId>,
    new_latest: Option<SnapshotId>,
    remaining_checkpoint_count: usize,
    update_index: bool,
    inventory_head_before: String,
    state: CheckpointDeletionState,
    created_at_utc: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum CheckpointDeletionState {
    Prepared,
    Staged,
    InventoryUpdated,
    Committed,
}

const CHECKPOINT_DELETION_JOURNAL_SCHEMA_VERSION: u32 = 4;

fn checkpoint_deletion_root(repo_root: &Path) -> PathBuf {
    repo_root.join("journals").join("checkpoint-delete")
}

fn checkpoint_deletion_transaction_relative(transaction_id: &str) -> PathBuf {
    Path::new("journals")
        .join("checkpoint-delete")
        .join(transaction_id)
}

fn write_checkpoint_deletion_journal(
    repo_root: &Path,
    transaction_dir: &Path,
    journal: &CheckpointDeletionJournal,
) -> Result<()> {
    crate::storage::AnchoredRoot::open(repo_root)?
        .write_json_atomic_path(&transaction_dir.join("journal.json"), journal)
}

fn finish_checkpoint_deletion(
    project: &crate::ProjectContext,
    anchored_repo: &crate::storage::AnchoredRoot,
    transaction_dir: &Path,
    journal: &mut CheckpointDeletionJournal,
    deleted_snapshot: &SnapshotFile,
) -> Result<Vec<String>> {
    if matches!(
        journal.state,
        CheckpointDeletionState::Prepared | CheckpointDeletionState::Staged
    ) {
        crate::storage::remove_snapshot_from_inventory_if_head(
            &project.repo_root,
            &project.project_id,
            &journal.checkpoint_id,
            &journal.inventory_head_before,
            &journal.transaction_id,
        )?;
        journal.state = CheckpointDeletionState::InventoryUpdated;
        write_checkpoint_deletion_journal(&project.repo_root, transaction_dir, journal)?;
    }
    update_latest_for_checkpoint_deletion(
        &project.repo_root,
        anchored_repo,
        journal.new_latest.as_ref(),
    )?;
    let mut warnings = Vec::new();
    if journal.update_index {
        if let Err(error) =
            crate::delete_snapshot_from_index(project, &journal.checkpoint_id, deleted_snapshot)
        {
            warnings.push(format!(
                "SQLite index update failed after checkpoint delete: {error}"
            ));
        }
    } else {
        warnings.push(
            "SQLite index was already unavailable; rebuild it to refresh the checkpoint list"
                .to_string(),
        );
    }
    match crate::remove_checkpoint_name_override(project, &journal.checkpoint_id) {
        Ok(name_warnings) => warnings.extend(name_warnings),
        Err(error) => warnings.push(format!(
            "checkpoint display name cleanup failed after delete: {error}"
        )),
    }
    journal.state = CheckpointDeletionState::Committed;
    if let Err(error) =
        write_checkpoint_deletion_journal(&project.repo_root, transaction_dir, journal)
    {
        let warning = format!(
            "checkpoint deletion was committed, but its journal could not be finalized and will be recovered later: {error}"
        );
        crate::diagnostics::log_warning("checkpoint-delete-commit", &warning);
        warnings.push(warning);
        return Ok(warnings);
    }
    if let Some(warning) = cleanup_committed_checkpoint_deletion_transaction(
        &project.repo_root,
        &journal.transaction_id,
    ) {
        warnings.push(warning);
    }
    Ok(warnings)
}

fn update_latest_for_checkpoint_deletion(
    repo_root: &Path,
    anchored_repo: &crate::storage::AnchoredRoot,
    latest: Option<&SnapshotId>,
) -> Result<()> {
    if let Some(latest) = latest {
        anchored_repo.verify_root_binding()?;
        return write_latest_snapshot_id(repo_root, latest);
    }
    let relative = Path::new("refs/latest");
    let (parent, leaf) = match anchored_repo.open_parent_for_mutation(relative, false) {
        Ok(value) => value,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    let expected = match parent.open_file(&leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    anchored_repo.verify_parent_binding(Path::new("refs"), &parent)?;
    parent.unlink_file_if_bound(&leaf, expected)?;
    parent.sync_all()?;
    anchored_repo.verify_root_binding()
}

fn cleanup_checkpoint_deletion_transaction(repo_root: &Path, transaction_id: &str) -> Result<()> {
    crate::storage::detach_and_cleanup_journal_transaction(
        repo_root,
        crate::storage::JournalFamily::CheckpointDelete,
        transaction_id,
    )
}

fn cleanup_committed_checkpoint_deletion_transaction(
    repo_root: &Path,
    transaction_id: &str,
) -> Option<String> {
    cleanup_checkpoint_deletion_transaction(repo_root, transaction_id)
        .err()
        .map(|error| {
            let warning = format!(
                "checkpoint deletion was committed, but journal cleanup was deferred until recovery: {error}"
            );
            crate::diagnostics::log_warning("checkpoint-delete-cleanup", &warning);
            warning
        })
}

pub fn recover_checkpoint_deletions(project_path: impl AsRef<Path>) -> Result<()> {
    let project = load_project(project_path)?;
    let _lock = acquire_project_repository_lock(&project, "checkpoint-delete-recovery")?;
    Ok(())
}

pub(crate) fn recover_checkpoint_deletions_unlocked(
    project: &crate::ProjectContext,
) -> Result<bool> {
    // A detached transaction is cleanup-only state. Drain it before parsing
    // active journals so a crash after the directory rename is idempotent.
    let recovered_cleanup = crate::storage::drain_journal_cleanup_trash(
        &project.repo_root,
        crate::storage::JournalFamily::CheckpointDelete,
    )?;
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let family_relative = Path::new("journals/checkpoint-delete");
    let family = match anchored_repo.open_directory(family_relative, false) {
        Ok(family) => family,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(recovered_cleanup)
        }
        Err(error) => return Err(error),
    };
    anchored_repo.verify_parent_binding(family_relative, &family)?;
    let mut recovered_any = recovered_cleanup;
    let mut transaction_ids = family.list_entry_names()?;
    transaction_ids.sort();
    for transaction_id in transaction_ids {
        if crate::storage::is_journal_cleanup_name(&transaction_id) {
            continue;
        }
        recovered_any = true;
        let transaction_relative = family_relative.join(&transaction_id);
        let transaction_dir = project.repo_root.join(&transaction_relative);
        let transaction = anchored_repo.open_directory(&transaction_relative, false)?;
        anchored_repo.verify_parent_binding(&transaction_relative, &transaction)?;
        drop(transaction);
        let mut journal: CheckpointDeletionJournal =
            anchored_repo.read_json_path(&transaction_dir.join("journal.json"))?;
        if journal.schema_version != CHECKPOINT_DELETION_JOURNAL_SCHEMA_VERSION
            || journal.transaction_id != transaction_id.to_string_lossy()
        {
            return Err(CheckPoError::Corruption(format!(
                "checkpoint deletion journal identity is invalid: {}",
                transaction_dir.display()
            )));
        }
        let original = crate::snapshot_path(&project.repo_root, &journal.checkpoint_id);
        let staged = transaction_dir.join("snapshot.root");
        let original_relative = original.strip_prefix(&project.repo_root).map_err(|_| {
            CheckPoError::Corruption("checkpoint root escaped repository".to_string())
        })?;
        let original_exists = match anchored_repo.open_file(original_relative) {
            Ok(_) => true,
            Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        let staged_relative = transaction_relative.join("snapshot.root");
        let staged_exists = match anchored_repo.open_file(&staged_relative) {
            Ok(_) => true,
            Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        // Prepared means the durable intent has not crossed the deletion
        // commit boundary. If the original still exists, abort safely whether
        // or not a durable staged copy was already published.
        if original_exists && journal.state == CheckpointDeletionState::Prepared {
            cleanup_checkpoint_deletion_transaction(&project.repo_root, &journal.transaction_id)?;
            continue;
        }
        if !original_exists && !staged_exists && journal.state == CheckpointDeletionState::Committed
        {
            let _ = cleanup_committed_checkpoint_deletion_transaction(
                &project.repo_root,
                &journal.transaction_id,
            );
            continue;
        }
        if !staged_exists {
            return Err(CheckPoError::Corruption(format!(
                "checkpoint deletion journal has inconsistent snapshot state: {}",
                transaction_dir.display()
            )));
        }
        let bytes = anchored_repo.read_bytes_bounded_path(
            &staged,
            crate::storage::merkle_codec::MAX_ROOT_BYTES as u64,
        )?;
        if crate::snapshot_id_from_bytes(&bytes) != journal.checkpoint_id {
            return Err(CheckPoError::Corruption(format!(
                "staged checkpoint digest does not match {}",
                journal.checkpoint_id
            )));
        }
        let snapshot = crate::storage::decode_snapshot_root_bytes(
            &project.repo_root,
            &journal.checkpoint_id,
            &bytes,
        )?;
        if snapshot.project_id != project.project_id {
            return Err(CheckPoError::Corruption(
                "staged checkpoint belongs to another project".to_string(),
            ));
        }
        if original_exists {
            if journal.state != CheckpointDeletionState::Staged {
                return Err(CheckPoError::Corruption(format!(
                    "checkpoint deletion journal retained its original after {:?}: {}",
                    journal.state,
                    transaction_dir.display()
                )));
            }
            let (original_parent, original_leaf) =
                anchored_repo.open_parent_for_mutation(original_relative, false)?;
            let mut original_file = original_parent.open_file(&original_leaf)?;
            let original_bytes =
                original_file.read_bounded(crate::storage::merkle_codec::MAX_ROOT_BYTES as u64)?;
            if original_bytes != bytes
                || crate::snapshot_id_from_bytes(&original_bytes) != journal.checkpoint_id
            {
                return Err(CheckPoError::WorkingTreeChanged(
                    original.display().to_string(),
                ));
            }
            original_parent.unlink_file_if_bound(&original_leaf, original_file)?;
            original_parent.sync_all()?;
        }
        if journal.state == CheckpointDeletionState::Committed {
            let _ = cleanup_committed_checkpoint_deletion_transaction(
                &project.repo_root,
                &journal.transaction_id,
            );
        } else {
            if journal.state == CheckpointDeletionState::Prepared {
                journal.state = CheckpointDeletionState::Staged;
                write_checkpoint_deletion_journal(&project.repo_root, &transaction_dir, &journal)?;
            }
            let _ = finish_checkpoint_deletion(
                project,
                &anchored_repo,
                &transaction_dir,
                &mut journal,
                &snapshot,
            )?;
        }
    }
    Ok(recovered_any)
}

fn summary_from_snapshot(
    checkpoint_id: SnapshotId,
    snapshot: &SnapshotFile,
    name: String,
    warnings: Vec<String>,
) -> CheckpointSummary {
    CheckpointSummary {
        checkpoint_id,
        name,
        created_at_utc: snapshot.created_at_utc.clone(),
        file_count: snapshot.files.len(),
        logical_size_bytes: snapshot.files.iter().map(|file| file.size_bytes).sum(),
        newly_stored_bytes: 0,
        warnings,
    }
}

fn refresh_fingerprints_after_checkpoint(
    project: &crate::ProjectContext,
    scanned: &[ScannedFile],
) -> Result<()> {
    let mut updates = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for file in scanned {
        seen_paths.insert(file.path.clone());
        let Some(fingerprint) = file.fingerprint.clone() else {
            continue;
        };
        updates.push(FileFingerprintUpdate {
            path: file.path.clone(),
            size_bytes: file.size_bytes,
            modified_at_utc: file.modified_at_utc.clone(),
            fingerprint,
            object_id: file.hash.clone(),
        });
    }
    crate::refresh_file_fingerprints(project, &updates, &seen_paths)
}
