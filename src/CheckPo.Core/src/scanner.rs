use crate::{
    canonical_utc, is_checkpo_temporary_file, relative_path_from_project,
    report_operation_progress, CancellationToken, CheckPoError, OperationProgress, Result,
    ScanWarning, ScannedFile, SnapshotEntry, SnapshotFile, TrackedUnityFilePath,
};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;

const PARALLEL_HASH_CHUNK_SIZE: usize = 64;

struct PendingScannedFile {
    path: TrackedUnityFilePath,
    full_path: std::path::PathBuf,
    size_bytes: u64,
    modified_at_utc: String,
    hash: Option<crate::ObjectId>,
    fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ScannedMetadataFile {
    pub(crate) path: TrackedUnityFilePath,
    pub(crate) size_bytes: u64,
    pub(crate) modified_at_utc: String,
}

pub fn scan_project_for_checkpoint(
    project: &crate::ProjectContext,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<(Vec<ScannedFile>, Vec<ScanWarning>, bool)> {
    // Callers that can prove a snapshot is an appropriate cache baseline pass it
    // explicitly. With no baseline every source file is hashed, so an unrelated
    // or unreadable refs/latest can never affect a scan.
    scan_project_for_checkpoint_with_baseline(project, None, progress, cancellation)
}

pub(crate) fn scan_project_for_checkpoint_with_baseline(
    project: &crate::ProjectContext,
    baseline: Option<&SnapshotFile>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<(Vec<ScannedFile>, Vec<ScanWarning>, bool)> {
    let cached = if platform_fingerprint_is_strong_enough_for_hash_reuse() {
        crate::load_file_fingerprints(project).unwrap_or_default()
    } else {
        Default::default()
    };
    let baseline_files = baseline.map(|snapshot| {
        snapshot
            .files
            .iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>()
    });
    scan_project_internal(
        project.project_root.as_path(),
        Some(&cached),
        baseline_files.as_ref(),
        progress,
        cancellation,
        None,
    )
}

pub(crate) fn scan_project_for_checkpoint_with_baseline_profiled(
    project: &crate::ProjectContext,
    baseline: Option<&SnapshotFile>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<(
    Vec<ScannedFile>,
    Vec<ScanWarning>,
    bool,
    crate::CheckpointScanMetrics,
)> {
    let cached = if platform_fingerprint_is_strong_enough_for_hash_reuse() {
        crate::load_file_fingerprints(project).unwrap_or_default()
    } else {
        Default::default()
    };
    let baseline_files = baseline.map(|snapshot| {
        snapshot
            .files
            .iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>()
    });
    let mut metrics = crate::CheckpointScanMetrics::default();
    let (files, warnings, incomplete) = scan_project_internal(
        project.project_root.as_path(),
        Some(&cached),
        baseline_files.as_ref(),
        progress,
        cancellation,
        Some(&mut metrics),
    )?;
    Ok((files, warnings, incomplete, metrics))
}

pub(crate) fn scan_project_metadata(
    project_root: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<(Vec<ScannedMetadataFile>, Vec<ScanWarning>)> {
    let (files, warnings, _) = collect_project_files(project_root, cancellation)?;
    Ok((
        files
            .into_iter()
            .map(|file| ScannedMetadataFile {
                path: file.path,
                size_bytes: file.size_bytes,
                modified_at_utc: file.modified_at_utc,
            })
            .collect(),
        warnings,
    ))
}

pub(crate) fn format_scan_warning(warning: &ScanWarning) -> String {
    format!("{}: {}", warning.relative_path, warning.reason)
}

fn scan_project_internal(
    project_root: &Path,
    cached: Option<&std::collections::BTreeMap<TrackedUnityFilePath, crate::CachedFileFingerprint>>,
    baseline: Option<&BTreeMap<TrackedUnityFilePath, &SnapshotEntry>>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
    mut metrics: Option<&mut crate::CheckpointScanMetrics>,
) -> Result<(Vec<ScannedFile>, Vec<ScanWarning>, bool)> {
    let anchored_root = crate::storage::AnchoredRoot::open(project_root)?;
    let enumerate_started = metrics.as_ref().map(|_| Instant::now());
    let (files, mut warnings, mut incomplete) = collect_project_files(project_root, cancellation)?;
    if let (Some(metrics), Some(started)) = (metrics.as_deref_mut(), enumerate_started) {
        metrics.enumerate_micros = crate::checkpoint_metrics::duration_micros(started.elapsed());
    }
    // Open each parent directory once per assessment group. On Unix unchanged
    // leaves then need only one fstatat(AT_SYMLINK_NOFOLLOW), instead of reopening
    // and walking every path component for every file. Windows retains its strong
    // handle fingerprint behind the same parent-relative API.
    let assessment_started = metrics.as_ref().map(|_| Instant::now());
    let mut files_by_parent =
        BTreeMap::<PathBuf, Vec<(std::ffi::OsString, PendingScannedFile)>>::new();
    for file in files {
        let relative = Path::new(file.path.as_str());
        let parent = relative.parent().ok_or_else(|| {
            CheckPoError::Corruption(format!("tracked path has no parent: {}", file.path))
        })?;
        let leaf = relative.file_name().ok_or_else(|| {
            CheckPoError::Corruption(format!("tracked path has no filename: {}", file.path))
        })?;
        files_by_parent
            .entry(parent.to_path_buf())
            .or_default()
            .push((leaf.to_os_string(), file));
    }
    let assessed = files_by_parent
        .into_par_iter()
        .map(|(parent_relative, grouped)| {
            let parent = match anchored_root.open_directory(&parent_relative, false) {
                Ok(parent) => parent,
                Err(error) => {
                    return Ok(grouped
                        .into_iter()
                        .map(|(_, file)| {
                            (
                                None,
                                Some(ScanWarning {
                                    relative_path: file.path.to_string(),
                                    reason: format!(
                                        "file parent could not be opened safely: {error}"
                                    ),
                                }),
                            )
                        })
                        .collect::<Vec<_>>());
                }
            };
            let outcomes = grouped
                .into_par_iter()
                .map(|(leaf, mut file)| {
                    crate::ensure_not_cancelled(cancellation)?;
                    let inspected = match parent.inspect_metadata_no_follow(&leaf) {
                        Ok(inspected) => inspected,
                        Err(error) => {
                            return Ok((
                                None,
                                Some(ScanWarning {
                                    relative_path: file.path.to_string(),
                                    reason: format!(
                                        "file metadata could not be read safely: {error}"
                                    ),
                                }),
                            ));
                        }
                    };
                    if inspected.is_link || !inspected.is_regular {
                        return Ok((
                            None,
                            Some(ScanWarning {
                                relative_path: file.path.to_string(),
                                reason: "symbolic links and non-regular files are not supported"
                                    .to_string(),
                            }),
                        ));
                    }
                    if inspected.size_bytes != file.size_bytes
                        || canonical_utc(inspected.modified) != file.modified_at_utc
                    {
                        return Ok((
                            None,
                            Some(ScanWarning {
                                relative_path: file.path.to_string(),
                                reason: "file changed while it was being scanned".to_string(),
                            }),
                        ));
                    }
                    file.fingerprint = inspected.fingerprint;
                    file.hash = match (
                        cached.and_then(|records| records.get(&file.path)),
                        baseline.and_then(|entries| entries.get(&file.path)),
                    ) {
                        (Some(record), Some(entry))
                            if Some(record.fingerprint.as_str()) == file.fingerprint.as_deref()
                                && record.size_bytes == file.size_bytes
                                && entry.size_bytes == file.size_bytes
                                && entry.content_size_bytes() == file.size_bytes
                                && entry.content_hash() == &record.object_id
                                && entry.modified_at_utc == file.modified_at_utc =>
                        {
                            Some(record.object_id.clone())
                        }
                        _ => None,
                    };
                    Ok((Some(file), None))
                })
                .collect::<Result<Vec<_>>>()?;
            anchored_root.verify_parent_binding(&parent_relative, &parent)?;
            Ok(outcomes)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if let (Some(metrics), Some(started)) = (metrics.as_deref_mut(), assessment_started) {
        metrics.fingerprint_assessment_micros =
            crate::checkpoint_metrics::duration_micros(started.elapsed());
    }
    let mut valid_files = Vec::with_capacity(assessed.len());
    for (file, warning) in assessed {
        if let Some(warning) = warning {
            if file.is_none() {
                incomplete = true;
            }
            warnings.push(warning);
        }
        if let Some(file) = file {
            valid_files.push(file);
        }
    }
    let mut files = valid_files;
    let total = files.len();
    if let Some(metrics) = metrics.as_deref_mut() {
        for file in &files {
            if file.hash.is_some() {
                metrics.reused_file_count += 1;
                metrics.reused_bytes = metrics.reused_bytes.saturating_add(file.size_bytes);
            } else {
                metrics.hashed_file_count += 1;
                metrics.hashed_bytes = metrics.hashed_bytes.saturating_add(file.size_bytes);
            }
        }
    }
    let hash_started = metrics.as_ref().map(|_| Instant::now());
    let mut completed = 0_usize;
    for chunk in files.chunks_mut(PARALLEL_HASH_CHUNK_SIZE) {
        crate::ensure_not_cancelled(cancellation)?;
        let chunk_warnings = chunk
            .par_iter_mut()
            .map(|file| -> Result<Option<ScanWarning>> {
                crate::ensure_not_cancelled(cancellation)?;
                if file.hash.is_some() {
                    return Ok(None);
                }
                let relative = Path::new(file.path.as_str());
                let hashed = (|| -> Result<crate::ObjectId> {
                    let mut opened = anchored_root.open_file(relative)?;
                    let hashed = opened.hash()?;
                    anchored_root.verify_binding(relative, &opened)?;
                    let modified = hashed
                        .metadata
                        .modified()
                        .map_err(|error| crate::io_error(&file.full_path, error))?;
                    let fingerprint = opened.fingerprint()?;
                    if hashed.metadata.len() != file.size_bytes
                        || canonical_utc(modified) != file.modified_at_utc
                        || fingerprint != file.fingerprint
                    {
                        return Err(CheckPoError::WorkingTreeChanged(file.path.to_string()));
                    }
                    Ok(hashed.object_id)
                })();
                match hashed {
                    Ok(hash) => {
                        file.hash = Some(hash);
                        Ok(None)
                    }
                    Err(error) => Ok(Some(ScanWarning {
                        relative_path: file.path.to_string(),
                        reason: format!("file content could not be read: {error}"),
                    })),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        if chunk_warnings.iter().any(Option::is_some) {
            incomplete = true;
        }
        warnings.extend(chunk_warnings.into_iter().flatten());
        completed += chunk.len();
        report_operation_progress(
            progress,
            "scan",
            completed,
            total,
            chunk.last().map(|file| file.path.to_string()),
        );
    }
    if let (Some(metrics), Some(started)) = (metrics.as_deref_mut(), hash_started) {
        metrics.hash_wall_micros = crate::checkpoint_metrics::duration_micros(started.elapsed());
    }
    let finalize_started = metrics.as_ref().map(|_| Instant::now());
    files.retain(|file| file.hash.is_some());
    files.sort_by(|a, b| a.path.cmp(&b.path));
    anchored_root.verify_root_binding()?;
    if files
        .iter()
        .any(|file| !file.full_path.starts_with(project_root))
    {
        return Err(CheckPoError::OutsideTrackedScope(
            project_root.display().to_string(),
        ));
    }
    let files = files
        .into_iter()
        .map(|file| {
            let hash = file.hash.ok_or_else(|| {
                CheckPoError::Unexpected(format!("scan hash missing for {}", file.path))
            })?;
            Ok(ScannedFile {
                path: file.path,
                full_path: file.full_path,
                size_bytes: file.size_bytes,
                modified_at_utc: file.modified_at_utc,
                hash,
                fingerprint: file.fingerprint,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if let (Some(metrics), Some(started)) = (metrics, finalize_started) {
        metrics.finalize_micros = crate::checkpoint_metrics::duration_micros(started.elapsed());
    }
    Ok((files, warnings, incomplete))
}

fn collect_project_files(
    project_root: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<(Vec<PendingScannedFile>, Vec<ScanWarning>, bool)> {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut incomplete = false;
    for root in ["Assets", "Packages", "ProjectSettings"] {
        crate::ensure_not_cancelled(cancellation)?;
        let root_path = project_root.join(root);
        match fs::symlink_metadata(&root_path) {
            Ok(metadata) if metadata.is_dir() && !crate::metadata_is_link_or_reparse(&metadata) => {
            }
            Ok(metadata) => {
                incomplete = true;
                warnings.push(ScanWarning {
                    relative_path: root.to_string(),
                    reason: if crate::metadata_is_link_or_reparse(&metadata) {
                        "tracked root is a symbolic link or reparse point".to_string()
                    } else {
                        "tracked root is not a directory".to_string()
                    },
                });
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                incomplete = true;
                warnings.push(ScanWarning {
                    relative_path: root.to_string(),
                    reason: format!("tracked root metadata could not be read: {error}"),
                });
                continue;
            }
        }
        let mut entries = WalkDir::new(&root_path).follow_links(false).into_iter();
        while let Some(entry) = entries.next() {
            crate::ensure_not_cancelled(cancellation)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    incomplete = true;
                    warnings.push(ScanWarning {
                        relative_path: root.to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            let full_path = entry.path().to_path_buf();
            let entry_metadata = match fs::symlink_metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if entry.file_type().is_dir() {
                        entries.skip_current_dir();
                    }
                    incomplete = true;
                    warnings.push(ScanWarning {
                        relative_path: full_path
                            .strip_prefix(project_root)
                            .unwrap_or(&full_path)
                            .to_string_lossy()
                            .replace('\\', "/"),
                        reason: format!("path metadata could not be read: {error}"),
                    });
                    continue;
                }
            };
            if crate::metadata_is_link_or_reparse(&entry_metadata) {
                if entry.file_type().is_dir() || entry_metadata.is_dir() {
                    entries.skip_current_dir();
                }
                incomplete = true;
                warnings.push(ScanWarning {
                    relative_path: full_path
                        .strip_prefix(project_root)
                        .unwrap_or(&full_path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    reason: "symbolic links and reparse points are not supported".to_string(),
                });
                continue;
            }
            if entry_metadata.is_dir() {
                continue;
            }
            if !entry_metadata.is_file() {
                continue;
            }
            let relative = match relative_path_from_project(project_root, &full_path) {
                Ok(relative) => relative,
                Err(error) => {
                    incomplete = true;
                    warnings.push(ScanWarning {
                        relative_path: full_path.display().to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if is_checkpo_temporary_file(entry.path()) {
                warnings.push(ScanWarning {
                    relative_path: relative,
                    reason: "temporary CheckPo file was skipped".to_string(),
                });
                continue;
            }
            let path = match TrackedUnityFilePath::parse(&relative) {
                Ok(path) => path,
                Err(error) => {
                    incomplete = true;
                    warnings.push(ScanWarning {
                        relative_path: relative,
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            let modified = match entry_metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    incomplete = true;
                    warnings.push(ScanWarning {
                        relative_path: path.to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            files.push(PendingScannedFile {
                path,
                full_path,
                size_bytes: entry_metadata.len(),
                modified_at_utc: canonical_utc(modified),
                hash: None,
                fingerprint: None,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if files
        .iter()
        .any(|file| !file.full_path.starts_with(project_root))
    {
        return Err(CheckPoError::OutsideTrackedScope(
            project_root.display().to_string(),
        ));
    }
    Ok((files, warnings, incomplete))
}

#[cfg(unix)]
fn platform_fingerprint_is_strong_enough_for_hash_reuse() -> bool {
    true
}

#[cfg(windows)]
fn platform_fingerprint_is_strong_enough_for_hash_reuse() -> bool {
    true
}

#[cfg(not(any(unix, windows)))]
fn platform_fingerprint_is_strong_enough_for_hash_reuse() -> bool {
    false
}

#[cfg(unix)]
pub(crate) fn file_fingerprint(_path: &Path, metadata: &fs::Metadata) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    Ok(Some(format!(
        "unix-v1:{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )))
}

#[cfg(windows)]
pub(crate) fn file_fingerprint(path: &Path, metadata: &fs::Metadata) -> Result<Option<String>> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO,
    };

    let file = fs::File::open(path).map_err(|error| crate::io_error(path, error))?;
    let mut handle_info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), handle_info.as_mut_ptr()) };
    if ok == 0 {
        return Err(crate::io_error(path, std::io::Error::last_os_error()));
    }
    let handle_info = unsafe { handle_info.assume_init() };

    let mut basic_info = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            basic_info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(crate::io_error(path, std::io::Error::last_os_error()));
    }
    let basic_info = unsafe { basic_info.assume_init() };
    let file_index = ((handle_info.nFileIndexHigh as u64) << 32) | handle_info.nFileIndexLow as u64;
    Ok(Some(format!(
        "windows-v2:{}:{}:{}:{}:{}:{}:{}",
        handle_info.dwVolumeSerialNumber,
        file_index,
        metadata.len(),
        basic_info.CreationTime,
        basic_info.LastWriteTime,
        basic_info.ChangeTime,
        handle_info.dwFileAttributes
    )))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn file_fingerprint(_path: &Path, _metadata: &fs::Metadata) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(test)]
mod benchmarks {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore = "creates 30,000 files; run explicitly for scanner performance validation"]
    fn cached_scan_thirty_thousand_files_in_one_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        let dense = project.join("Assets/Dense");
        fs::create_dir_all(&dense).unwrap();
        fs::create_dir_all(project.join("Packages")).unwrap();
        fs::create_dir_all(project.join("ProjectSettings")).unwrap();
        for index in 0..30_000_u32 {
            fs::File::create(dense.join(format!("file-{index:05}.asset"))).unwrap();
        }

        let (initial, warnings, incomplete) =
            scan_project_internal(project, None, None, None, None, None).unwrap();
        assert!(!incomplete, "initial scan warnings: {warnings:?}");
        assert_eq!(initial.len(), 30_000);

        let cached = initial
            .iter()
            .map(|file| {
                (
                    file.path.clone(),
                    crate::CachedFileFingerprint {
                        size_bytes: file.size_bytes,
                        fingerprint: file
                            .fingerprint
                            .clone()
                            .expect("benchmark platform must provide strong fingerprints"),
                        object_id: file.hash.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let entries = initial
            .iter()
            .map(|file| SnapshotEntry {
                path: file.path.clone(),
                size_bytes: file.size_bytes,
                modified_at_utc: file.modified_at_utc.clone(),
                content: crate::SnapshotContent::Whole {
                    hash: file.hash.clone(),
                    size_bytes: file.size_bytes,
                },
            })
            .collect::<Vec<_>>();
        let baseline = entries
            .iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>();

        let mut metrics = crate::CheckpointScanMetrics::default();
        let started = Instant::now();
        let (cached_scan, warnings, incomplete) = scan_project_internal(
            project,
            Some(&cached),
            Some(&baseline),
            None,
            None,
            Some(&mut metrics),
        )
        .unwrap();
        let elapsed = started.elapsed();

        assert!(!incomplete, "cached scan warnings: {warnings:?}");
        assert_eq!(cached_scan.len(), 30_000);
        assert_eq!(metrics.reused_file_count, 30_000);
        assert_eq!(metrics.hashed_file_count, 0);
        println!(
            "files={} total_ms={:.1} enumerate_ms={:.1} fingerprint_ms={:.1} hash_ms={:.1}",
            cached_scan.len(),
            elapsed.as_secs_f64() * 1000.0,
            metrics.enumerate_micros as f64 / 1000.0,
            metrics.fingerprint_assessment_micros as f64 / 1000.0,
            metrics.hash_wall_micros as f64 / 1000.0,
        );
    }

    #[test]
    #[ignore = "requires CHECKPO_BENCH_PROJECT"]
    fn strong_fingerprint_parallel_benchmark() {
        let project = std::env::var_os("CHECKPO_BENCH_PROJECT")
            .map(std::path::PathBuf::from)
            .expect("CHECKPO_BENCH_PROJECT is required");
        let enumerate_started = Instant::now();
        let (files, warnings, incomplete) = collect_project_files(&project, None).unwrap();
        let enumerate_elapsed = enumerate_started.elapsed();
        assert!(!incomplete, "scan warnings: {warnings:?}");

        let fingerprint_started = Instant::now();
        let fingerprints = files
            .par_iter()
            .map(|file| {
                let metadata = fs::metadata(&file.full_path)
                    .map_err(|error| crate::io_error(&file.full_path, error))?;
                file_fingerprint(&file.full_path, &metadata)
            })
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let fingerprint_elapsed = fingerprint_started.elapsed();
        assert_eq!(fingerprints.len(), files.len());
        println!(
            "files={} enumerate_ms={:.1} fingerprint_ms={:.1}",
            files.len(),
            enumerate_elapsed.as_secs_f64() * 1000.0,
            fingerprint_elapsed.as_secs_f64() * 1000.0
        );
    }

    #[test]
    #[ignore = "requires CHECKPO_BENCH_PROJECT and CHECKPO_BENCH_REPO"]
    fn cached_checkpoint_scan_benchmark() {
        let project = std::env::var_os("CHECKPO_BENCH_PROJECT")
            .map(std::path::PathBuf::from)
            .expect("CHECKPO_BENCH_PROJECT is required");
        let repo = std::env::var_os("CHECKPO_BENCH_REPO")
            .map(std::path::PathBuf::from)
            .expect("CHECKPO_BENCH_REPO is required");
        let project_id = crate::ProjectId::parse(
            repo.file_name()
                .and_then(|value| value.to_str())
                .expect("repo directory must be the project id"),
        )
        .unwrap();
        let storage_root = repo
            .parent()
            .and_then(|repos| repos.parent())
            .expect("repo must be under <storage>/repos")
            .to_path_buf();
        let context = crate::ProjectContext {
            project_id,
            project_root: crate::ProjectRoot::new(project),
            storage_root: crate::StorageRoot::new(storage_root),
            repo_root: repo,
            location_status: crate::ProjectLocationStatus::Current,
            warnings: Vec::new(),
        };
        let latest = crate::read_latest_snapshot_id(&context.repo_root)
            .unwrap()
            .expect("latest snapshot is required");
        let baseline = crate::load_project_snapshot(&context, &latest).unwrap();

        let started = Instant::now();
        let (files, warnings, incomplete) =
            scan_project_for_checkpoint_with_baseline(&context, Some(&baseline), None, None)
                .unwrap();
        let elapsed = started.elapsed();
        assert!(!incomplete, "scan warnings: {warnings:?}");
        let object_check_started = Instant::now();
        let mut unique_objects = std::collections::BTreeMap::new();
        for file in &baseline.files {
            unique_objects.insert(file.content_hash().clone(), file.content_size_bytes());
        }
        let unique_objects = unique_objects.into_iter().collect::<Vec<_>>();
        let checked_objects = unique_objects
            .clone()
            .into_par_iter()
            .map(|(object_id, size_bytes)| {
                let path = crate::object_path(&context.repo_root, &object_id);
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert!(metadata.is_file());
                assert_eq!(metadata.len(), size_bytes);
                object_id
            })
            .collect::<Vec<_>>();
        let object_check_elapsed = object_check_started.elapsed();
        let safe_object_check_started = Instant::now();
        crate::ensure_regular_directory_no_follow(&context.repo_root.join("objects/loose"))
            .unwrap();
        let mut object_shards = std::collections::BTreeSet::new();
        for (object_id, _) in &unique_objects {
            let path = crate::object_path(&context.repo_root, object_id);
            let shard = path.parent().unwrap().to_path_buf();
            object_shards.insert(shard);
        }
        object_shards
            .par_iter()
            .for_each(|path| crate::ensure_regular_directory_no_follow(path).unwrap());
        let safely_checked_objects = unique_objects
            .into_par_iter()
            .map(|(object_id, size_bytes)| {
                let path = crate::object_path(&context.repo_root, &object_id);
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert!(metadata.is_file());
                assert_eq!(metadata.len(), size_bytes);
                object_id
            })
            .collect::<Vec<_>>();
        let safe_object_check_elapsed = safe_object_check_started.elapsed();
        println!(
            "files={} warnings={} cached_scan_ms={:.1} unique_objects={} object_check_ms={:.1} shards={} safe_object_check_ms={:.1}",
            files.len(),
            warnings.len(),
            elapsed.as_secs_f64() * 1000.0,
            checked_objects.len(),
            object_check_elapsed.as_secs_f64() * 1000.0,
            object_shards.len(),
            safe_object_check_elapsed.as_secs_f64() * 1000.0
        );
        assert_eq!(safely_checked_objects.len(), checked_objects.len());
    }
}
