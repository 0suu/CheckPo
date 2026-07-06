use super::*;

pub const REPO_FORMAT_VERSION: u32 = 1;
pub const SNAPSHOT_FORMAT: &str = "canonical-json-v1";
pub const OBJECT_FORMAT: &str = "loose-whole-file-v1";
pub const HASH_ALGORITHM: &str = "blake3";

pub fn canonical_utc<T: Into<DateTime<Utc>>>(time: T) -> String {
    time.into().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn now_utc_string() -> String {
    canonical_utc(Utc::now())
}

pub fn default_repository_config(project_id: &ProjectId) -> RepositoryConfig {
    RepositoryConfig {
        schema_version: 1,
        repo_format_version: REPO_FORMAT_VERSION,
        project_id: project_id.clone(),
        hash_algorithm: HASH_ALGORITHM.to_string(),
        snapshot_format: SNAPSHOT_FORMAT.to_string(),
        object_format: OBJECT_FORMAT.to_string(),
    }
}

pub fn validate_repository_config(config: &RepositoryConfig, project_id: &ProjectId) -> Result<()> {
    let expected = default_repository_config(project_id);
    if config != &expected {
        return Err(CheckPoError::Corruption(
            "repo.json does not match CheckPo schema v1".to_string(),
        ));
    }
    Ok(())
}

pub fn init_repo_layout(storage_root: &Path, project_id: &ProjectId) -> Result<PathBuf> {
    let repo_root = repo_root(storage_root, project_id);
    for dir in [
        repo_root.join("refs"),
        repo_root.join("snapshots"),
        repo_root.join("objects").join("loose"),
        repo_root.join("indexes"),
        repo_root.join("journals"),
        repo_root.join("tmp"),
        repo_root.join("locks"),
    ] {
        fs::create_dir_all(&dir).map_err(|error| io_error(&dir, error))?;
    }
    write_json_atomic(
        &repo_root.join("repo.json"),
        &default_repository_config(project_id),
    )?;
    Ok(repo_root)
}

pub fn repo_root(storage_root: &Path, project_id: &ProjectId) -> PathBuf {
    storage_root.join("repos").join(project_id.as_str())
}

pub fn load_repo_config(repo_root: &Path, project_id: &ProjectId) -> Result<RepositoryConfig> {
    let path = repo_root.join("repo.json");
    let config: RepositoryConfig = read_json(&path)?;
    validate_repository_config(&config, project_id)?;
    Ok(config)
}

pub fn snapshots_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("snapshots")
}

pub fn snapshot_path(repo_root: &Path, snapshot_id: &SnapshotId) -> PathBuf {
    snapshots_dir(repo_root).join(format!("{}.json", snapshot_id.as_str()))
}

pub fn refs_latest_path(repo_root: &Path) -> PathBuf {
    repo_root.join("refs").join("latest")
}

pub fn object_path(repo_root: &Path, object_id: &ObjectId) -> PathBuf {
    let id = object_id.as_str();
    repo_root
        .join("objects")
        .join("loose")
        .join(&id[0..2])
        .join(&id[2..4])
        .join(id)
}

pub(crate) fn object_id_from_loose_relative_path(
    relative: &Path,
) -> std::result::Result<ObjectId, String> {
    let parts = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_string_lossy().to_string(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    if parts.len() != 5 || parts[0] != "objects" || parts[1] != "loose" {
        return Err("object path must be objects/loose/<first2>/<next2>/<hash>.".to_string());
    }
    let first = &parts[2];
    let second = &parts[3];
    let hash = &parts[4];
    if first.len() != 2 || second.len() != 2 {
        return Err("object path prefixes must be two lowercase hex characters.".to_string());
    }
    if hash.len() != 64 {
        return Err("object filename must be a 64 character BLAKE3 hash.".to_string());
    }
    if hash.get(0..2) != Some(first.as_str()) || hash.get(2..4) != Some(second.as_str()) {
        return Err("object path prefixes do not match object hash.".to_string());
    }
    ObjectId::parse(hash).map_err(|error| error.to_string())
}
