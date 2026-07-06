use super::*;

pub fn db_path(repo_root: &Path) -> PathBuf {
    repo_root.join("indexes").join("local.db")
}

pub fn open_db(repo_root: &Path) -> Result<rusqlite::Connection> {
    let path = db_path(repo_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    let conn = rusqlite::Connection::open(&path).map_err(|error| db_error(&path, error))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| db_error(&path, error))?;
    Ok(conn)
}
