use super::*;
use std::time::{Duration, SystemTime};

const MALFORMED_LOCK_GRACE_PERIOD: Duration = Duration::from_secs(60);

pub struct FileLock {
    path: PathBuf,
    token: String,
}

pub type RepositoryLock = FileLock;

pub fn acquire_repository_lock(repo_root: &Path, operation: &str) -> Result<RepositoryLock> {
    let lock_dir = repo_root.join("locks");
    fs::create_dir_all(&lock_dir).map_err(|error| io_error(&lock_dir, error))?;
    let path = lock_dir.join("repository.lock");
    FileLock::acquire(&path, operation)
}

impl FileLock {
    pub(crate) fn acquire(path: &Path, operation: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        let token = Uuid::new_v4().simple().to_string();
        if create_lock_file(path, operation, &token)? {
            return Ok(Self {
                path: path.to_path_buf(),
                token,
            });
        }
        if let Some(stale_text) = stale_lock_text(path)? {
            reclaim_stale_lock(path, &stale_text, operation)?;
            if create_lock_file(path, operation, &token)? {
                return Ok(Self {
                    path: path.to_path_buf(),
                    token,
                });
            }
        }
        Err(CheckPoError::RepositoryLocked(format!(
            "{operation} ({})",
            path.display()
        )))
    }
}

pub(crate) fn create_lock_file(path: &Path, operation: &str, token: &str) -> Result<bool> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(io_error(path, error)),
    };
    let result = (|| -> Result<()> {
        writeln!(
            file,
            "operation={operation}\npid={}\ntoken={token}\ncreatedAtUtc={}",
            std::process::id(),
            now_utc_string()
        )
        .map_err(|error| io_error(path, error))?;
        file.sync_all().map_err(|error| io_error(path, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result.map(|_| true)
}

pub(crate) fn reclaim_stale_lock(path: &Path, expected_text: &str, operation: &str) -> Result<()> {
    let reclaim_path = path.with_file_name(format!(
        "{}.reclaim",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("repository.lock")
    ));
    let _reclaim = ReclaimLock::acquire(&reclaim_path, operation)?;
    match fs::read_to_string(path) {
        Ok(current_text) if current_text == expected_text => {
            let quarantine_path = path.with_file_name(format!(
                "{}.stale-{}",
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("repository.lock"),
                Uuid::new_v4().simple()
            ));
            match fs::rename(path, &quarantine_path) {
                Ok(()) => {
                    let _ = fs::remove_file(&quarantine_path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(path, error)),
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(path, error)),
    }
    Ok(())
}

pub(crate) fn lock_file_has_token(path: &Path, token: &str) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("token=").map(str::to_string))
        })
        .as_deref()
        == Some(token)
}

struct ReclaimLock {
    path: PathBuf,
}

impl ReclaimLock {
    fn acquire(path: &Path, operation: &str) -> Result<Self> {
        let token = Uuid::new_v4().simple().to_string();
        if !create_lock_file(path, operation, &token)? {
            return Err(CheckPoError::RepositoryLocked(format!(
                "{operation}-stale-lock-reclaim ({})",
                path.display()
            )));
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for ReclaimLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stale_lock_text(path: &Path) -> Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(path, error)),
    };
    let Some(pid) = text.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|value| value.trim().parse::<u32>().ok())
    }) else {
        let modified = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| io_error(path, error))?;
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        return Ok((age >= MALFORMED_LOCK_GRACE_PERIOD).then_some(text));
    };
    Ok((!process_is_running(pid)).then_some(text))
}

#[cfg(unix)]
pub(crate) fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub(crate) fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn process_is_running(_pid: u32) -> bool {
    true
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if lock_file_has_token(&self.path, &self.token) {
            let _ = fs::remove_file(&self.path);
        }
        let _ = sync_parent_dir(&self.path);
    }
}
