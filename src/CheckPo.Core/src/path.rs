use crate::{CheckPoError, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct TrackedUnityFilePath(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SnapshotId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ObjectId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ProjectId(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoot(PathBuf);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageRoot(PathBuf);

impl TrackedUnityFilePath {
    pub fn parse(input: &str) -> Result<Self> {
        let value = input;
        if value.is_empty() {
            return Err(invalid_path(input, "empty path"));
        }
        if value.contains('\\') {
            return Err(invalid_path(input, "backslash is not allowed"));
        }
        if value.contains(':') {
            return Err(invalid_path(input, "colon is not allowed"));
        }
        if Path::new(value).is_absolute() {
            return Err(invalid_path(input, "absolute path is not allowed"));
        }
        let segments = value.split('/').collect::<Vec<_>>();
        if segments.len() < 2 {
            return Err(invalid_path(input, "tracked root alone is not a file path"));
        }
        match segments[0] {
            "Assets" | "Packages" | "ProjectSettings" => {}
            _ => return Err(CheckPoError::OutsideTrackedScope(input.to_string())),
        }
        for segment in &segments {
            if segment.is_empty() {
                return Err(invalid_path(input, "empty segment is not allowed"));
            }
            if *segment == "." || *segment == ".." {
                return Err(invalid_path(input, "dot segments are not allowed"));
            }
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_project_path(&self, project_root: &Path) -> PathBuf {
        project_root.join(self.as_str())
    }
}

impl TryFrom<String> for TrackedUnityFilePath {
    type Error = CheckPoError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl<'de> Deserialize<'de> for TrackedUnityFilePath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for TrackedUnityFilePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SnapshotId {
    pub fn parse(input: &str) -> Result<Self> {
        validate_lower_hex_id(input, "snapshot")?;
        Ok(Self(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SnapshotId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ObjectId {
    pub fn parse(input: &str) -> Result<Self> {
        validate_lower_hex_id(input, "object")?;
        Ok(Self(input.to_string()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ProjectId {
    pub fn parse(input: &str) -> Result<Self> {
        validate_project_id(input)?;
        Ok(Self(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProjectRoot {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl StorageRoot {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

pub fn parse_tracked_paths(paths: &[String]) -> Result<Vec<TrackedUnityFilePath>> {
    paths
        .iter()
        .map(|path| TrackedUnityFilePath::parse(path))
        .collect()
}

pub fn hash_bytes(bytes: &[u8]) -> ObjectId {
    ObjectId::from_bytes(bytes)
}

pub fn validate_lower_hex_id(input: &str, label: &str) -> Result<()> {
    if input.len() != 64
        || !input
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(CheckPoError::InvalidId(format!(
            "{label} id must be 64 lowercase hex characters: {input}"
        )));
    }
    Ok(())
}

pub fn validate_project_id(input: &str) -> Result<()> {
    if input.len() != 32
        || !input
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(CheckPoError::InvalidId(format!(
            "project id must be 32 lowercase hex characters: {input}"
        )));
    }
    Ok(())
}

pub fn relative_path_from_project(project_root: &Path, full_path: &Path) -> Result<String> {
    let relative = full_path
        .strip_prefix(project_root)
        .map_err(|_| CheckPoError::OutsideTrackedScope(full_path.display().to_string()))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(value) = component else {
            return Err(CheckPoError::InvalidTrackedPath(
                relative.display().to_string(),
            ));
        };
        let value = value.to_str().ok_or_else(|| {
            CheckPoError::InvalidTrackedPath(format!(
                "{}: non-UTF-8 path component is not supported",
                relative.display()
            ))
        })?;
        parts.push(value.to_string());
    }
    Ok(parts.join("/"))
}

fn invalid_path(input: &str, reason: &str) -> CheckPoError {
    CheckPoError::InvalidTrackedPath(format!("{input}: {reason}"))
}
