use crate::{
    canonical_utc, hash_file, relative_path_from_project, report_operation_progress,
    CancellationToken, CheckPoError, OperationProgress, Result, ScanWarning, ScannedFile,
    TrackedUnityFilePath,
};
use rayon::prelude::*;
use std::fs;
use std::path::Path;
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

pub fn scan_project_for_checkpoint(
    project: &crate::ProjectContext,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<(Vec<ScannedFile>, Vec<ScanWarning>)> {
    let cached = if platform_fingerprint_is_strong_enough_for_hash_reuse() {
        crate::load_file_fingerprints(project).unwrap_or_default()
    } else {
        Default::default()
    };
    scan_project_internal(
        project.project_root.as_path(),
        Some(&cached),
        progress,
        cancellation,
    )
}

fn scan_project_internal(
    project_root: &Path,
    cached: Option<&std::collections::BTreeMap<TrackedUnityFilePath, crate::CachedFileFingerprint>>,
    progress: Option<&dyn Fn(OperationProgress)>,
    cancellation: Option<&CancellationToken>,
) -> Result<(Vec<ScannedFile>, Vec<ScanWarning>)> {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    for root in ["Assets", "Packages", "ProjectSettings"] {
        crate::ensure_not_cancelled(cancellation)?;
        let root_path = project_root.join(root);
        if !root_path.exists() {
            continue;
        }
        if !root_path.is_dir() {
            warnings.push(ScanWarning {
                relative_path: root.to_string(),
                reason: "tracked root is not a directory".to_string(),
            });
            continue;
        }
        for entry in WalkDir::new(&root_path).follow_links(false).into_iter() {
            crate::ensure_not_cancelled(cancellation)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(ScanWarning {
                        relative_path: root.to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if entry.file_type().is_symlink() || entry.file_type().is_dir() {
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let full_path = entry.path().to_path_buf();
            let relative = match relative_path_from_project(project_root, &full_path) {
                Ok(relative) => relative,
                Err(error) => {
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
                    warnings.push(ScanWarning {
                        relative_path: relative,
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            let leaf_metadata = match fs::symlink_metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(ScanWarning {
                        relative_path: path.to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if leaf_metadata.file_type().is_symlink() {
                warnings.push(ScanWarning {
                    relative_path: path.to_string(),
                    reason: "symlink files are not supported".to_string(),
                });
                continue;
            }
            let metadata = match fs::metadata(&full_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(ScanWarning {
                        relative_path: path.to_string(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata
                .modified()
                .map_err(|error| crate::io_error(&full_path, error))?;
            let fingerprint = file_fingerprint(&full_path, &metadata)?;
            let hash = cached
                .and_then(|records| records.get(&path))
                .filter(|record| {
                    Some(record.fingerprint.as_str()) == fingerprint.as_deref()
                        && record.size_bytes == metadata.len()
                })
                .map(|record| record.object_id.clone())
                .map(Some)
                .unwrap_or(None);
            files.push(PendingScannedFile {
                path,
                full_path,
                size_bytes: metadata.len(),
                modified_at_utc: canonical_utc(modified),
                hash,
                fingerprint,
            });
        }
    }
    let total = files.len();
    let mut completed = 0_usize;
    for chunk in files.chunks_mut(PARALLEL_HASH_CHUNK_SIZE) {
        crate::ensure_not_cancelled(cancellation)?;
        chunk.par_iter_mut().try_for_each(|file| -> Result<()> {
            crate::ensure_not_cancelled(cancellation)?;
            if file.hash.is_none() {
                file.hash = Some(hash_file(&file.full_path)?);
            }
            Ok(())
        })?;
        completed += chunk.len();
        report_operation_progress(
            progress,
            "scan",
            completed,
            total,
            chunk.last().map(|file| file.path.to_string()),
        );
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
    Ok((files, warnings))
}

fn is_checkpo_temporary_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if name.starts_with(".checkpo-") && name.ends_with(".tmp") {
        return true;
    }
    if !name.starts_with('.') || !name.ends_with(".tmp") {
        return false;
    }
    let body = &name[1..name.len() - ".tmp".len()];
    let Some((_, suffix)) = body.rsplit_once('.') else {
        return false;
    };
    suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
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
