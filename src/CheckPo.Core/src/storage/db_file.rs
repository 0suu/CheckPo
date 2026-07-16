use super::*;

pub fn db_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(derived_index_directory(repo_root)?.join("local.db"))
}

pub fn file_fingerprint_db_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(derived_index_directory(repo_root)?.join("working-tree-cache.db"))
}

fn derived_index_directory(repo_root: &Path) -> Result<PathBuf> {
    let project_id = repo_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            CheckPoError::Corruption(format!(
                "repository path has no project id: {}",
                repo_root.display()
            ))
        })?;
    let project_id = crate::ProjectId::parse(project_id).map_err(|_| {
        CheckPoError::Corruption(format!(
            "repository path does not end in a valid project id: {}",
            repo_root.display()
        ))
    })?;
    Ok(crate::default_storage_root()?
        .join("derived-indexes")
        .join(project_id.as_str()))
}

pub(crate) fn open_file_fingerprint_db(repo_root: &Path) -> Result<BoundDbConnection> {
    let path = file_fingerprint_db_path(repo_root)?;
    open_db_path(&path)
}

pub(crate) fn remove_file_fingerprint_db_if_exists(repo_root: &Path) -> Result<()> {
    let path = file_fingerprint_db_path(repo_root)?;
    remove_db_path_if_exists(&path)
}

pub(crate) fn remove_db_path_if_exists(path: &Path) -> Result<()> {
    let Some(indexes_path) = path.parent() else {
        return Err(CheckPoError::Corruption(
            "SQLite path has no parent".to_string(),
        ));
    };
    let anchored_indexes = match AnchoredRoot::open(indexes_path) {
        Ok(root) => root,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    let indexes = anchored_indexes.open_directory_for_mutation(Path::new(""), false)?;
    let leaf = path.file_name().ok_or_else(|| {
        CheckPoError::Corruption(format!("SQLite path has no file name: {}", path.display()))
    })?;
    let file = match indexes.open_file(leaf) {
        Ok(file) => file,
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    indexes.unlink_file_if_bound(leaf, file)?;
    indexes.sync_all()?;
    anchored_indexes.verify_root_binding()
}

pub(crate) struct BoundDbConnection {
    connection: rusqlite::Connection,
    _database_directory: AnchoredRoot,
    _indexes: AnchoredParent,
}

impl std::ops::Deref for BoundDbConnection {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

pub(crate) fn open_db_path(path: &Path) -> Result<BoundDbConnection> {
    let indexes_path = path.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!("SQLite path has no parent: {}", path.display()))
    })?;
    create_absolute_dir_all_no_follow(indexes_path)?;
    let anchored_indexes = AnchoredRoot::open(indexes_path)?;
    let indexes = anchored_indexes.open_directory_for_mutation(Path::new(""), false)?;
    anchored_indexes.verify_root_binding()?;
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
    anchored_indexes.verify_root_binding()?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    Ok(BoundDbConnection {
        connection,
        _database_directory: anchored_indexes,
        _indexes: indexes,
    })
}
