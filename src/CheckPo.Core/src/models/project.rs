use crate::{ProjectId, ProjectRoot, StorageRoot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMarkerFile {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub created_at_utc: String,
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub project_id: ProjectId,
    pub project_root: ProjectRoot,
    pub storage_root: StorageRoot,
    pub repo_root: PathBuf,
    pub location_status: ProjectLocationStatus,
    pub warnings: Vec<ProjectWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    pub project_id: String,
    pub project_root_path: PathBuf,
    pub storage_root_path: PathBuf,
    pub project_name: Option<String>,
    pub unity_version: Option<String>,
    pub location_status: ProjectLocationStatus,
    pub warnings: Vec<ProjectWarning>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProjectLocationStatus {
    Current,
    MovedFromMissingOrDifferentMarker,
    CopiedSuspected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProjectWarningKind {
    CopiedProjectSuspected,
    ProjectMoved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectWarning {
    pub kind: ProjectWarningKind,
    pub message: String,
    pub location_status: ProjectLocationStatus,
    pub previous_project_root_path: PathBuf,
    pub current_project_root_path: PathBuf,
    pub previous_path_exists: bool,
    pub previous_marker_has_same_project_id: bool,
    pub requires_user_decision: bool,
    pub destructive_operations_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryFile {
    pub schema_version: u32,
    pub projects: BTreeMap<String, RegistryProjectEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryProjectEntry {
    pub storage_root_path: PathBuf,
    pub last_project_root_path: PathBuf,
    pub project_name: Option<String>,
    pub updated_at_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryConfig {
    pub schema_version: u32,
    pub repo_format_version: u32,
    pub project_id: ProjectId,
    pub hash_algorithm: String,
    pub snapshot_format: String,
    pub object_format: String,
    pub manifest_chunk_format: String,
    pub manifest_storage_format: String,
    pub path_key_policy: String,
}
