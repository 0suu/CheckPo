// DirectorySyncBatch remains as coverage for the legacy path-based atomic
// move helpers. Production repository publication uses held parent handles.
#![cfg_attr(not(test), allow(dead_code))]

use super::*;
use std::collections::BTreeSet;

/// Collects directory-entry mutations that must become durable before a
/// checkpoint root is published. Files are individually synced before they are
/// added to this batch; flushing it is therefore the publish barrier for those
/// already-synced files.
pub(crate) struct DirectorySyncBatch {
    trusted_root: PathBuf,
    directories: BTreeSet<PathBuf>,
}

impl DirectorySyncBatch {
    pub(crate) fn new(trusted_root: &Path) -> Result<Self> {
        ensure_regular_directory_no_follow(trusted_root)?;
        Ok(Self {
            trusted_root: trusted_root.to_path_buf(),
            directories: BTreeSet::new(),
        })
    }

    pub(crate) fn record_parent(&mut self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            CheckPoError::Unexpected(format!("path has no parent directory: {}", path.display()))
        })?;
        self.record_directory(parent)
    }

    pub(crate) fn record_directory(&mut self, directory: &Path) -> Result<()> {
        if !directory.starts_with(&self.trusted_root) {
            return Err(CheckPoError::Unexpected(format!(
                "directory sync is outside trusted root {}: {}",
                self.trusted_root.display(),
                directory.display()
            )));
        }
        self.directories.insert(directory.to_path_buf());
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn merge(&mut self, other: Self) -> Result<()> {
        if self.trusted_root != other.trusted_root {
            return Err(CheckPoError::Unexpected(format!(
                "cannot merge directory sync batches for different roots: {} and {}",
                self.trusted_root.display(),
                other.trusted_root.display()
            )));
        }
        self.directories.extend(other.directories);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn flush(
        &mut self,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    ) -> Result<()> {
        self.flush_with_progress(recorder, |_, _| Ok(()))
    }

    pub(crate) fn flush_with_progress(
        &mut self,
        recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
        mut progress: impl FnMut(usize, usize) -> Result<()>,
    ) -> Result<()> {
        // Children must be made durable before the directory entry that makes
        // the child reachable. Sorting deepest-first preserves that ordering.
        let mut directories = std::mem::take(&mut self.directories)
            .into_iter()
            .collect::<Vec<_>>();
        directories.sort_by(|left, right| {
            right
                .components()
                .count()
                .cmp(&left.components().count())
                .then_with(|| left.cmp(right))
        });
        let total = directories.len();
        for (index, directory) in directories.into_iter().enumerate() {
            ensure_regular_directory_no_follow(&directory)?;
            match recorder {
                Some(recorder) => recorder.measure(
                    crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
                    || sync_directory(&directory),
                )?,
                None => sync_directory(&directory)?,
            }
            if let Some(recorder) = recorder {
                recorder.directory_fsync();
            }
            progress(index + 1, total)?;
        }
        Ok(())
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.directories.len()
    }
}

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
    create_dir_all_no_follow_profiled(base, target, None)
}

pub(crate) fn create_dir_all_no_follow_profiled(
    base: &Path,
    target: &Path,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<()> {
    let relative = target.strip_prefix(base).map_err(|_| {
        CheckPoError::Unexpected(format!(
            "directory is outside trusted base {}: {}",
            base.display(),
            target.display()
        ))
    })?;
    measure_directory_prepare(recorder, || ensure_regular_directory_no_follow(base))?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(CheckPoError::Corruption(format!(
                "unsafe directory component in {}",
                target.display()
            )));
        };
        current.push(component);
        match measure_directory_prepare(recorder, || fs::symlink_metadata(&current)) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe directory path: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match measure_directory_prepare(recorder, || fs::create_dir(&current)) {
                    Ok(()) => {
                        if let Some(recorder) = recorder {
                            recorder.directory_created();
                        }
                        match recorder {
                            Some(recorder) => recorder.measure(
                                crate::checkpoint_metrics::IoTimingKind::DirectoryFsync,
                                || sync_parent_dir(&current),
                            )?,
                            None => sync_parent_dir(&current)?,
                        }
                        if let Some(recorder) = recorder {
                            recorder.directory_fsync();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        measure_directory_prepare(recorder, || {
                            ensure_regular_directory_no_follow(&current)
                        })?;
                    }
                    Err(error) => return Err(io_error(&current, error)),
                }
            }
            Err(error) => return Err(io_error(&current, error)),
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn create_dir_all_no_follow_batched(
    base: &Path,
    target: &Path,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: &mut DirectorySyncBatch,
) -> Result<()> {
    let relative = target.strip_prefix(base).map_err(|_| {
        CheckPoError::Unexpected(format!(
            "directory is outside trusted base {}: {}",
            base.display(),
            target.display()
        ))
    })?;
    measure_directory_prepare(recorder, || ensure_regular_directory_no_follow(base))?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(CheckPoError::Corruption(format!(
                "unsafe directory component in {}",
                target.display()
            )));
        };
        current.push(component);
        match measure_directory_prepare(recorder, || fs::symlink_metadata(&current)) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe directory path: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match measure_directory_prepare(recorder, || fs::create_dir(&current)) {
                    Ok(()) => {
                        if let Some(recorder) = recorder {
                            recorder.directory_created();
                        }
                        sync_batch.record_parent(&current)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        measure_directory_prepare(recorder, || {
                            ensure_regular_directory_no_follow(&current)
                        })?;
                    }
                    Err(error) => return Err(io_error(&current, error)),
                }
            }
            Err(error) => return Err(io_error(&current, error)),
        }
    }
    Ok(())
}

fn measure_directory_prepare<T>(
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    operation: impl FnOnce() -> T,
) -> T {
    match recorder {
        Some(recorder) => recorder.measure(
            crate::checkpoint_metrics::IoTimingKind::DirectoryPrepare,
            operation,
        ),
        None => operation(),
    }
}

#[cfg(windows)]
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
    Ok(target.to_path_buf())
}

#[cfg(not(windows))]
pub(crate) fn create_absolute_dir_all_no_follow(target: &Path) -> Result<PathBuf> {
    if !target.is_absolute()
        || target.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(CheckPoError::Corruption(format!(
            "directory path is not a safe absolute path: {}",
            target.display()
        )));
    }

    let mut existing_prefix = target.to_path_buf();
    let mut missing_components = Vec::new();
    loop {
        match fs::symlink_metadata(&existing_prefix) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {
                break;
            }
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe existing directory prefix: {}",
                    existing_prefix.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = existing_prefix.file_name().ok_or_else(|| {
                    CheckPoError::Corruption(format!(
                        "no existing directory prefix for {}",
                        target.display()
                    ))
                })?;
                missing_components.push(component.to_os_string());
                if !existing_prefix.pop() {
                    return Err(CheckPoError::Corruption(format!(
                        "no existing directory prefix for {}",
                        target.display()
                    )));
                }
            }
            Err(error) => return Err(io_error(&existing_prefix, error)),
        }
    }

    let canonical_prefix = existing_prefix
        .canonicalize()
        .map_err(|error| io_error(&existing_prefix, error))?;
    ensure_regular_directory_no_follow(&canonical_prefix)?;
    let mut canonical_target = canonical_prefix.clone();
    for component in missing_components.into_iter().rev() {
        canonical_target.push(component);
    }
    create_dir_all_no_follow(&canonical_prefix, &canonical_target)?;
    Ok(canonical_target)
}

pub(crate) fn validate_repository_layout_no_follow(repo_root: &Path) -> Result<()> {
    ensure_regular_directory_no_follow(repo_root)?;
    for relative in [
        "refs",
        "snapshots",
        "snapshots/v2",
        "inventory",
        "inventory/snapshots",
        "inventory/snapshots/states",
        "inventory/snapshots/sets",
        "inventory/snapshots/sets/roots",
        "inventory/snapshots/sets/leaves",
        "manifests",
        "manifests/v2",
        "manifests/v2/nodes",
        "manifests/v2/leaves",
        "objects",
        "objects/loose",
        "indexes",
        "journals",
        "journals/transactions",
        "tmp",
        "locks",
    ] {
        ensure_regular_directory_no_follow(&repo_root.join(relative))?;
    }
    for (relative, label) in [
        ("quarantined-journals", "transaction quarantine"),
        ("recovery-rescues", "recovery rescue"),
    ] {
        let optional_directory = repo_root.join(relative);
        match fs::symlink_metadata(&optional_directory) {
            Ok(metadata) if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(CheckPoError::Corruption(format!(
                    "unsafe {label} directory: {}",
                    optional_directory.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&optional_directory, error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn profiled_directory_creation_separates_prepare_and_fsync() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        fs::create_dir(&base).unwrap();
        let recorder = crate::checkpoint_metrics::ArtifactIoRecorder::default();

        create_dir_all_no_follow_profiled(&base, &base.join("first/second"), Some(&recorder))
            .unwrap();

        let metrics = recorder.snapshot();
        assert_eq!(metrics.directory_create_count, 2);
        assert_eq!(metrics.directory_fsync_count, 2);
        assert_eq!(metrics.existence_check_micros, 0);
    }

    #[cfg(unix)]
    #[test]
    fn batched_directory_creation_syncs_each_mutated_parent_once() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        fs::create_dir(&base).unwrap();
        let recorder = crate::checkpoint_metrics::ArtifactIoRecorder::default();
        let mut batch = DirectorySyncBatch::new(&base).unwrap();

        create_dir_all_no_follow_batched(
            &base,
            &base.join("first/second"),
            Some(&recorder),
            &mut batch,
        )
        .unwrap();
        create_dir_all_no_follow_batched(
            &base,
            &base.join("first/third"),
            Some(&recorder),
            &mut batch,
        )
        .unwrap();

        assert_eq!(batch.pending_count(), 2);
        assert_eq!(recorder.snapshot().directory_fsync_count, 0);
        batch.flush(Some(&recorder)).unwrap();

        let metrics = recorder.snapshot();
        assert_eq!(metrics.directory_create_count, 3);
        assert_eq!(metrics.directory_fsync_count, 2);
        assert_eq!(batch.pending_count(), 0);
    }

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

    #[cfg(unix)]
    #[test]
    fn absolute_directory_creation_canonicalizes_existing_prefix_with_symlink_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        let existing = outside.join("existing");
        let linked = temp.path().join("linked");
        fs::create_dir_all(&existing).unwrap();
        std::os::unix::fs::symlink(&outside, &linked).unwrap();

        let created = create_absolute_dir_all_no_follow(&linked.join("existing/created")).unwrap();

        assert_eq!(created, existing.canonicalize().unwrap().join("created"));
        assert!(created.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn absolute_directory_creation_rejects_dangling_symlink_file_and_parent_component() {
        let temp = tempfile::tempdir().unwrap();
        let dangling = temp.path().join("dangling");
        let file = temp.path().join("file");
        std::os::unix::fs::symlink(temp.path().join("missing"), &dangling).unwrap();
        fs::write(&file, "not a directory").unwrap();

        assert!(create_absolute_dir_all_no_follow(&dangling.join("created")).is_err());
        assert!(create_absolute_dir_all_no_follow(&file.join("created")).is_err());
        assert!(create_absolute_dir_all_no_follow(&temp.path().join("safe/../created")).is_err());
    }
}
