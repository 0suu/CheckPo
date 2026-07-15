use super::*;

pub const REPO_FORMAT_VERSION: u32 = 5;
const REPOSITORY_CONFIG_SCHEMA_VERSION: u32 = 2;
pub const SNAPSHOT_FORMAT: &str = "merkle-radix-bin-v2";
pub const OBJECT_FORMAT: &str = "loose-whole-file-one-level-v2";
pub const HASH_ALGORITHM: &str = "blake3";
pub const MANIFEST_CHUNK_FORMAT: &str = "merkle-radix-bin-v2";
pub const MANIFEST_STORAGE_FORMAT: &str = "loose-content-addressed-one-level-v2";
pub const PATH_KEY_POLICY: &str = "unicode-16.0-nfc-lowercase-v1";

pub fn canonical_utc<T: Into<DateTime<Utc>>>(time: T) -> String {
    time.into().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn now_utc_string() -> String {
    canonical_utc(Utc::now())
}

pub fn default_repository_config(project_id: &ProjectId) -> RepositoryConfig {
    RepositoryConfig {
        schema_version: REPOSITORY_CONFIG_SCHEMA_VERSION,
        repo_format_version: REPO_FORMAT_VERSION,
        project_id: project_id.clone(),
        hash_algorithm: HASH_ALGORITHM.to_string(),
        snapshot_format: SNAPSHOT_FORMAT.to_string(),
        object_format: OBJECT_FORMAT.to_string(),
        manifest_chunk_format: MANIFEST_CHUNK_FORMAT.to_string(),
        manifest_storage_format: MANIFEST_STORAGE_FORMAT.to_string(),
        path_key_policy: PATH_KEY_POLICY.to_string(),
    }
}

pub fn validate_repository_config(config: &RepositoryConfig, project_id: &ProjectId) -> Result<()> {
    validate_repository_versions(config.schema_version, config.repo_format_version)?;
    let expected = default_repository_config(project_id);
    if config != &expected {
        return Err(CheckPoError::Corruption(
            "repo.json does not match CheckPo repository format v5".to_string(),
        ));
    }
    Ok(())
}

pub fn init_repo_layout(storage_root: &Path, project_id: &ProjectId) -> Result<PathBuf> {
    let repo_root = repo_root(storage_root, project_id);
    create_dir_all_no_follow(storage_root, &repo_root)?;
    let config_path = repo_root.join("repo.json");
    let config_exists = match fs::symlink_metadata(&config_path) {
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err(CheckPoError::Corruption(format!(
                    "repo.json is not a regular file: {}",
                    config_path.display()
                )));
            }
            load_repo_config(&repo_root, project_id)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io_error(&config_path, error)),
    };
    for dir in [
        repo_root.join("refs"),
        snapshots_dir(&repo_root),
        manifest_nodes_dir(&repo_root),
        manifest_leaves_dir(&repo_root),
        repo_root.join("objects").join("loose"),
        repo_root.join("indexes"),
        repo_root.join("journals"),
        repo_root.join("journals").join("transactions"),
        repo_root.join("tmp"),
        repo_root.join("locks"),
    ] {
        create_dir_all_no_follow(&repo_root, &dir)?;
    }
    if !config_exists {
        match AnchoredRoot::open(&repo_root)?.write_json_atomic_new(
            Path::new("repo.json"),
            &default_repository_config(project_id),
        ) {
            Ok(()) => {}
            Err(CheckPoError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                let metadata = fs::symlink_metadata(&config_path)
                    .map_err(|error| io_error(&config_path, error))?;
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                    return Err(CheckPoError::Corruption(format!(
                        "repo.json is not a regular file: {}",
                        config_path.display()
                    )));
                }
                load_repo_config(&repo_root, project_id)?;
            }
            Err(error) => return Err(error),
        }
    }
    super::snapshot_inventory::initialize_snapshot_inventory(&repo_root, project_id)?;
    validate_repository_layout_no_follow(&repo_root)?;
    Ok(repo_root)
}

fn validate_repository_versions(schema_version: u32, repo_format_version: u32) -> Result<()> {
    if schema_version != REPOSITORY_CONFIG_SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "repository config schema".to_string(),
            found: schema_version,
            supported: REPOSITORY_CONFIG_SCHEMA_VERSION,
        });
    }
    if repo_format_version != REPO_FORMAT_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "repository format".to_string(),
            found: repo_format_version,
            supported: REPO_FORMAT_VERSION,
        });
    }
    Ok(())
}

pub fn repo_root(storage_root: &Path, project_id: &ProjectId) -> PathBuf {
    storage_root.join("repos").join(project_id.as_str())
}

pub fn load_repo_config(repo_root: &Path, project_id: &ProjectId) -> Result<RepositoryConfig> {
    let path = repo_root.join("repo.json");
    ensure_regular_directory_no_follow(repo_root)?;
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RepositoryConfigEnvelope {
        schema_version: u32,
        repo_format_version: Option<u32>,
    }

    let bytes = AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, 1024 * 1024)?;
    let envelope: RepositoryConfigEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| json_error(&path, error))?;
    if envelope.schema_version != REPOSITORY_CONFIG_SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "repository config schema".to_string(),
            found: envelope.schema_version,
            supported: REPOSITORY_CONFIG_SCHEMA_VERSION,
        });
    }
    if let Some(repo_format_version) = envelope.repo_format_version {
        if repo_format_version != REPO_FORMAT_VERSION {
            return Err(CheckPoError::UnsupportedFormat {
                artifact: "repository format".to_string(),
                found: repo_format_version,
                supported: REPO_FORMAT_VERSION,
            });
        }
    }
    let config: RepositoryConfig =
        serde_json::from_slice(&bytes).map_err(|error| json_error(&path, error))?;
    validate_repository_config(&config, project_id)?;
    Ok(config)
}

pub fn snapshots_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("snapshots").join("v2")
}

pub fn snapshot_path(repo_root: &Path, snapshot_id: &SnapshotId) -> PathBuf {
    let id = snapshot_id.as_str();
    snapshots_dir(repo_root)
        .join(&id[0..2])
        .join(&id[2..4])
        .join(format!("{id}.root"))
}

pub(crate) fn manifest_nodes_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("manifests").join("v2").join("nodes")
}

pub(crate) fn manifest_leaves_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("manifests").join("v2").join("leaves")
}

pub(crate) fn manifest_node_path(repo_root: &Path, id: &str) -> PathBuf {
    manifest_nodes_dir(repo_root).join(&id[0..2]).join(id)
}

pub(crate) fn manifest_leaf_path(repo_root: &Path, id: &str) -> PathBuf {
    manifest_leaves_dir(repo_root).join(&id[0..2]).join(id)
}

pub fn refs_latest_path(repo_root: &Path) -> PathBuf {
    repo_root.join("refs").join("latest")
}

pub fn checkpoint_names_path(repo_root: &Path) -> PathBuf {
    repo_root.join("refs").join("checkpoint_names.json")
}

pub fn object_path(repo_root: &Path, object_id: &ObjectId) -> PathBuf {
    let id = object_id.as_str();
    repo_root
        .join("objects")
        .join("loose")
        .join(&id[0..2])
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
    if parts.len() != 4 || parts[0] != "objects" || parts[1] != "loose" {
        return Err("object path must be objects/loose/<first2>/<hash>.".to_string());
    }
    let first = &parts[2];
    let hash = &parts[3];
    if first.len() != 2 {
        return Err("object path prefix must be two lowercase hex characters.".to_string());
    }
    if hash.len() != 64 {
        return Err("object filename must be a 64 character BLAKE3 hash.".to_string());
    }
    if hash.get(0..2) != Some(first.as_str()) {
        return Err("object path prefix does not match object hash.".to_string());
    }
    ObjectId::parse(hash).map_err(|error| error.to_string())
}
