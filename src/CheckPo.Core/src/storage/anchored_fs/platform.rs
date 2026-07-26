use super::*;

pub(super) fn validated_relative_components(path: &Path) -> Result<Vec<&std::ffi::OsStr>> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CheckPoError::Corruption(format!(
            "anchored path must be a non-empty relative path: {}",
            path.display()
        )));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe anchored path component: {}",
                    path.display()
                )))
            }
        }
    }
    if components.is_empty() {
        return Err(CheckPoError::Corruption(format!(
            "anchored path has no components: {}",
            path.display()
        )));
    }
    Ok(components)
}

#[cfg(unix)]
pub(super) fn open_unix_path(
    parent_fd: libc::c_int,
    path: &Path,
    flags: libc::c_int,
) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let value = std::ffi::CString::new(bytes)
        .map_err(|_| CheckPoError::Corruption(format!("path contains NUL: {}", path.display())))?;
    open_unix_cstring(parent_fd, &value, flags).map_err(|error| io_error(path, error))
}

#[cfg(unix)]
pub(super) fn open_unix_component(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    flags: libc::c_int,
    display_path: &Path,
) -> Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let value = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
        CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
    })?;
    open_unix_cstring(parent_fd, &value, flags).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            CheckPoError::Corruption(format!(
                "anchored path is not a no-follow regular file: {}",
                display_path.display()
            ))
        } else {
            io_error(display_path, error)
        }
    })
}

#[cfg(unix)]
pub(super) fn open_unix_cstring(
    parent_fd: libc::c_int,
    value: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<File> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::openat(parent_fd, value.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
pub(super) fn create_unix_directory_component(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let value = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
        CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
    })?;
    let result = unsafe { libc::mkdirat(parent_fd, value.as_ptr(), 0o777) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return Ok(false);
    }
    Err(io_error(display_path, error))
}

#[cfg(unix)]
pub(super) fn create_unix_directory_component_exclusive(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<()> {
    create_unix_directory_component_exclusive_with_mode(parent_fd, component, display_path, 0o777)
}

#[cfg(unix)]
pub(super) fn create_unix_directory_component_exclusive_with_mode(
    parent_fd: libc::c_int,
    component: &std::ffi::OsStr,
    display_path: &Path,
    mode: libc::mode_t,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let value = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
        CheckPoError::Corruption(format!("path contains NUL: {}", display_path.display()))
    })?;
    let result = unsafe { libc::mkdirat(parent_fd, value.as_ptr(), mode) };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(display_path, std::io::Error::last_os_error()))
    }
}

#[cfg(unix)]
pub(super) fn unix_stat_mtime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_mtime, stat.st_mtime_nsec)
}

#[cfg(unix)]
pub(super) fn unix_stat_ctime(stat: &libc::stat) -> (i64, i64) {
    (stat.st_ctime, stat.st_ctime_nsec)
}

#[cfg(unix)]
pub(super) fn unix_system_time(
    seconds: i64,
    nanoseconds: i64,
    path: &Path,
) -> Result<std::time::SystemTime> {
    if !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(CheckPoError::Corruption(format!(
            "file has an invalid timestamp: {}",
            path.display()
        )));
    }
    let nanoseconds = nanoseconds as u32;
    let value = if seconds >= 0 {
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(seconds as u64, nanoseconds))
    } else if nanoseconds == 0 {
        std::time::UNIX_EPOCH.checked_sub(std::time::Duration::new(seconds.unsigned_abs(), 0))
    } else {
        // POSIX represents -0.5s as tv_sec=-1,tv_nsec=500_000_000.
        std::time::UNIX_EPOCH.checked_sub(std::time::Duration::new(
            seconds.unsigned_abs() - 1,
            1_000_000_000 - nanoseconds,
        ))
    };
    value.ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "file timestamp is out of range: {}",
            path.display()
        ))
    })
}

pub(super) fn validate_leaf(leaf: &std::ffi::OsStr, parent: &Path) -> Result<()> {
    let path = Path::new(leaf);
    if leaf.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(CheckPoError::Corruption(format!(
            "unsafe anchored leaf below {}: {}",
            parent.display(),
            path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn anchored_exchange_files(
    left_parent: &AnchoredParent,
    left_leaf: &std::ffi::OsStr,
    right_parent: &AnchoredParent,
    right_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let left = std::ffi::CString::new(left_leaf.as_bytes())?;
    let right = std::ffi::CString::new(right_leaf.as_bytes())?;
    let result = unsafe {
        libc::renameatx_np(
            left_parent.directory.as_raw_fd(),
            left.as_ptr(),
            right_parent.directory.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn anchored_exchange_files(
    left_parent: &AnchoredParent,
    left_leaf: &std::ffi::OsStr,
    right_parent: &AnchoredParent,
    right_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let left = std::ffi::CString::new(left_leaf.as_bytes())?;
    let right = std::ffi::CString::new(right_leaf.as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_parent.directory.as_raw_fd(),
            left.as_ptr(),
            right_parent.directory.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
pub(super) fn anchored_rename_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredFile,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::renameatx_np(
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn anchored_rename_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredFile,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(super) fn anchored_rename_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    expected_source: &AnchoredFile,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    let source = open_windows_relative_file_for_mutation(&source_parent.directory, source_leaf)
        .map_err(checkpo_error_into_io)?;
    let display_path = source_parent.display_path.join(source_leaf);
    if FileIdentity::from_file(&display_path, &source).map_err(checkpo_error_into_io)?
        != expected_source.identity
    {
        return Err(std::io::Error::other("rename source identity changed"));
    }
    super::super::windows_durability::rename_open_handle_no_replace(
        &source,
        &destination_parent.directory,
        destination_leaf,
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(super) fn anchored_rename_no_replace(
    _source_parent: &AnchoredParent,
    _source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredFile,
    _destination_parent: &AnchoredParent,
    _destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive handle-relative rename is unavailable on this platform",
    ))
}

#[cfg(target_os = "macos")]
pub(super) fn anchored_rename_directory_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredParent,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::renameatx_np(
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn anchored_rename_directory_no_replace(
    source_parent: &AnchoredParent,
    source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredParent,
    destination_parent: &AnchoredParent,
    destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source_leaf.as_bytes())?;
    let destination = std::ffi::CString::new(destination_leaf.as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.directory.as_raw_fd(),
            source.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(super) fn anchored_rename_directory_no_replace(
    _source_parent: &AnchoredParent,
    _source_leaf: &std::ffi::OsStr,
    _expected_source: &AnchoredParent,
    _destination_parent: &AnchoredParent,
    _destination_leaf: &std::ffi::OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive handle-relative directory rename is unavailable on this platform",
    ))
}

#[cfg(unix)]
pub(super) fn anchored_unlink(
    parent: &AnchoredParent,
    leaf: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let leaf = std::ffi::CString::new(leaf.as_bytes())?;
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    let result = unsafe { libc::unlinkat(parent.directory.as_raw_fd(), leaf.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) fn anchored_unlink(
    parent: &AnchoredParent,
    leaf: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<()> {
    let path = parent.display_path.join(leaf);
    if directory {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(windows)]
pub(super) fn open_windows_directory_no_follow(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| io_error(path, error))
}

#[cfg(windows)]
pub(super) fn windows_replace_record_leaf(
    destination_leaf: &std::ffi::OsStr,
) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStrExt;

    let mut hasher = blake3::Hasher::new();
    for unit in destination_leaf.encode_wide() {
        hasher.update(&unit.to_le_bytes());
    }
    std::ffi::OsString::from(format!(
        ".checkpo-replace-{}.json",
        hasher.finalize().to_hex()
    ))
}

#[cfg(windows)]
pub(super) fn open_windows_relative_directory(
    parent: &File,
    component: &std::ffi::OsStr,
    create_new: bool,
    writable: bool,
) -> Result<File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    let desired_access = GENERIC_READ
        | FILE_READ_ATTRIBUTES
        | SYNCHRONIZE
        | if create_new || writable {
            GENERIC_WRITE
        } else {
            0
        };
    open_windows_relative(
        parent,
        component,
        desired_access,
        true,
        create_new,
        create_new || writable,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(component), error))
}

#[cfg(windows)]
pub(super) fn reopen_windows_directory_for_mutation(
    directory: &File,
    display_path: &Path,
) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    // ReOpenFile is not a reliable way to upgrade a directory handle on
    // Windows. Reacquire a write-through twin by path, but bind it to the
    // already-held read anchor before allowing any mutation. Intermediate
    // path replacement can therefore only produce an identity mismatch, not
    // redirect a later handle-relative operation.
    let reopened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
        )
        .open(display_path)
        .map_err(|error| io_error(display_path, error))?;
    let metadata = reopened
        .metadata()
        .map_err(|error| io_error(display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "mutation anchor is not a regular directory: {}",
            display_path.display()
        )));
    }
    if FileIdentity::from_file(display_path, directory)?
        != FileIdentity::from_file(display_path, &reopened)?
    {
        return Err(CheckPoError::WorkingTreeChanged(
            display_path.display().to_string(),
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
pub(super) fn reopen_windows_directory_for_removal(
    directory: &File,
    display_path: &Path,
) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    };

    let reopened = fs::OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
        )
        .open(display_path)
        .map_err(|error| io_error(display_path, error))?;
    let metadata = reopened
        .metadata()
        .map_err(|error| io_error(display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "removal anchor is not a regular directory: {}",
            display_path.display()
        )));
    }
    if FileIdentity::from_file(display_path, directory)?
        != FileIdentity::from_file(display_path, &reopened)?
    {
        return Err(CheckPoError::WorkingTreeChanged(
            display_path.display().to_string(),
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
pub(super) fn reopen_windows_file_for_durability(file: &File, display_path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let reopened = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH)
        .open(display_path)
        .map_err(|error| io_error(display_path, error))?;
    let metadata = reopened
        .metadata()
        .map_err(|error| io_error(display_path, error))?;
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(format!(
            "durability target is not a regular file: {}",
            display_path.display()
        )));
    }
    if FileIdentity::from_file(display_path, file)?
        != FileIdentity::from_file(display_path, &reopened)?
    {
        return Err(CheckPoError::WorkingTreeChanged(
            display_path.display().to_string(),
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
pub(super) fn open_windows_relative_file(
    parent: &File,
    leaf: &std::ffi::OsStr,
    read_write: bool,
    create_new: bool,
) -> Result<File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    let desired_access = GENERIC_READ
        | FILE_READ_ATTRIBUTES
        | SYNCHRONIZE
        | if read_write { GENERIC_WRITE } else { 0 };
    open_windows_relative(
        parent,
        leaf,
        desired_access,
        false,
        create_new,
        create_new || read_write,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
pub(super) fn open_windows_relative_file_for_mutation(
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<File> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        true,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
pub(super) fn open_windows_relative_file_for_removal(
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        false,
        false,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
pub(super) fn open_windows_relative_file_for_unversioned_removal(
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        false,
        true,
        true,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
pub(super) fn open_windows_relative_file_for_finalization(
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, SYNCHRONIZE};
    open_windows_relative(
        parent,
        leaf,
        GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        false,
        false,
        false,
        true,
        false,
    )
    .map_err(|error| io_error(Path::new(leaf), error))
}

#[cfg(windows)]
pub(super) fn checkpo_error_into_io(error: CheckPoError) -> std::io::Error {
    match error {
        CheckPoError::Io { source, .. } => source,
        error => std::io::Error::other(error.to_string()),
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub(super) fn open_windows_relative(
    parent: &File,
    component: &std::ffi::OsStr,
    desired_access: u32,
    directory: bool,
    create_new: bool,
    write_through: bool,
    share_write: bool,
    share_delete: bool,
) -> std::io::Result<File> {
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH,
    };
    use windows_sys::Win32::Foundation::{
        RtlNtStatusToDosError, HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut wide = component.encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Windows relative component",
        ));
    }
    let byte_len = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows relative component is too long",
            )
        })?;
    let unicode = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = INVALID_HANDLE_VALUE;
    let mut io_status = MaybeUninit::<IO_STATUS_BLOCK>::zeroed();
    let create_options = FILE_OPEN_REPARSE_POINT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | if write_through { FILE_WRITE_THROUGH } else { 0 }
        | if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            io_status.as_mut_ptr(),
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ
                | if share_write { FILE_SHARE_WRITE } else { 0 }
                | if share_delete { FILE_SHARE_DELETE } else { 0 },
            if create_new { FILE_CREATE } else { FILE_OPEN },
            create_options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(unsafe {
            RtlNtStatusToDosError(status) as i32
        }));
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_portable_directory_no_follow(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_new_portable_file_no_follow(path: &Path) -> Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_existing_portable_file_no_follow(path: &Path) -> Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_read_only_portable_file_no_follow(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| io_error(path, error))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_portable_file_no_follow(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| io_error(path, error))
}
