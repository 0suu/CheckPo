use super::*;

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).map_err(|error| io_error(path, error))?;
    serde_json::from_reader(file).map_err(|error| json_error(path, error))
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| json_error(path, error))?;
    write_bytes_atomic(path, &bytes)
}

pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    write_bytes_atomic(path, text.as_bytes())
}

pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temp_path = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("checkpo"),
        Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut file = File::create(&temp_path).map_err(|error| io_error(&temp_path, error))?;
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

pub(super) fn replace_file(temp_path: &Path, destination: &Path) -> Result<()> {
    fs::rename(temp_path, destination).map_err(|error| io_error(destination, error))
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
    let mut created_destination = false;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let temp_path = destination.with_file_name(format!(
        ".{}.{}.tmp",
        destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("copy"),
        Uuid::new_v4().simple()
    ));
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
        fs::copy(source, &temp_path).map_err(|error| io_error(&temp_path, error))?;
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
