use super::*;

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).map_err(|error| io_error(path, error))?;
    serde_json::from_reader(file).map_err(|error| json_error(path, error))
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| json_error(path, error))?;
    write_bytes_atomic(path, &bytes)
}

pub(crate) fn write_json_atomic_new<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| json_error(path, error))?;
    write_bytes_atomic_new(path, &bytes)
}

pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    write_bytes_atomic(path, text.as_bytes())
}

pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temp_path = short_temporary_path(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| io_error(&temp_path, error))?;
        file.write_all(bytes)
            .map_err(|error| io_error(&temp_path, error))?;
        file.sync_all()
            .map_err(|error| io_error(&temp_path, error))?;
        replace_file(&temp_path, path)?;
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_bytes_atomic_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temp_path = short_temporary_path(path);
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| io_error(&temp_path, error))?;
        file.write_all(bytes)
            .map_err(|error| io_error(&temp_path, error))?;
        file.sync_all()
            .map_err(|error| io_error(&temp_path, error))?;
        move_file_no_replace(&temp_path, path)?;
        sync_parent_dir(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(not(windows))]
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        let file = File::open(parent).map_err(|error| io_error(parent, error))?;
        file.sync_all().map_err(|error| io_error(parent, error))?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn sync_parent_chain(path: &Path, stop_at: &Path) -> Result<()> {
    if !path.starts_with(stop_at) {
        return Err(CheckPoError::Unexpected(format!(
            "cannot sync parent chain outside {}: {}",
            stop_at.display(),
            path.display()
        )));
    }
    let mut current = path.parent();
    while let Some(directory) = current {
        let handle = File::open(directory).map_err(|error| io_error(directory, error))?;
        handle
            .sync_all()
            .map_err(|error| io_error(directory, error))?;
        if directory == stop_at {
            return Ok(());
        }
        current = directory.parent();
    }
    Err(CheckPoError::Unexpected(format!(
        "parent chain did not reach {} from {}",
        stop_at.display(),
        path.display()
    )))
}

#[cfg(windows)]
pub(crate) fn sync_parent_chain(_path: &Path, _stop_at: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn replace_file(temp_path: &Path, destination: &Path) -> Result<()> {
    fs::rename(temp_path, destination).map_err(|error| io_error(destination, error))
}

#[cfg(windows)]
pub(super) fn replace_file(temp_path: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn absolute_wide(path: &Path) -> std::io::Result<Vec<u16>> {
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?;
        let absolute = fs::canonicalize(parent)?.join(file_name);
        Ok(absolute
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect())
    }

    let source = absolute_wide(temp_path).map_err(|error| io_error(temp_path, error))?;
    let destination_wide =
        absolute_wide(destination).map_err(|error| io_error(destination, error))?;
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io_error(destination, std::io::Error::last_os_error()));
    }
    Ok(())
}

pub(crate) fn move_file_no_replace(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    match fs::hard_link(source, destination) {
        Ok(()) => fs::remove_file(source).map_err(|error| io_error(source, error)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(io_error(destination, error))
        }
        Err(_) => copy_file_no_replace(source, destination, CopySourceDisposition::Remove),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopySourceDisposition {
    Keep,
    Remove,
}

pub(crate) fn copy_file_no_replace(
    source: &Path,
    destination: &Path,
    source_disposition: CopySourceDisposition,
) -> Result<()> {
    materialize_file_no_replace(
        source,
        destination,
        source_disposition,
        |source, temp_path| fs::copy(source, temp_path).map(|_| ()),
    )
}

pub(crate) fn reflink_or_copy_file_no_replace(source: &Path, destination: &Path) -> Result<()> {
    materialize_file_no_replace(
        source,
        destination,
        CopySourceDisposition::Keep,
        reflink_or_copy_to_new_file,
    )
}

fn reflink_or_copy_to_new_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    match reflink_copy::reflink(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            match fs::symlink_metadata(destination) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    fs::remove_file(destination)?;
                }
                Ok(_) => {
                    return Err(std::io::Error::other(
                        "reflink left a non-regular destination",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }

            let mut input = File::open(source)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            std::io::copy(&mut input, &mut output)?;
            Ok(())
        }
    }
}

fn materialize_file_no_replace(
    source: &Path,
    destination: &Path,
    source_disposition: CopySourceDisposition,
    materialize: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let mut created_destination = false;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temp_path = short_temporary_path(destination);
    let result = (|| -> Result<()> {
        if destination.exists() {
            return Err(io_error(
                destination,
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "destination already exists",
                ),
            ));
        }
        materialize(source, &temp_path).map_err(|error| io_error(&temp_path, error))?;
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&temp_path)
            .map_err(|error| io_error(&temp_path, error))?;
        output
            .sync_all()
            .map_err(|error| io_error(&temp_path, error))?;
        match fs::hard_link(&temp_path, destination) {
            Ok(()) => {
                created_destination = true;
                fs::remove_file(&temp_path).map_err(|error| io_error(&temp_path, error))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(io_error(destination, error));
            }
            Err(_) => {
                let mut input =
                    File::open(&temp_path).map_err(|error| io_error(&temp_path, error))?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(destination)
                    .map_err(|error| io_error(destination, error))?;
                created_destination = true;
                std::io::copy(&mut input, &mut output)
                    .map_err(|error| io_error(destination, error))?;
                output
                    .sync_all()
                    .map_err(|error| io_error(destination, error))?;
            }
        }
        if source_disposition == CopySourceDisposition::Remove {
            fs::remove_file(source).map_err(|error| io_error(source, error))?;
        }
        Ok(())
    })();
    if result.is_err() && created_destination {
        let _ = fs::remove_file(destination);
    }
    let _ = fs::remove_file(&temp_path);
    result
}

fn short_temporary_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(".checkpo-{}.tmp", Uuid::new_v4().simple()))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn atomic_materialization_uses_short_temp_name_for_long_destination_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let long_name = format!("{}.asset", "a".repeat(220));
        let atomic_destination = temp.path().join(&long_name);

        write_bytes_atomic(&atomic_destination, b"atomic").unwrap();
        assert_eq!(fs::read(&atomic_destination).unwrap(), b"atomic");

        let source = temp.path().join("source");
        let copied_destination = temp.path().join(format!("copy-{long_name}"));
        fs::write(&source, "copied").unwrap();
        copy_file_no_replace(&source, &copied_destination, CopySourceDisposition::Keep).unwrap();
        assert_eq!(fs::read(&copied_destination).unwrap(), b"copied");
    }
}
