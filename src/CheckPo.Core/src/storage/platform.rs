use super::*;

#[cfg(unix)]
pub(crate) fn available_space_bytes(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        CheckPoError::InvalidProject(format!(
            "path contains an interior NUL byte: {}",
            path.display()
        ))
    })?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    let available_blocks = u64::from(stat.f_bavail);
    let block_size = stat.f_frsize;
    Ok(available_blocks.saturating_mul(block_size))
}

#[cfg(windows)]
pub(crate) fn available_space_bytes(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available = 0_u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    Ok(available)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn available_space_bytes(_path: &Path) -> Result<u64> {
    Err(CheckPoError::Unexpected(
        "free space check is not supported on this platform".to_string(),
    ))
}
