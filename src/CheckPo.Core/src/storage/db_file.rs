use super::*;

pub fn db_path(repo_root: &Path) -> PathBuf {
    repo_root.join("indexes").join("local.db")
}

pub fn file_fingerprint_db_path(repo_root: &Path) -> PathBuf {
    repo_root.join("indexes").join("working-tree-cache.db")
}

#[cfg(test)]
pub(crate) fn open_db(repo_root: &Path) -> Result<BoundDbConnection> {
    let path = db_path(repo_root);
    open_db_path(&path)
}

pub(crate) fn open_file_fingerprint_db(repo_root: &Path) -> Result<BoundDbConnection> {
    let path = file_fingerprint_db_path(repo_root);
    open_db_path(&path)
}

pub(crate) fn remove_file_fingerprint_db_if_exists(repo_root: &Path) -> Result<()> {
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let indexes = match anchored_repo.open_directory_for_mutation(Path::new("indexes"), false) {
        Ok(indexes) => indexes,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    anchored_repo.verify_parent_binding(Path::new("indexes"), &indexes)?;
    let leaf = std::ffi::OsStr::new("working-tree-cache.db");
    let file = match indexes.open_file(leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    indexes.unlink_file_if_bound(leaf, file)?;
    indexes.sync_all()?;
    anchored_repo.verify_parent_binding(Path::new("indexes"), &indexes)?;
    anchored_repo.verify_root_binding()
}

pub(crate) struct BoundDbConnection {
    connection: rusqlite::Connection,
    _repo: AnchoredRoot,
    _indexes: AnchoredParent,
}

impl std::ops::Deref for BoundDbConnection {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

fn open_db_path(path: &Path) -> Result<BoundDbConnection> {
    let indexes_path = path.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!("SQLite path has no parent: {}", path.display()))
    })?;
    create_absolute_dir_all_no_follow(indexes_path)?;
    let repo_root = indexes_path.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "SQLite path is outside a repository: {}",
            path.display()
        ))
    })?;
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let indexes = anchored_repo.open_directory_for_mutation(Path::new("indexes"), false)?;
    anchored_repo.verify_parent_binding(Path::new("indexes"), &indexes)?;
    let leaf = path.file_name().ok_or_else(|| {
        CheckPoError::Corruption(format!("SQLite path has no file name: {}", path.display()))
    })?;
    match indexes.open_file(leaf) {
        Ok(file) => indexes.verify_file_binding(leaf, &file)?,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            let file = indexes.create_new_file(leaf)?;
            file.sync_all()?;
            indexes.verify_file_binding(leaf, &file)?;
            indexes.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    let sqlite_path = indexes_path
        .canonicalize()
        .map_err(|error| io_error(indexes_path, error))?
        .join(leaf);
    let connection = rusqlite::Connection::open_with_flags(
        &sqlite_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| db_error(path, error))?;
    let opened = indexes.open_file(leaf)?;
    indexes.verify_file_binding(leaf, &opened)?;
    anchored_repo.verify_parent_binding(Path::new("indexes"), &indexes)?;
    anchored_repo.verify_root_binding()?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    Ok(BoundDbConnection {
        connection,
        _repo: anchored_repo,
        _indexes: indexes,
    })
}
