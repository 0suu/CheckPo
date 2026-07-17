#[cfg(windows)]
use crate::{io_error, Result};
#[cfg(windows)]
use std::fs::{self, File};
#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::mem::{offset_of, size_of};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};
#[cfg(windows)]
use std::path::{Path, PathBuf};

#[cfg(windows)]
use windows_sys::Wdk::Storage::FileSystem::{
    FileDispositionInformation, FileRenameInformation, FileRenameInformationEx,
    NtSetInformationFile, FILE_DISPOSITION_INFORMATION, FILE_RENAME_INFORMATION,
    FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileDispositionInfoEx, FileIdExtdDirectoryInfo,
    FileIdExtdDirectoryRestartInfo, FileIdInfo, FlushFileBuffers, GetFileInformationByHandleEx,
    SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_WRITE_THROUGH, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectoryEntryIdentity {
    file_id: [u8; 16],
    is_directory: bool,
    is_reparse_point: bool,
}

/// Crash-recovery classification for a rename whose source identity was
/// recorded before mutation. Kept independent of Win32 calls so every state
/// in the recovery matrix is exercised on all development platforms.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameRecoveryState {
    RetryFromSource,
    Committed,
    AmbiguousDuplicate,
    Missing,
    Conflict,
}

#[cfg(test)]
fn classify_rename_recovery_state(
    source_matches_expected: Option<bool>,
    destination_matches_expected: Option<bool>,
) -> RenameRecoveryState {
    use RenameRecoveryState::*;

    match (source_matches_expected, destination_matches_expected) {
        (Some(true), None) => RetryFromSource,
        (None, Some(true)) => Committed,
        (Some(true), Some(true)) => AmbiguousDuplicate,
        (None, None) => Missing,
        _ => Conflict,
    }
}

#[cfg(windows)]
pub(super) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    rename_durable(source, destination, false, false)
}

#[cfg(windows)]
pub(super) fn rename_open_handle_no_replace(
    source: &File,
    destination_parent: &File,
    destination_leaf: &std::ffi::OsStr,
) -> io::Result<()> {
    rename_open_handle_no_replace_inner(source, destination_parent, destination_leaf, false, true)
}

#[cfg(windows)]
pub(super) fn rename_open_handle_no_replace_unflushed(
    source: &File,
    destination_parent: &File,
    destination_leaf: &std::ffi::OsStr,
) -> io::Result<()> {
    rename_open_handle_no_replace_inner(source, destination_parent, destination_leaf, false, false)
}

#[cfg(windows)]
pub(super) fn rename_open_directory_handle_no_replace(
    source: &File,
    destination_parent: &File,
    destination_leaf: &std::ffi::OsStr,
) -> io::Result<()> {
    rename_open_handle_no_replace_inner(source, destination_parent, destination_leaf, true, true)
}

#[cfg(windows)]
fn rename_open_handle_no_replace_inner(
    source: &File,
    destination_parent: &File,
    destination_leaf: &std::ffi::OsStr,
    expected_directory: bool,
    flush_file: bool,
) -> io::Result<()> {
    let source_identity = file_identity(Path::new("<held-source>"), source)?;
    if directory_entry_identity(destination_parent, destination_leaf)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    if flush_file {
        flush_handle(Path::new("<held-source>"), source)?;
    }
    set_rename_information(source, destination_parent, destination_leaf, false)?;
    if flush_file {
        flush_handle(Path::new("<held-source>"), source)?;
    }
    match directory_entry_identity(destination_parent, destination_leaf)? {
        Some(entry)
            if entry.file_id == source_identity.file_id
                && entry.is_directory == expected_directory
                && !entry.is_reparse_point =>
        {
            Ok(())
        }
        _ => Err(io::Error::other("held-handle rename identity mismatch")),
    }
}

#[cfg(windows)]
pub(super) fn remove_open_handle(
    file: File,
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> io::Result<()> {
    remove_open_handle_inner(file, parent, leaf, false)
}

#[cfg(windows)]
pub(super) fn remove_open_directory_handle(
    directory: File,
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> io::Result<()> {
    remove_open_handle_inner(directory, parent, leaf, true)
}

#[cfg(windows)]
fn remove_open_handle_inner(
    file: File,
    parent: &File,
    leaf: &std::ffi::OsStr,
    expected_directory: bool,
) -> io::Result<()> {
    let expected = file_identity(Path::new("<held-remove>"), &file)?;
    match directory_entry_identity(parent, leaf)? {
        Some(entry)
            if entry.file_id == expected.file_id
                && entry.is_directory == expected_directory
                && !entry.is_reparse_point => {}
        Some(_) => return Err(io::Error::other("held remove target identity changed")),
        None => return Err(io::Error::from(io::ErrorKind::NotFound)),
    }
    if expected_directory {
        flush_handle(Path::new("<held-remove>"), &file)?;
        let disposition = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                file.as_raw_handle() as HANDLE,
                &mut io_status,
                (&disposition as *const FILE_DISPOSITION_INFORMATION).cast(),
                size_of::<FILE_DISPOSITION_INFORMATION>() as u32,
                FileDispositionInformation,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(unsafe {
                RtlNtStatusToDosError(status) as i32
            }));
        }
    } else {
        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfoEx,
                (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(file);
    match directory_entry_identity(parent, leaf)? {
        None => flush_handle(Path::new("<held-parent>"), parent),
        Some(entry) if entry.file_id == expected.file_id => Err(io::Error::other(
            "held remove remains pending because another handle is open",
        )),
        Some(_) => Err(io::Error::other(
            "held remove target was replaced concurrently",
        )),
    }
}

#[cfg(windows)]
pub(super) fn rename_replace(source: &Path, destination: &Path) -> Result<()> {
    rename_durable(source, destination, true, false).map_err(|error| io_error(destination, error))
}

#[cfg(all(test, windows))]
pub(super) fn remove_file(path: &Path) -> Result<()> {
    let invalid_path = |message: &'static str| {
        io_error(path, io::Error::new(io::ErrorKind::InvalidInput, message))
    };
    let parent = path
        .parent()
        .ok_or_else(|| invalid_path("path has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| invalid_path("path has no file name"))?;
    let parent_handle = open_existing(
        parent,
        GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES,
        true,
    )
    .map_err(|error| io_error(parent, error))?;
    ensure_expected_kind(parent, &parent_handle, true).map_err(|error| io_error(parent, error))?;
    let expected = directory_entry_identity(&parent_handle, leaf)
        .map_err(|error| io_error(path, error))?
        .ok_or_else(|| io_error(path, io::Error::from(io::ErrorKind::NotFound)))?;
    if expected.is_directory || expected.is_reparse_point {
        return Err(io_error(
            path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "durable remove target is not a regular file",
            ),
        ));
    }

    let file = open_existing(
        path,
        GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES,
        false,
    )
    .map_err(|error| io_error(path, error))?;
    ensure_expected_kind(path, &file, false).map_err(|error| io_error(path, error))?;
    let opened = file_identity(path, &file).map_err(|error| io_error(path, error))?;
    let parent_identity =
        file_identity(parent, &parent_handle).map_err(|error| io_error(parent, error))?;
    if opened.volume_serial_number != parent_identity.volume_serial_number
        || opened.file_id != expected.file_id
    {
        return Err(io_error(
            path,
            io::Error::other("durable remove target changed while opening"),
        ));
    }
    flush_handle(path, &file).map_err(|error| io_error(path, error))?;
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if result == 0 {
        return Err(io_error(path, io::Error::last_os_error()));
    }
    drop(file);

    match directory_entry_identity(&parent_handle, leaf).map_err(|error| io_error(path, error))? {
        None => {}
        Some(current) if current.file_id == expected.file_id => {
            return Err(io_error(
                path,
                io::Error::other("durable remove remains pending because another handle is open"),
            ));
        }
        Some(_) => {
            return Err(io_error(
                path,
                io::Error::other("durable remove destination was replaced concurrently"),
            ));
        }
    }
    flush_handle(parent, &parent_handle)
        .map_err(|error| io_error(parent, durable_barrier_error(parent, error)))
}

#[cfg(windows)]
fn rename_durable(
    source: &Path,
    destination: &Path,
    replace: bool,
    source_is_directory: bool,
) -> io::Result<()> {
    let source_handle = open_existing(
        source,
        GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES,
        source_is_directory,
    )?;
    ensure_expected_kind(source, &source_handle, source_is_directory)?;
    let source_identity = file_identity(source, &source_handle)?;

    let destination_parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    let destination_parent_handle = open_existing(
        destination_parent,
        GENERIC_READ | FILE_READ_ATTRIBUTES,
        true,
    )?;
    ensure_expected_kind(destination_parent, &destination_parent_handle, true)?;
    let destination_volume = file_identity(destination_parent, &destination_parent_handle)?;
    if source_identity.volume_serial_number != destination_volume.volume_serial_number {
        return Err(io::Error::from_raw_os_error(17));
    }
    let destination_leaf = destination.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
    })?;
    if !replace && directory_entry_identity(&destination_parent_handle, destination_leaf)?.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("destination already exists: {}", destination.display()),
        ));
    }

    flush_handle(source, &source_handle)?;
    if let Err(error) = set_rename_information(
        &source_handle,
        &destination_parent_handle,
        destination_leaf,
        replace,
    ) {
        if matches!(error.raw_os_error(), Some(80 | 183))
            || (!replace
                && directory_entry_identity(&destination_parent_handle, destination_leaf)?
                    .is_some())
        {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, error));
        }
        return Err(map_unsupported_filesystem_operation(
            "handle-relative rename",
            error,
        ));
    }

    // The rename metadata is flushed through the still-open source handle.
    // A readback through the same held parent handle then proves that the
    // published leaf resolves to the exact file that was renamed. No path
    // component is resolved again at this trust boundary.
    flush_handle(destination, &source_handle)?;
    let destination_identity =
        directory_entry_identity(&destination_parent_handle, destination_leaf)?;
    if destination_identity
        != Some(DirectoryEntryIdentity {
            file_id: source_identity.file_id,
            is_directory: source_is_directory,
            is_reparse_point: false,
        })
    {
        return Err(io::Error::other(format!(
            "durable rename identity mismatch after publishing {}",
            destination.display()
        )));
    }
    let rebound_parent = open_existing(
        destination_parent,
        GENERIC_READ | FILE_READ_ATTRIBUTES,
        true,
    )?;
    ensure_expected_kind(destination_parent, &rebound_parent, true)?;
    if file_identity(destination_parent, &rebound_parent)? != destination_volume {
        return Err(io::Error::other(format!(
            "destination parent changed during durable rename: {}",
            destination_parent.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn sync_directory(directory: &Path) -> Result<()> {
    let handle = open_existing(
        directory,
        GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES,
        true,
    )
    .map_err(|error| io_error(directory, durable_barrier_error(directory, error)))?;
    ensure_expected_kind(directory, &handle, true).map_err(|error| io_error(directory, error))?;
    flush_handle(directory, &handle)
        .map_err(|error| io_error(directory, durable_barrier_error(directory, error)))
}

#[cfg(windows)]
fn durable_barrier_error(directory: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "the filesystem cannot provide a durable directory barrier for {}: {error}",
            directory.display()
        ),
    )
}

#[cfg(windows)]
fn map_unsupported_filesystem_operation(operation: &str, error: io::Error) -> io::Error {
    if matches!(error.raw_os_error(), Some(1 | 50 | 87)) {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("the filesystem does not support durable {operation}: {error}"),
        )
    } else {
        error
    }
}

#[cfg(windows)]
fn open_existing(path: &Path, access: u32, directory: bool) -> io::Result<File> {
    let path = absolute_path(path)?;
    let path_wide = wide_null_terminated(path.as_os_str());
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH;
    if directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    }
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(file_name)) => Ok(fs::canonicalize(parent)?.join(file_name)),
        // A configured storage root may itself be a drive root. It has no leaf
        // name but still needs a real directory durability barrier.
        _ if path.is_absolute() => fs::canonicalize(path),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no parent or file name",
        )),
    }
}

#[cfg(windows)]
fn wide_null_terminated(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn ensure_expected_kind(path: &Path, file: &File, directory: bool) -> io::Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(map_unsupported_filesystem_operation(
            "attribute readback",
            io::Error::last_os_error(),
        ));
    }
    let is_directory = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || is_directory != directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsafe durable-I/O target (reparse point or unexpected type): {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn file_identity(_path: &Path, file: &File) -> io::Result<FileIdentity> {
    let mut info = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(map_unsupported_filesystem_operation(
            "FileId identity readback",
            io::Error::last_os_error(),
        ));
    }
    Ok(FileIdentity {
        volume_serial_number: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

#[cfg(windows)]
fn directory_entry_identity(
    directory: &File,
    leaf: &std::ffi::OsStr,
) -> io::Result<Option<DirectoryEntryIdentity>> {
    let expected_name: Vec<u16> = leaf.encode_wide().collect();
    // FILE_ID_EXTD_DIR_INFO is naturally 8-byte aligned. A u64 buffer keeps
    // every returned entry aligned while still allowing variable-length names.
    let mut storage = vec![0u64; 8 * 1024];
    let mut first = true;
    loop {
        storage.fill(0);
        let class = if first {
            FileIdExtdDirectoryRestartInfo
        } else {
            FileIdExtdDirectoryInfo
        };
        first = false;
        let result = unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle() as HANDLE,
                class,
                storage.as_mut_ptr().cast(),
                (storage.len() * size_of::<u64>()) as u32,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(None);
            }
            return Err(map_unsupported_filesystem_operation(
                "FileId directory readback",
                error,
            ));
        }

        let buffer_start = storage.as_ptr().cast::<u8>();
        let buffer_len = storage.len() * size_of::<u64>();
        let mut offset = 0usize;
        loop {
            if offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) > buffer_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid durable rename directory readback",
                ));
            }
            let entry = unsafe { &*buffer_start.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>() };
            let name_bytes = entry.FileNameLength as usize;
            if !name_bytes.is_multiple_of(size_of::<u16>())
                || offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) + name_bytes > buffer_len
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid durable rename directory entry name",
                ));
            }
            let name = unsafe {
                std::slice::from_raw_parts(entry.FileName.as_ptr(), name_bytes / size_of::<u16>())
            };
            if windows_names_equal(name, &expected_name)? {
                return Ok(Some(DirectoryEntryIdentity {
                    file_id: entry.FileId.Identifier,
                    is_directory: entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    is_reparse_point: entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                }));
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            let next = entry.NextEntryOffset as usize;
            if next == 0
                || offset
                    .checked_add(next)
                    .is_none_or(|value| value >= buffer_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid durable rename directory entry chain",
                ));
            }
            offset += next;
        }
    }
}

#[cfg(windows)]
pub(super) fn windows_names_equal(left: &[u16], right: &[u16]) -> io::Result<bool> {
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    let left_len = i32::try_from(left.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let right_len = i32::try_from(right.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let result = unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left_len,
            right.as_ptr(),
            right_len,
            true.into(),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result == CSTR_EQUAL)
}

#[cfg(windows)]
pub(super) fn list_directory_entries(
    directory: &File,
) -> io::Result<Vec<(std::ffi::OsString, bool, bool)>> {
    use std::os::windows::ffi::OsStringExt;
    let mut entries = Vec::new();
    let mut storage = vec![0u64; 8 * 1024];
    let mut first = true;
    loop {
        storage.fill(0);
        let class = if first {
            FileIdExtdDirectoryRestartInfo
        } else {
            FileIdExtdDirectoryInfo
        };
        first = false;
        let result = unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle() as HANDLE,
                class,
                storage.as_mut_ptr().cast(),
                (storage.len() * size_of::<u64>()) as u32,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(entries);
            }
            return Err(map_unsupported_filesystem_operation(
                "FileId directory enumeration",
                error,
            ));
        }
        let buffer_start = storage.as_ptr().cast::<u8>();
        let buffer_len = storage.len() * size_of::<u64>();
        let mut offset = 0usize;
        loop {
            if offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) > buffer_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid held directory readback",
                ));
            }
            let entry = unsafe { &*buffer_start.add(offset).cast::<FILE_ID_EXTD_DIR_INFO>() };
            let name_bytes = entry.FileNameLength as usize;
            if !name_bytes.is_multiple_of(size_of::<u16>())
                || offset + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) + name_bytes > buffer_len
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid held directory entry name",
                ));
            }
            let name = unsafe {
                std::slice::from_raw_parts(entry.FileName.as_ptr(), name_bytes / size_of::<u16>())
            };
            if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                entries.push((
                    std::ffi::OsString::from_wide(name),
                    entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                ));
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            let next = entry.NextEntryOffset as usize;
            if next == 0
                || offset
                    .checked_add(next)
                    .is_none_or(|value| value >= buffer_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid held directory entry chain",
                ));
            }
            offset += next;
        }
    }
}

#[cfg(windows)]
fn flush_handle(_path: &Path, file: &File) -> io::Result<()> {
    let result = unsafe { FlushFileBuffers(file.as_raw_handle() as HANDLE) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn set_rename_information(
    file: &File,
    destination_parent: &File,
    destination_leaf: &std::ffi::OsStr,
    replace: bool,
) -> io::Result<()> {
    let destination_wide = wide_null_terminated(destination_leaf);
    let file_name_bytes = (destination_wide.len() - 1)
        .checked_mul(size_of::<u16>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination is too long"))?;
    let buffer_size = offset_of!(FILE_RENAME_INFORMATION, FileName)
        .checked_add(destination_wide.len() * size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination is too long"))?;
    let storage_words = buffer_size
        .checked_add(size_of::<u64>() - 1)
        .map(|value| value / size_of::<u64>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination is too long"))?;
    let mut buffer = vec![0u64; storage_words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    unsafe {
        if replace {
            // The destination is deliberately held open while its identity is
            // verified. Extended POSIX rename semantics permit that old
            // handle to remain valid while publishing the new file.
            (*info).Anonymous.Flags = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
        } else {
            (*info).Anonymous.ReplaceIfExists = false;
        }
        (*info).RootDirectory = destination_parent.as_raw_handle() as HANDLE;
        (*info).FileNameLength = file_name_bytes;
        std::ptr::copy_nonoverlapping(
            destination_wide.as_ptr(),
            (*info).FileName.as_mut_ptr(),
            destination_wide.len(),
        );
    }
    let mut io_status = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut io_status,
            buffer.as_ptr().cast(),
            buffer_size as u32,
            if replace {
                FileRenameInformationEx
            } else {
                FileRenameInformation
            },
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(unsafe {
            RtlNtStatusToDosError(status) as i32
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_recovery_matrix_is_explicit_for_every_observable_state() {
        use RenameRecoveryState::*;

        let matrix = [
            ((Some(true), None), RetryFromSource),
            ((None, Some(true)), Committed),
            ((Some(true), Some(true)), AmbiguousDuplicate),
            ((None, None), Missing),
            ((Some(false), None), Conflict),
            ((None, Some(false)), Conflict),
            ((Some(false), Some(true)), Conflict),
            ((Some(true), Some(false)), Conflict),
            ((Some(false), Some(false)), Conflict),
        ];

        for ((source, destination), expected) in matrix {
            assert_eq!(
                classify_rename_recovery_state(source, destination),
                expected
            );
        }
    }
}
