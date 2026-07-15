use super::*;

#[derive(Debug)]
pub struct FileLock {
    _file: File,
    _parent: File,
}

pub struct RepositoryLock {
    _file_lock: FileLock,
    _repo_root: AnchoredRoot,
    _locks_directory: AnchoredParent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockMode {
    Shared,
    Exclusive,
}

pub fn acquire_repository_lock(repo_root: &Path, operation: &str) -> Result<RepositoryLock> {
    acquire_repository_lock_mode(repo_root, operation, LockMode::Exclusive)
}

pub fn acquire_repository_shared_lock(repo_root: &Path, operation: &str) -> Result<RepositoryLock> {
    acquire_repository_lock_mode(repo_root, operation, LockMode::Shared)
}

fn acquire_repository_lock_mode(
    repo_root: &Path,
    operation: &str,
    mode: LockMode,
) -> Result<RepositoryLock> {
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let locks_relative = Path::new("locks");
    let locks_directory = anchored_repo.open_directory(locks_relative, true)?;
    anchored_repo.verify_parent_binding(locks_relative, &locks_directory)?;
    let lock_path = repo_root.join("locks/repository.lock");
    let file_lock = match mode {
        LockMode::Exclusive => FileLock::acquire(&lock_path, operation)?,
        LockMode::Shared => FileLock::acquire_shared(&lock_path, operation)?,
    };
    anchored_repo.verify_parent_binding(locks_relative, &locks_directory)?;
    anchored_repo.verify_root_binding()?;
    Ok(RepositoryLock {
        _file_lock: file_lock,
        _repo_root: anchored_repo,
        _locks_directory: locks_directory,
    })
}

impl FileLock {
    pub(crate) fn acquire(path: &Path, operation: &str) -> Result<Self> {
        Self::acquire_mode(path, operation, LockMode::Exclusive)
    }

    pub(crate) fn acquire_shared(path: &Path, operation: &str) -> Result<Self> {
        Self::acquire_mode(path, operation, LockMode::Shared)
    }

    fn acquire_mode(path: &Path, operation: &str, mode: LockMode) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| CheckPoError::Corruption("lock path has no parent directory".into()))?;
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        let (file, parent) = match open_lock_file(path, mode)? {
            Some(opened) => opened,
            None => {
                crate::diagnostics::log_warning(
                    "repository-lock",
                    &format!("{operation} could not acquire {}", path.display()),
                );
                return Err(CheckPoError::RepositoryLocked(format!(
                    "{operation} ({})",
                    path.display()
                )));
            }
        };
        Ok(Self {
            _file: file,
            _parent: parent,
        })
    }
}

fn unsafe_lock_path(path: &Path, reason: &str) -> CheckPoError {
    CheckPoError::Corruption(format!("unsafe lock path {}: {reason}", path.display()))
}

#[cfg(unix)]
fn open_lock_file(path: &Path, mode: LockMode) -> Result<Option<(File, File)>> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let parent_path = path
        .parent()
        .ok_or_else(|| unsafe_lock_path(path, "missing parent directory"))?;
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent_path)
        .map_err(|error| io_error(parent_path, error))?;
    let parent_metadata = parent
        .metadata()
        .map_err(|error| io_error(parent_path, error))?;
    if !parent_metadata.is_dir() {
        return Err(unsafe_lock_path(parent_path, "parent is not a directory"));
    }

    let file_name = path
        .file_name()
        .ok_or_else(|| unsafe_lock_path(path, "missing file name"))?;
    let file_name = CString::new(file_name.as_bytes())
        .map_err(|_| unsafe_lock_path(path, "file name contains NUL"))?;
    let raw_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if raw_fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Err(unsafe_lock_path(path, "lock file is a symbolic link"));
        }
        return Err(io_error(path, error));
    }
    let file = unsafe { File::from_raw_fd(raw_fd) };
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if !metadata.is_file() {
        return Err(unsafe_lock_path(path, "lock file is not a regular file"));
    }

    let operation = match mode {
        LockMode::Shared => libc::LOCK_SH,
        LockMode::Exclusive => libc::LOCK_EX,
    };
    let result = unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some((file, parent)));
    }
    let error = std::io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        return Ok(None);
    }
    Err(io_error(path, error))
}

#[cfg(windows)]
fn open_lock_file(path: &Path, mode: LockMode) -> Result<Option<(File, File)>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let parent_path = path
        .parent()
        .ok_or_else(|| unsafe_lock_path(path, "missing parent directory"))?;
    let parent = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent_path)
        .map_err(|error| io_error(parent_path, error))?;
    let parent_metadata = parent
        .metadata()
        .map_err(|error| io_error(parent_path, error))?;
    if !parent_metadata.is_dir()
        || parent_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(unsafe_lock_path(
            parent_path,
            "parent is a reparse point or is not a directory",
        ));
    }

    let share_mode = match mode {
        LockMode::Shared => FILE_SHARE_READ | FILE_SHARE_WRITE,
        LockMode::Exclusive => 0,
    };
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => {
            let metadata = file.metadata().map_err(|error| io_error(path, error))?;
            if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(unsafe_lock_path(
                    path,
                    "lock file is a reparse point or is not a regular file",
                ));
            }
            Ok(Some((file, parent)))
        }
        Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => Ok(None),
        Err(error) => Err(io_error(path, error)),
    }
}

#[cfg(not(any(unix, windows)))]
fn open_lock_file(path: &Path, _mode: LockMode) -> Result<Option<(File, File)>> {
    Err(io_error(
        path,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "OS advisory file locks are not supported on this platform",
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_lock_rejects_concurrent_acquisition_and_releases_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repository.lock");
        let first = FileLock::acquire(&path, "first").unwrap();

        let error = FileLock::acquire(&path, "second").unwrap_err();
        assert!(matches!(error, CheckPoError::RepositoryLocked(_)));

        drop(first);
        FileLock::acquire(&path, "third").unwrap();
    }

    #[test]
    fn shared_locks_coexist_and_block_exclusive_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repository.lock");
        let first = FileLock::acquire_shared(&path, "first-reader").unwrap();
        let second = FileLock::acquire_shared(&path, "second-reader").unwrap();

        let error = FileLock::acquire(&path, "writer").unwrap_err();
        assert!(matches!(error, CheckPoError::RepositoryLocked(_)));

        drop(first);
        drop(second);
        FileLock::acquire(&path, "writer").unwrap();
    }

    #[test]
    fn exclusive_lock_blocks_shared_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repository.lock");
        let writer = FileLock::acquire(&path, "writer").unwrap();

        let error = FileLock::acquire_shared(&path, "reader").unwrap_err();
        assert!(matches!(error, CheckPoError::RepositoryLocked(_)));

        drop(writer);
        FileLock::acquire_shared(&path, "reader").unwrap();
    }

    #[test]
    fn stale_metadata_is_never_modified() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repository.lock");
        let original = "operation=stale\npid=1\n";
        fs::write(&path, original).unwrap();

        let lock = FileLock::acquire(&path, "current").unwrap();
        drop(lock);

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn hard_linked_lock_file_is_never_modified() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("important.txt");
        let path = temp.path().join("repository.lock");
        fs::write(&target, "important content").unwrap();
        fs::hard_link(&target, &path).unwrap();

        let lock = FileLock::acquire(&path, "current").unwrap();
        drop(lock);

        assert_eq!(fs::read_to_string(&target).unwrap(), "important content");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_lock_file_is_rejected_without_touching_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("important.txt");
        let path = temp.path().join("repository.lock");
        fs::write(&target, "important content").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let error = FileLock::acquire(&path, "current").unwrap_err();

        assert!(matches!(error, CheckPoError::Corruption(_)));
        assert_eq!(fs::read_to_string(&target).unwrap(), "important content");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_parent_is_rejected_without_creating_lock() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let linked_parent = temp.path().join("locks");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &linked_parent).unwrap();

        let error =
            FileLock::acquire(&linked_parent.join("repository.lock"), "current").unwrap_err();

        assert!(matches!(error, CheckPoError::Io { .. }));
        assert!(!target.join("repository.lock").exists());
    }

    #[cfg(windows)]
    #[test]
    fn reparse_point_lock_file_is_rejected_without_touching_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("important.txt");
        let path = temp.path().join("repository.lock");
        fs::write(&target, "important content").unwrap();
        if std::os::windows::fs::symlink_file(&target, &path).is_err() {
            return;
        }

        assert!(FileLock::acquire(&path, "current").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "important content");
    }

    #[cfg(windows)]
    #[test]
    fn reparse_point_parent_is_rejected_without_creating_lock() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let linked_parent = temp.path().join("locks");
        fs::create_dir(&target).unwrap();
        if std::os::windows::fs::symlink_dir(&target, &linked_parent).is_err() {
            return;
        }

        assert!(FileLock::acquire(&linked_parent.join("repository.lock"), "current").is_err());
        assert!(!target.join("repository.lock").exists());
    }
}
