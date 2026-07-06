use super::*;

pub fn put_object_from_file_with_known_hash(
    repo_root: &Path,
    source: &Path,
    object_id: &ObjectId,
    size_bytes: u64,
) -> Result<bool> {
    let tmp_dir = repo_root.join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(|error| io_error(&tmp_dir, error))?;
    let temp_path = tmp_dir.join(format!("object-{}.tmp", Uuid::new_v4().simple()));
    let mut input = File::open(source).map_err(|error| io_error(source, error))?;
    let mut output = File::create(&temp_path).map_err(|error| io_error(&temp_path, error))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied_size_bytes = 0_u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| io_error(source, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| io_error(&temp_path, error))?;
        copied_size_bytes += read as u64;
    }
    output
        .sync_all()
        .map_err(|error| io_error(&temp_path, error))?;
    drop(output);
    if copied_size_bytes != size_bytes {
        fs::remove_file(&temp_path).map_err(|error| io_error(&temp_path, error))?;
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            source.display(),
            size_bytes,
            copied_size_bytes
        )));
    }
    let actual = ObjectId::parse(hasher.finalize().to_hex().as_ref())?;
    if &actual != object_id {
        let _ = fs::remove_file(&temp_path);
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            source.display(),
            object_id,
            actual
        )));
    }
    let dest = object_path(repo_root, object_id);
    if dest.exists() {
        return replace_corrupt_existing_object(&temp_path, &dest, object_id, size_bytes);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    if let Err(error) = move_file_no_replace(&temp_path, &dest) {
        if dest.exists() {
            return replace_corrupt_existing_object(&temp_path, &dest, object_id, size_bytes);
        }
        return Err(error);
    }
    sync_parent_dir(&dest)?;
    Ok(true)
}

fn replace_corrupt_existing_object(
    temp_path: &Path,
    destination: &Path,
    object_id: &ObjectId,
    size_bytes: u64,
) -> Result<bool> {
    match verify_file_hash_and_size(destination, object_id, size_bytes) {
        Ok(()) => {
            fs::remove_file(temp_path).map_err(|error| io_error(temp_path, error))?;
            Ok(false)
        }
        Err(_) => {
            replace_file(temp_path, destination)?;
            sync_parent_dir(destination)?;
            verify_file_hash_and_size(destination, object_id, size_bytes)?;
            Ok(true)
        }
    }
}

pub(crate) fn copy_object_to_file(
    repo_root: &Path,
    object_id: &ObjectId,
    destination: &Path,
    size_bytes: u64,
) -> Result<()> {
    if let Err(error) = copy_object_to_file_verified(repo_root, object_id, destination, size_bytes)
    {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn copy_object_to_file_verified(
    repo_root: &Path,
    object_id: &ObjectId,
    destination: &Path,
    size_bytes: u64,
) -> Result<()> {
    let source = object_path(repo_root, object_id);
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CheckPoError::ObjectMissing(object_id.to_string()));
        }
        Err(error) => return Err(io_error(&source, error)),
    };
    if !metadata.file_type().is_file() {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular object file",
            source.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let mut input = File::open(&source).map_err(|error| io_error(&source, error))?;
    let mut output = File::create(destination).map_err(|error| io_error(destination, error))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied_size_bytes = 0_u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| io_error(&source, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| io_error(destination, error))?;
        copied_size_bytes += read as u64;
    }
    output
        .sync_all()
        .map_err(|error| io_error(destination, error))?;
    drop(output);
    if copied_size_bytes != size_bytes {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            source.display(),
            size_bytes,
            copied_size_bytes
        )));
    }
    let actual = ObjectId::parse(hasher.finalize().to_hex().as_ref())?;
    if &actual != object_id {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            source.display(),
            object_id,
            actual
        )));
    }
    Ok(())
}

pub fn verify_file_hash_and_size(path: &Path, expected: &ObjectId, size_bytes: u64) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if !metadata.file_type().is_file() {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() != size_bytes {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} size expected {}, got {}",
            path.display(),
            size_bytes,
            metadata.len()
        )));
    }
    let actual = hash_file(path)?;
    if &actual != expected {
        return Err(CheckPoError::ObjectHashMismatch(format!(
            "{} expected {}, got {}",
            path.display(),
            expected,
            actual
        )));
    }
    Ok(())
}

pub fn hash_file(path: &Path) -> Result<ObjectId> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let hash = hasher.finalize();
    ObjectId::parse(hash.to_hex().as_ref())
}
