use super::*;
use serde_json::Value;
use std::time::{Duration, SystemTime};

const UNITY_LOCK_FRESHNESS: Duration = Duration::from_secs(5);
const TARGET_STABLE_PASS_COUNT: usize = 3;
const TARGET_STABLE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_EDITOR_INSTANCE_BYTES: u64 = 1024 * 1024;

pub(super) fn ensure_unity_editor_is_closed(project: &ProjectContext) -> Result<()> {
    let project_root = project.project_root.as_path();
    let editor_instance = project_root.join("Library/EditorInstance.json");
    if let Some(pid) = read_unity_editor_pid(&editor_instance)? {
        if process_is_alive(pid)? {
            return Err(crate::user_error(format!(
                "Unity Editor is still using this project (pid {pid}). Close Unity before applying or recovering an authoritative restore."
            )));
        }
    }

    let lockfile = project_root.join("Temp/UnityLockfile");
    if recent_regular_file(&lockfile, UNITY_LOCK_FRESHNESS)? {
        return Err(crate::user_error(
            "Unity Editor appears to still be using this project. Close Unity and retry after the project lock settles.",
        ));
    }
    Ok(())
}

pub(super) fn verify_target_is_stable(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
    kind: OperationPlanKind,
    selected_paths: Option<&[TrackedUnityFilePath]>,
) -> Result<()> {
    ensure_unity_editor_is_closed(project)?;
    for pass in 0..TARGET_STABLE_PASS_COUNT {
        verify_target_once(project, checkpoint_id, kind, selected_paths)?;
        if pass + 1 < TARGET_STABLE_PASS_COUNT {
            std::thread::sleep(TARGET_STABLE_INTERVAL);
        }
    }
    Ok(())
}

pub(super) fn verify_target_once(
    project: &ProjectContext,
    checkpoint_id: &SnapshotId,
    kind: OperationPlanKind,
    selected_paths: Option<&[TrackedUnityFilePath]>,
) -> Result<()> {
    ensure_unity_editor_is_closed(project)?;
    let remaining = super::plan::build_plan(project, checkpoint_id.clone(), kind, selected_paths)?;
    if !remaining.warnings.is_empty() || remaining.has_changes {
        return Err(CheckPoError::WorkingTreeChanged(
            "Unity project changed while the checkpoint was being applied".to_string(),
        ));
    }
    Ok(())
}

fn read_unity_editor_pid(path: &Path) -> Result<Option<u32>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::io_error(path, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(format!(
            "Unity editor instance metadata is unsafe: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_EDITOR_INSTANCE_BYTES {
        return Err(CheckPoError::Corruption(format!(
            "Unity editor instance metadata is unexpectedly large: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| crate::io_error(path, error))?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let pid = value
        .get("process_id")
        .or_else(|| value.get("processId"))
        .or_else(|| value.get("pid"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    Ok(pid.filter(|pid| *pid != 0))
}

fn recent_regular_file(path: &Path, freshness: Duration) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(crate::io_error(path, error)),
    };
    if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(format!(
            "Unity project lock is unsafe: {}",
            path.display()
        )));
    }
    let modified = metadata
        .modified()
        .map_err(|error| crate::io_error(path, error))?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    Ok(age <= freshness)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> Result<bool> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| CheckPoError::Corruption("Unity process id is out of range".to_string()))?;
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(crate::io_error("Unity process", error)),
    }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> Result<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(5) => Ok(true),
            Some(87) => Ok(false),
            _ => Err(crate::io_error("Unity process", error)),
        };
    }
    let mut exit_code = 0_u32;
    let succeeded = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    unsafe { CloseHandle(handle) };
    if succeeded == 0 {
        return Err(crate::io_error(
            "Unity process",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(exit_code == 259)
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> Result<bool> {
    Ok(false)
}
