use crate::{io_error, json_error, CheckPoError, ObjectId, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

mod file;
mod parent;
mod platform;
mod root;
mod sync_batch;
#[cfg(test)]
mod tests;

use platform::*;

/// A directory that remains bound to the inode opened at construction time.
///
/// On Unix every descendant is opened relative to this handle with
/// `O_NOFOLLOW`. Renaming or replacing the path used to construct this value
/// therefore cannot redirect later reads outside the directory that was
/// originally approved.
pub(crate) struct AnchoredRoot {
    display_path: PathBuf,
    identity: FileIdentity,
    #[cfg(any(unix, windows))]
    directory: File,
}

pub(crate) struct AnchoredFile {
    display_path: PathBuf,
    file: File,
    identity: FileIdentity,
}

pub(crate) struct AnchoredParent {
    display_path: PathBuf,
    directory: File,
    identity: FileIdentity,
}

/// Held parent-directory descriptors whose entry updates form one durability
/// barrier. The bounded pending set avoids exhausting the process descriptor
/// limit on projects with many distinct directories; an early partial flush is
/// safe because it only makes already-created entries durable sooner.
pub(crate) struct AnchoredParentSyncBatch {
    parents: Vec<AnchoredParent>,
    max_pending: usize,
    completed_count: usize,
    unreported_sync_duration: std::time::Duration,
    unreported_sync_count: usize,
}

/// The subset of file metadata needed by the scanner's cache fast path.
///
/// On Unix this is produced by one `fstatat(AT_SYMLINK_NOFOLLOW)` against a
/// held parent-directory descriptor.  In particular, collecting it does not
/// open every unchanged source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnchoredFileMetadata {
    pub(crate) size_bytes: u64,
    pub(crate) modified: std::time::SystemTime,
    pub(crate) fingerprint: Option<String>,
    pub(crate) is_regular: bool,
    pub(crate) is_link: bool,
}

pub(crate) struct AnchoredHash {
    pub(crate) object_id: ObjectId,
    pub(crate) metadata: fs::Metadata,
    /// Opaque proof of the exact handle version observed after hashing.
    /// Callers must not recapture this after `hash`: doing so would admit a
    /// write that raced between the hash and the second metadata read.
    pub(crate) version: AnchoredFileVersion,
}

#[derive(Clone, Copy)]
pub(crate) struct AnchoredFileVersion {
    full: FileVersion,
    #[cfg(unix)]
    stable_content: StableFileVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
    #[cfg(not(any(unix, windows)))]
    length: u64,
    #[cfg(not(any(unix, windows)))]
    modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    identity: FileIdentity,
    length: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
    #[cfg(windows)]
    changed: i64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplaceProtocolPhase {
    RecoveryRecordDurable,
    DestinationDetached,
    ReplacementPublished,
}

#[cfg(windows)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowsReplaceRecoveryRecord {
    version: u32,
    destination_leaf_utf16: Vec<u16>,
    temporary_leaf_utf16: Vec<u16>,
    tombstone_leaf_utf16: Vec<u16>,
    old_volume_serial_number: u64,
    old_file_id: [u8; 16],
    new_volume_serial_number: u64,
    new_file_id: [u8; 16],
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StableFileVersion {
    identity: FileIdentity,
    length: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

fn validate_content_addressed_bytes(
    file: &mut AnchoredFile,
    path: &Path,
    expected: &[u8],
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<()> {
    let metadata = file.metadata()?;
    let actual = if metadata.len() == expected.len() as u64 {
        measure_anchored_io(
            recorder,
            crate::checkpoint_metrics::IoTimingKind::ExistingValidationRead,
            || file.read_bounded(expected.len() as u64),
        )?
    } else {
        Vec::new()
    };
    if actual != expected {
        return Err(CheckPoError::Corruption(format!(
            "content-addressed destination conflicts with expected bytes: {}",
            path.display()
        )));
    }
    Ok(())
}

fn measure_anchored_io<T>(
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    kind: crate::checkpoint_metrics::IoTimingKind,
    operation: impl FnOnce() -> T,
) -> T {
    match recorder {
        Some(recorder) => recorder.measure(kind, operation),
        None => operation(),
    }
}

impl FileIdentity {
    fn is_definitely_on_different_volume(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device != other.device
        }

        #[cfg(windows)]
        {
            self.volume_serial_number != 0
                && other.volume_serial_number != 0
                && self.volume_serial_number != other.volume_serial_number
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = other;
            false
        }
    }

    #[cfg(not(windows))]
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            Ok(Self {
                length: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    #[cfg(windows)]
    fn from_file(path: &Path, file: &File) -> Result<Self> {
        use std::mem::MaybeUninit;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
        };

        let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if result == 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        let info = unsafe { info.assume_init() };
        Ok(Self {
            volume_serial_number: info.VolumeSerialNumber,
            file_id: info.FileId.Identifier,
        })
    }
}

#[cfg(windows)]
fn windows_file_basic_info(
    file: &File,
    path: &Path,
) -> Result<windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
    };

    let mut info = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    Ok(unsafe { info.assume_init() })
}

impl FileVersion {
    fn from_file_metadata(file: &File, path: &Path, metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        {
            let _ = (file, path);
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                identity: FileIdentity::from_metadata(metadata)?,
                length: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
            })
        }

        #[cfg(windows)]
        {
            let basic = windows_file_basic_info(file, path)?;
            Ok(Self {
                identity: FileIdentity::from_file(path, file)?,
                length: metadata.len(),
                modified: metadata.modified().ok(),
                changed: basic.ChangeTime,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (file, path);
            Ok(Self {
                identity: FileIdentity::from_metadata(metadata)?,
                length: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }
    }

    #[cfg(unix)]
    fn stable_content(self) -> StableFileVersion {
        StableFileVersion {
            identity: self.identity,
            length: self.length,
            modified_seconds: self.modified_seconds,
            modified_nanoseconds: self.modified_nanoseconds,
        }
    }
}

impl AnchoredFileVersion {
    fn from_full(full: FileVersion) -> Self {
        Self {
            full,
            #[cfg(unix)]
            stable_content: full.stable_content(),
        }
    }
}
