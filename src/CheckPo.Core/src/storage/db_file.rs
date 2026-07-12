use super::*;

pub fn db_path(repo_root: &Path) -> PathBuf {
    repo_root.join("indexes").join("local.db")
}

pub fn file_fingerprint_db_path(repo_root: &Path) -> PathBuf {
    repo_root.join("indexes").join("working-tree-cache.db")
}

pub fn open_db(repo_root: &Path) -> Result<rusqlite::Connection> {
    let path = db_path(repo_root);
    open_db_path(&path)
}

pub fn open_file_fingerprint_db(repo_root: &Path) -> Result<rusqlite::Connection> {
    let path = file_fingerprint_db_path(repo_root);
    open_db_path(&path)
}

fn open_db_path(path: &Path) -> Result<rusqlite::Connection> {
    if let Some(parent) = path.parent() {
        create_absolute_dir_all_no_follow(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
        Ok(_) => {
            return Err(CheckPoError::Corruption(format!(
                "unsafe SQLite database path: {}",
                path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| io_error(path, error))?;
        }
        Err(error) => return Err(io_error(path, error)),
    }
    let conn = rusqlite::Connection::open(path).map_err(|error| db_error(path, error))?;
    ensure_regular_file_no_follow(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| db_error(path, error))?;
    Ok(conn)
}
