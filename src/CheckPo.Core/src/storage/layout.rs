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
    let anchored_repo = create_private_repository_root(storage_root, &repo_root)?;
    let config_path = repo_root.join("repo.json");
    let config_exists = match anchored_repo.open_file(Path::new("repo.json")) {
        Ok(_) => {
            load_repo_config_anchored(&anchored_repo, &config_path, project_id)?;
            true
        }
        Err(CheckPoError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            false
        }
        Err(error) => return Err(error),
    };
    for relative in [
        Path::new("refs"),
        Path::new("snapshots/v2"),
        Path::new("manifests/v2/nodes"),
        Path::new("manifests/v2/leaves"),
        Path::new("objects/loose"),
        Path::new("indexes"),
        Path::new("journals"),
        Path::new("journals/transactions"),
        Path::new("tmp"),
        Path::new("locks"),
    ] {
        let directory = anchored_repo.open_directory_for_mutation(relative, true)?;
        anchored_repo.verify_parent_binding(relative, &directory)?;
    }
    if !config_exists {
        match anchored_repo.write_json_atomic_new(
            Path::new("repo.json"),
            &default_repository_config(project_id),
        ) {
            Ok(()) => {}
            Err(CheckPoError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                load_repo_config_anchored(&anchored_repo, &config_path, project_id)?;
            }
            Err(error) => return Err(error),
        }
        anchored_repo.make_file_private(Path::new("repo.json"))?;
    }
    anchored_repo.verify_root_binding()?;
    super::snapshot_inventory::initialize_snapshot_inventory_anchored(
        &anchored_repo,
        &repo_root,
        project_id,
    )?;
    anchored_repo.verify_root_binding()?;
    validate_repository_layout_no_follow(&repo_root)?;
    Ok(repo_root)
}

#[cfg(unix)]
fn create_private_repository_root(storage_root: &Path, repo_root: &Path) -> Result<AnchoredRoot> {
    let project_id = repo_root.file_name().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "repository root has no project id component: {}",
            repo_root.display()
        ))
    })?;
    let storage = AnchoredRoot::open(storage_root)?;
    let storage_parent = storage.open_directory_for_mutation(Path::new(""), false)?;
    let repos = storage_parent.open_or_create_private_directory(std::ffi::OsStr::new("repos"))?;
    let repository = repos.open_or_create_private_directory(project_id)?;
    let anchored_repo = AnchoredRoot::from_held_parent(repository);
    anchored_repo.verify_root_binding()?;
    Ok(anchored_repo)
}

#[cfg(not(unix))]
fn create_private_repository_root(storage_root: &Path, repo_root: &Path) -> Result<AnchoredRoot> {
    create_dir_all_no_follow(storage_root, repo_root)?;
    AnchoredRoot::open(repo_root)
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
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    load_repo_config_anchored(&anchored_repo, &path, project_id)
}

fn load_repo_config_anchored(
    anchored_repo: &AnchoredRoot,
    path: &Path,
    project_id: &ProjectId,
) -> Result<RepositoryConfig> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RepositoryConfigEnvelope {
        schema_version: u32,
        repo_format_version: Option<u32>,
    }

    let bytes = anchored_repo.read_bytes_bounded_path(path, 1024 * 1024)?;
    let envelope: RepositoryConfigEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| json_error(path, error))?;
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
        serde_json::from_slice(&bytes).map_err(|error| json_error(path, error))?;
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
