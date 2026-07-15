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
            if segment.ends_with(' ') || segment.ends_with('.') {
                return Err(invalid_path(
                    input,
                    "segments ending with space or dot are not allowed",
                ));
            }
            if segment.chars().any(is_windows_forbidden_character) {
                return Err(invalid_path(
                    input,
                    "Windows-forbidden characters are not allowed",
                ));
            }
            if is_windows_reserved_name(segment) {
                return Err(invalid_path(
                    input,
                    "Windows reserved file names are not allowed",
                ));
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

    pub(crate) fn from_digest_bytes(bytes: [u8; 32]) -> Self {
        Self(encode_lower_hex(&bytes))
    }

    pub(crate) fn digest_bytes(&self) -> [u8; 32] {
        decode_lower_hex(self.as_str()).expect("SnapshotId is validated at construction")
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

    pub(crate) fn from_digest_bytes(bytes: [u8; 32]) -> Self {
        Self(encode_lower_hex(&bytes))
    }

    pub(crate) fn digest_bytes(&self) -> [u8; 32] {
        decode_lower_hex(self.as_str()).expect("ObjectId is validated at construction")
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

    pub(crate) fn uuid_bytes(&self) -> [u8; 16] {
        decode_lower_hex(self.as_str()).expect("ProjectId is validated at construction")
    }

    pub(crate) fn from_uuid_bytes(bytes: [u8; 16]) -> Self {
        Self(encode_lower_hex(&bytes))
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

fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_lower_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 {
        return None;
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(decoded)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
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

pub(crate) fn is_checkpo_temporary_file(path: &Path) -> bool {
    is_checkpo_owned_temporary_file(path)
}

pub(crate) fn is_checkpo_owned_temporary_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(is_checkpo_owned_temporary_file_name)
}

pub(crate) fn is_checkpo_atomic_materialization_temporary_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(is_checkpo_atomic_materialization_temporary_file_name)
}

fn is_checkpo_owned_temporary_file_name(name: &str) -> bool {
    is_checkpo_atomic_materialization_temporary_file_name(name)
        || name
            .strip_prefix(".checkpo-r-")
            .and_then(|body| body.strip_suffix(".tmp"))
            .and_then(|body| body.split_once('-'))
            .is_some_and(|(digest, transaction_id)| {
                is_lowercase_hex(digest, 16) && is_lowercase_hex_uuid(transaction_id)
            })
}

fn is_checkpo_atomic_materialization_temporary_file_name(name: &str) -> bool {
    name.strip_prefix(".checkpo-")
        .and_then(|body| body.strip_suffix(".tmp"))
        .is_some_and(is_lowercase_hex_uuid)
}

fn is_lowercase_hex_uuid(value: &str) -> bool {
    is_lowercase_hex(value, 32)
}

fn is_lowercase_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn invalid_path(input: &str, reason: &str) -> CheckPoError {
    CheckPoError::InvalidTrackedPath(format!("{input}: {reason}"))
}

fn is_windows_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || upper
        .strip_prefix("COM")
        .is_some_and(is_reserved_device_digit)
        || upper
            .strip_prefix("LPT")
            .is_some_and(is_reserved_device_digit)
}

fn is_reserved_device_digit(value: &str) -> bool {
    matches!(
        value,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

fn is_windows_forbidden_character(value: char) -> bool {
    value <= '\u{1f}' || matches!(value, '<' | '>' | '"' | '|' | '?' | '*')
}

#[cfg(test)]
mod tests {
    use super::{
        is_checkpo_atomic_materialization_temporary_file, is_checkpo_owned_temporary_file,
        TrackedUnityFilePath,
    };
    use std::path::Path;

    #[test]
    fn atomic_materialization_temporary_file_name_is_strict() {
        let owned = Path::new("Assets/.checkpo-0123456789abcdef0123456789abcdef.tmp");
        assert!(is_checkpo_atomic_materialization_temporary_file(owned));
        assert!(is_checkpo_owned_temporary_file(owned));

        for path in [
            "Assets/.checkpo-0123456789abcdef0123456789abcde.tmp",
            "Assets/.checkpo-0123456789abcdef0123456789abcdef0.tmp",
            "Assets/.checkpo-0123456789abcdef0123456789abcdeF.tmp",
            "Assets/.checkpo-note-0123456789abcdef0123456789abcdef.tmp",
            "Assets/checkpo-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-0123456789abcdef0123456789abcdef.tmp.txt",
        ] {
            assert!(
                !is_checkpo_atomic_materialization_temporary_file(Path::new(path)),
                "{path}"
            );
        }
    }

    #[test]
    fn owned_temporary_file_name_is_limited_to_generated_formats() {
        for path in [
            "Assets/.checkpo-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-r-0123456789abcdef-0123456789abcdef0123456789abcdef.tmp",
        ] {
            assert!(is_checkpo_owned_temporary_file(Path::new(path)), "{path}");
        }

        for path in [
            "Assets/.Foo.prefab.0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-Foo.prefab-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-anything-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-r-0123456789abcde-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-r-0123456789abcdef0-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-r-0123456789abcdeF-0123456789abcdef0123456789abcdef.tmp",
            "Assets/.checkpo-r-0123456789abcdef-0123456789abcdef0123456789abcdeF.tmp",
            "Assets/.checkpo-r-extra-0123456789abcdef-0123456789abcdef0123456789abcdef.tmp",
        ] {
            assert!(!is_checkpo_owned_temporary_file(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn tracked_path_rejects_windows_reserved_names() {
        for path in [
            "Assets/CON",
            "Assets/con.txt",
            "Assets/Aux.asset",
            "Assets/COM1.prefab",
            "Assets/LPT9.meta",
            "Assets/COM¹.prefab",
            "Assets/com².asset",
            "Assets/LPT³.meta",
            "Assets/CONIN$",
            "Assets/conout$.txt",
            "ProjectSettings/NUL",
        ] {
            assert!(TrackedUnityFilePath::parse(path).is_err(), "{path}");
        }
    }

    #[test]
    fn tracked_path_rejects_windows_forbidden_characters_and_controls() {
        for path in [
            "Assets/Foo<Bar.prefab",
            "Assets/Foo>Bar.prefab",
            "Assets/Foo\"Bar.prefab",
            "Assets/Foo|Bar.prefab",
            "Assets/Foo?Bar.prefab",
            "Assets/Foo*Bar.prefab",
            "Assets/Foo\u{0}Bar.prefab",
            "Assets/Foo\u{1f}Bar.prefab",
        ] {
            assert!(TrackedUnityFilePath::parse(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn tracked_path_rejects_segments_ending_with_dot_or_space() {
        for path in [
            "Assets/Foo.",
            "Assets/Foo ",
            "Assets/Folder./File.asset",
            "Assets/Folder /File.asset",
        ] {
            assert!(TrackedUnityFilePath::parse(path).is_err(), "{path}");
        }
    }
}
