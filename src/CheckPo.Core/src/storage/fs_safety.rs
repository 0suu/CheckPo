use super::*;

pub(crate) fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(crate) fn ensure_regular_directory_no_follow(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(CheckPoError::Corruption(format!(
            "unsafe directory path: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn ensure_regular_file_no_follow(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(CheckPoError::Corruption(format!(
            "unsafe regular file path: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

pub(crate) fn create_dir_all_no_follow(base: &Path, target: &Path) -> Result<()> {
    let relative = target.strip_prefix(base).map_err(|_| {
        CheckPoError::Unexpected(format!(
            "directory is outside trusted base {}: {}",
            base.display(),
            target.display()
        ))
    })?;
    ensure_regular_directory_no_follow(base)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(CheckPoError::Corruption(format!(
                "unsafe directory component in {}",
                target.display()
            )));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe directory path: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => sync_parent_dir(&current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        ensure_regular_directory_no_follow(&current)?;
                    }
                    Err(error) => return Err(io_error(&current, error)),
                }
            }
            Err(error) => return Err(io_error(&current, error)),
        }
    }
    Ok(())
}

pub(crate) fn create_absolute_dir_all_no_follow(target: &Path) -> Result<PathBuf> {
    if !target.is_absolute() {
        return Err(CheckPoError::Corruption(format!(
            "directory path is not absolute: {}",
            target.display()
        )));
    }
    let mut current = PathBuf::new();
    for component in target.components() {
        current.push(component.as_os_str());
        match component {
            Component::Prefix(_) => continue,
            Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe directory component in {}",
                    target.display()
                )))
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe directory path: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => sync_parent_dir(&current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        ensure_regular_directory_no_follow(&current)?;
                    }
                    Err(error) => return Err(io_error(&current, error)),
                }
            }
            Err(error) => return Err(io_error(&current, error)),
        }
    }
    ensure_regular_directory_no_follow(target)?;
    #[cfg(windows)]
    {
        Ok(target.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        target
            .canonicalize()
            .map_err(|error| io_error(target, error))
    }
}

pub(crate) fn validate_repository_layout_no_follow(repo_root: &Path) -> Result<()> {
    ensure_regular_directory_no_follow(repo_root)?;
    for relative in [
        "refs",
        "snapshots",
        "objects",
        "objects/loose",
        "indexes",
        "journals",
        "tmp",
        "locks",
    ] {
        ensure_regular_directory_no_follow(&repo_root.join(relative))?;
    }
    let quarantine = repo_root.join("quarantined-journals");
    match fs::symlink_metadata(&quarantine) {
        Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(CheckPoError::Corruption(format!(
                "unsafe transaction quarantine directory: {}",
                quarantine.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(&quarantine, error)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn directory_creation_does_not_follow_symbolic_link() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        let outside = temp.path().join("outside");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("linked")).unwrap();

        assert!(create_dir_all_no_follow(&base, &base.join("linked/created")).is_err());
        assert!(!outside.join("created").exists());
    }

    #[cfg(windows)]
    #[test]
    fn directory_creation_does_not_follow_reparse_point() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        let outside = temp.path().join("outside");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&outside).unwrap();
        if std::os::windows::fs::symlink_dir(&outside, base.join("linked")).is_err() {
            return;
        }

        assert!(create_dir_all_no_follow(&base, &base.join("linked/created")).is_err());
        assert!(!outside.join("created").exists());
    }

    #[cfg(unix)]
    #[test]
    fn absolute_directory_creation_rejects_symbolic_link_parent() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        let linked = temp.path().join("linked");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &linked).unwrap();

        assert!(create_absolute_dir_all_no_follow(&linked.join("created")).is_err());
        assert!(!outside.join("created").exists());
    }
}
