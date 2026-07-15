//! Canonical binary codec for snapshot v2 roots and manifest chunks.
//!
//! This module deliberately does not use serde. Field order, integer width and
//! byte order are persistent repository format, so they are written explicitly.

use std::fmt;

const MAGIC: &[u8; 8] = b"CPMRKL2\0";
const CODEC_VERSION: u8 = 1;
const ENVELOPE_LEN: usize = 16;

pub const MAX_ROOT_BYTES: usize = 64 * 1024;
pub const MAX_MANIFEST_CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_EXACT_PATH_BYTES: usize = 128 * 1024;
pub const MAX_CHECKPOINT_NAME_BYTES: usize = 4 * 1024;
pub const MAX_TOOL_VERSION_BYTES: usize = 256;
pub const MAX_PREFIX_BYTES: usize = MAX_EXACT_PATH_BYTES;
pub const MAX_LEAF_ENTRIES: usize = 16_384;
pub const MAX_NODE_CHILDREN: usize = 256;

pub const ROOT_HASH_DOMAIN: &[u8] = b"checkpo.snapshot-root.v2\0";
pub const NODE_HASH_DOMAIN: &[u8] = b"checkpo.manifest-node.v2\0";
pub const LEAF_HASH_DOMAIN: &[u8] = b"checkpo.manifest-leaf.v2\0";

const PAYLOAD_SCHEMA: u16 = 1;
const TRACKED_SCOPE_POLICY: u16 = 1;
const PATH_KEY_POLICY: u16 = 1;
const CONTENT_POLICY: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("invalid snapshot v2 encoding: {0}")]
    Invalid(String),
    #[error("snapshot v2 value exceeds {field} limit ({actual} > {limit})")]
    Limit {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("snapshot v2 integer overflow while computing {0}")]
    Overflow(&'static str),
}

pub type CodecResult<T> = std::result::Result<T, CodecError>;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest32([u8; 32]);

impl Digest32 {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
        }
        output
    }
}

impl fmt::Debug for Digest32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl fmt::Display for Digest32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ChunkKind {
    Root = 1,
    Node = 2,
    Leaf = 3,
}

impl ChunkKind {
    fn parse(value: u8) -> CodecResult<Self> {
        match value {
            1 => Ok(Self::Root),
            2 => Ok(Self::Node),
            3 => Ok(Self::Leaf),
            _ => Err(CodecError::Invalid(format!("unknown chunk kind {value}"))),
        }
    }

    fn maximum_stored_len(self) -> usize {
        match self {
            Self::Root => MAX_ROOT_BYTES,
            Self::Node | Self::Leaf => MAX_MANIFEST_CHUNK_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestRef {
    pub kind: ChunkKind,
    pub id: Digest32,
}

impl ManifestRef {
    pub fn new(kind: ChunkKind, id: Digest32) -> CodecResult<Self> {
        if !matches!(kind, ChunkKind::Node | ChunkKind::Leaf) {
            return Err(CodecError::Invalid(
                "manifest reference must point to a node or leaf".to_string(),
            ));
        }
        Ok(Self { kind, id })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub unix_seconds: i64,
    pub nanoseconds: u32,
}

impl Timestamp {
    pub fn new(unix_seconds: i64, nanoseconds: u32) -> CodecResult<Self> {
        if nanoseconds >= 1_000_000_000 {
            return Err(CodecError::Invalid(format!(
                "timestamp nanoseconds out of range: {nanoseconds}"
            )));
        }
        Ok(Self {
            unix_seconds,
            nanoseconds,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManifestSummary {
    pub entry_count: u64,
    pub logical_size_bytes: u64,
    pub total_exact_path_bytes: u64,
}

impl ManifestSummary {
    pub fn checked_add(self, other: Self) -> CodecResult<Self> {
        Ok(Self {
            entry_count: self
                .entry_count
                .checked_add(other.entry_count)
                .ok_or(CodecError::Overflow("manifest entry count"))?,
            logical_size_bytes: self
                .logical_size_bytes
                .checked_add(other.logical_size_bytes)
                .ok_or(CodecError::Overflow("manifest logical size"))?,
            total_exact_path_bytes: self
                .total_exact_path_bytes
                .checked_add(other.total_exact_path_bytes)
                .ok_or(CodecError::Overflow("manifest exact path bytes"))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRoot {
    pub project_id: [u8; 16],
    pub parent: Option<Digest32>,
    pub created: Timestamp,
    pub checkpoint_name: String,
    pub tool_version: String,
    pub manifest: Option<ManifestRef>,
    pub summary: ManifestSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafEntry {
    pub exact_path: String,
    pub size_bytes: u64,
    pub modified: Timestamp,
    pub object_id: Digest32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestLeaf {
    pub portable_prefix: Vec<u8>,
    pub entries: Vec<LeafEntry>,
}

impl ManifestLeaf {
    pub fn summary(&self) -> CodecResult<ManifestSummary> {
        let mut summary = ManifestSummary::default();
        for entry in &self.entries {
            summary = summary.checked_add(ManifestSummary {
                entry_count: 1,
                logical_size_bytes: entry.size_bytes,
                total_exact_path_bytes: u64::try_from(entry.exact_path.len())
                    .map_err(|_| CodecError::Overflow("exact path byte count"))?,
            })?;
        }
        Ok(summary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestChild {
    /// Zero is the portable-key terminator; all other values are the next byte.
    pub edge: u8,
    pub child: ManifestRef,
    pub summary: ManifestSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestNode {
    pub portable_prefix: Vec<u8>,
    pub children: Vec<ManifestChild>,
}

impl ManifestNode {
    pub fn summary(&self) -> CodecResult<ManifestSummary> {
        self.children
            .iter()
            .try_fold(ManifestSummary::default(), |summary, child| {
                summary.checked_add(child.summary)
            })
    }
}

pub fn root_id(stored_bytes: &[u8]) -> Digest32 {
    domain_hash(ROOT_HASH_DOMAIN, stored_bytes)
}

pub fn verify_root_id(expected: Digest32, stored_bytes: &[u8]) -> CodecResult<()> {
    let actual = root_id(stored_bytes);
    if actual != expected {
        return Err(CodecError::Invalid(format!(
            "snapshot root digest mismatch: expected {expected}, found {actual}"
        )));
    }
    Ok(())
}

pub fn manifest_id(kind: ChunkKind, stored_bytes: &[u8]) -> CodecResult<Digest32> {
    match kind {
        ChunkKind::Node => Ok(domain_hash(NODE_HASH_DOMAIN, stored_bytes)),
        ChunkKind::Leaf => Ok(domain_hash(LEAF_HASH_DOMAIN, stored_bytes)),
        ChunkKind::Root => Err(CodecError::Invalid(
            "root cannot be used as a manifest chunk".to_string(),
        )),
    }
}

fn domain_hash(domain: &[u8], bytes: &[u8]) -> Digest32 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(bytes);
    Digest32::from_bytes(*hasher.finalize().as_bytes())
}

pub fn verify_manifest_id(reference: ManifestRef, stored_bytes: &[u8]) -> CodecResult<()> {
    let actual = manifest_id(reference.kind, stored_bytes)?;
    if actual != reference.id {
        return Err(CodecError::Invalid(format!(
            "manifest {} digest mismatch: expected {}, found {}",
            reference.kind as u8, reference.id, actual
        )));
    }
    Ok(())
}

pub fn encode_root(root: &SnapshotRoot) -> CodecResult<Vec<u8>> {
    validate_root(root)?;

    let mut writer = Writer::default();
    writer.u16(PAYLOAD_SCHEMA);
    writer.raw(&root.project_id);
    match root.parent {
        None => writer.u8(0),
        Some(id) => {
            writer.u8(1);
            writer.digest(id);
        }
    }
    writer.i64(root.created.unix_seconds);
    writer.u32(root.created.nanoseconds);
    writer.string(&root.checkpoint_name)?;
    writer.string(&root.tool_version)?;
    writer.u16(TRACKED_SCOPE_POLICY);
    writer.u16(PATH_KEY_POLICY);
    writer.u16(CONTENT_POLICY);
    write_optional_manifest_ref(&mut writer, root.manifest)?;
    writer.summary(root.summary);
    envelope(ChunkKind::Root, writer.finish())
}

pub fn decode_root(stored_bytes: &[u8]) -> CodecResult<SnapshotRoot> {
    let payload = decode_envelope(stored_bytes, ChunkKind::Root)?;
    let mut reader = Reader::new(payload);
    require_schema(reader.u16()?)?;
    let project_id = reader.array()?;
    let parent = match reader.u8()? {
        0 => None,
        1 => Some(reader.digest()?),
        value => {
            return Err(CodecError::Invalid(format!(
                "unknown root parent kind {value}"
            )))
        }
    };
    let created = Timestamp::new(reader.i64()?, reader.u32()?)?;
    let checkpoint_name = reader.string("checkpoint name", MAX_CHECKPOINT_NAME_BYTES)?;
    let tool_version = reader.string("tool version", MAX_TOOL_VERSION_BYTES)?;
    require_policy("tracked scope", reader.u16()?, TRACKED_SCOPE_POLICY)?;
    require_policy("path key", reader.u16()?, PATH_KEY_POLICY)?;
    require_policy("content", reader.u16()?, CONTENT_POLICY)?;
    let manifest = read_optional_manifest_ref(&mut reader)?;
    let summary = reader.summary()?;
    reader.finish()?;
    if manifest.is_none() && summary != ManifestSummary::default() {
        return Err(CodecError::Invalid(
            "empty manifest has a non-empty summary".to_string(),
        ));
    }
    let root = SnapshotRoot {
        project_id,
        parent,
        created,
        checkpoint_name,
        tool_version,
        manifest,
        summary,
    };
    validate_root(&root)?;
    Ok(root)
}

fn validate_root(root: &SnapshotRoot) -> CodecResult<()> {
    validate_text(
        "checkpoint name",
        &root.checkpoint_name,
        MAX_CHECKPOINT_NAME_BYTES,
    )?;
    if root.checkpoint_name.trim().is_empty() || root.checkpoint_name.trim() != root.checkpoint_name
    {
        return Err(CodecError::Invalid(
            "checkpoint name must be non-empty and trimmed".to_string(),
        ));
    }
    validate_text("tool version", &root.tool_version, MAX_TOOL_VERSION_BYTES)?;
    Timestamp::new(root.created.unix_seconds, root.created.nanoseconds)?;
    if root.manifest.is_none() && root.summary != ManifestSummary::default() {
        return Err(CodecError::Invalid(
            "empty manifest must have an empty summary".to_string(),
        ));
    }
    Ok(())
}

pub fn encode_leaf(leaf: &ManifestLeaf) -> CodecResult<Vec<u8>> {
    validate_bytes("portable prefix", &leaf.portable_prefix, MAX_PREFIX_BYTES)?;
    validate_count("leaf entry count", leaf.entries.len(), MAX_LEAF_ENTRIES)?;
    let summary = leaf.summary()?;
    let mut writer = Writer::default();
    writer.u16(PAYLOAD_SCHEMA);
    writer.bytes(&leaf.portable_prefix)?;
    writer.u32(usize_to_u32("leaf entry count", leaf.entries.len())?);
    writer.summary(summary);
    for entry in &leaf.entries {
        validate_text("exact path", &entry.exact_path, MAX_EXACT_PATH_BYTES)?;
        Timestamp::new(entry.modified.unix_seconds, entry.modified.nanoseconds)?;
        writer.string(&entry.exact_path)?;
        writer.u64(entry.size_bytes);
        writer.i64(entry.modified.unix_seconds);
        writer.u32(entry.modified.nanoseconds);
        writer.digest(entry.object_id);
    }
    envelope(ChunkKind::Leaf, writer.finish())
}

pub fn decode_leaf(stored_bytes: &[u8]) -> CodecResult<ManifestLeaf> {
    let payload = decode_envelope(stored_bytes, ChunkKind::Leaf)?;
    let mut reader = Reader::new(payload);
    require_schema(reader.u16()?)?;
    let portable_prefix = reader.bytes("portable prefix", MAX_PREFIX_BYTES)?;
    let entry_count = reader.count("leaf entry count", MAX_LEAF_ENTRIES)?;
    let stored_summary = reader.summary()?;
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        entries.push(LeafEntry {
            exact_path: reader.string("exact path", MAX_EXACT_PATH_BYTES)?,
            size_bytes: reader.u64()?,
            modified: Timestamp::new(reader.i64()?, reader.u32()?)?,
            object_id: reader.digest()?,
        });
    }
    reader.finish()?;
    let leaf = ManifestLeaf {
        portable_prefix,
        entries,
    };
    if leaf.summary()? != stored_summary {
        return Err(CodecError::Invalid(
            "leaf summary does not match entries".to_string(),
        ));
    }
    Ok(leaf)
}

pub fn encode_node(node: &ManifestNode) -> CodecResult<Vec<u8>> {
    validate_bytes("portable prefix", &node.portable_prefix, MAX_PREFIX_BYTES)?;
    validate_count("node child count", node.children.len(), MAX_NODE_CHILDREN)?;
    let summary = node.summary()?;
    let mut writer = Writer::default();
    writer.u16(PAYLOAD_SCHEMA);
    writer.bytes(&node.portable_prefix)?;
    writer.u32(usize_to_u32("node child count", node.children.len())?);
    writer.summary(summary);
    for child in &node.children {
        writer.u8(child.edge);
        writer.u8(manifest_kind_byte(child.child)?);
        writer.digest(child.child.id);
        writer.summary(child.summary);
    }
    envelope(ChunkKind::Node, writer.finish())
}

pub fn decode_node(stored_bytes: &[u8]) -> CodecResult<ManifestNode> {
    let payload = decode_envelope(stored_bytes, ChunkKind::Node)?;
    let mut reader = Reader::new(payload);
    require_schema(reader.u16()?)?;
    let portable_prefix = reader.bytes("portable prefix", MAX_PREFIX_BYTES)?;
    let child_count = reader.count("node child count", MAX_NODE_CHILDREN)?;
    let stored_summary = reader.summary()?;
    let mut children = Vec::with_capacity(child_count);
    for _ in 0..child_count {
        let edge = reader.u8()?;
        let kind = read_manifest_kind(reader.u8()?)?;
        let id = reader.digest()?;
        let summary = reader.summary()?;
        children.push(ManifestChild {
            edge,
            child: ManifestRef { kind, id },
            summary,
        });
    }
    reader.finish()?;
    let node = ManifestNode {
        portable_prefix,
        children,
    };
    if node.summary()? != stored_summary {
        return Err(CodecError::Invalid(
            "node summary does not match children".to_string(),
        ));
    }
    Ok(node)
}

fn manifest_kind_byte(reference: ManifestRef) -> CodecResult<u8> {
    match reference.kind {
        ChunkKind::Node => Ok(2),
        ChunkKind::Leaf => Ok(3),
        ChunkKind::Root => Err(CodecError::Invalid(
            "manifest child cannot reference a root".to_string(),
        )),
    }
}

fn read_manifest_kind(value: u8) -> CodecResult<ChunkKind> {
    match value {
        2 => Ok(ChunkKind::Node),
        3 => Ok(ChunkKind::Leaf),
        _ => Err(CodecError::Invalid(format!(
            "unknown manifest chunk kind {value}"
        ))),
    }
}

fn write_optional_manifest_ref(
    writer: &mut Writer,
    reference: Option<ManifestRef>,
) -> CodecResult<()> {
    match reference {
        None => writer.u8(0),
        Some(reference) => {
            writer.u8(manifest_kind_byte(reference)?);
            writer.digest(reference.id);
        }
    }
    Ok(())
}

fn read_optional_manifest_ref(reader: &mut Reader<'_>) -> CodecResult<Option<ManifestRef>> {
    match reader.u8()? {
        0 => Ok(None),
        value => {
            let kind = read_manifest_kind(value)?;
            Ok(Some(ManifestRef {
                kind,
                id: reader.digest()?,
            }))
        }
    }
}

fn envelope(kind: ChunkKind, payload: Vec<u8>) -> CodecResult<Vec<u8>> {
    let stored_len = ENVELOPE_LEN
        .checked_add(payload.len())
        .ok_or(CodecError::Overflow("stored chunk length"))?;
    let limit = kind.maximum_stored_len();
    if stored_len > limit {
        return Err(CodecError::Limit {
            field: "stored chunk bytes",
            actual: stored_len,
            limit,
        });
    }
    let payload_len = usize_to_u32("payload length", payload.len())?;
    let mut stored = Vec::with_capacity(stored_len);
    stored.extend_from_slice(MAGIC);
    stored.push(kind as u8);
    stored.push(CODEC_VERSION);
    stored.extend_from_slice(&0_u16.to_be_bytes());
    stored.extend_from_slice(&payload_len.to_be_bytes());
    stored.extend_from_slice(&payload);
    Ok(stored)
}

fn decode_envelope(stored: &[u8], expected_kind: ChunkKind) -> CodecResult<&[u8]> {
    let limit = expected_kind.maximum_stored_len();
    if stored.len() > limit {
        return Err(CodecError::Limit {
            field: "stored chunk bytes",
            actual: stored.len(),
            limit,
        });
    }
    if stored.len() < ENVELOPE_LEN {
        return Err(CodecError::Invalid("truncated envelope".to_string()));
    }
    if &stored[..8] != MAGIC {
        return Err(CodecError::Invalid("invalid envelope magic".to_string()));
    }
    let found_kind = ChunkKind::parse(stored[8])?;
    if found_kind != expected_kind {
        return Err(CodecError::Invalid(format!(
            "wrong chunk kind: expected {}, found {}",
            expected_kind as u8, found_kind as u8
        )));
    }
    if stored[9] != CODEC_VERSION {
        return Err(CodecError::Invalid(format!(
            "unsupported codec version {}",
            stored[9]
        )));
    }
    let flags = u16::from_be_bytes([stored[10], stored[11]]);
    if flags != 0 {
        return Err(CodecError::Invalid(format!(
            "unsupported envelope flags {flags:#06x}"
        )));
    }
    let payload_len = u32::from_be_bytes(stored[12..16].try_into().expect("four bytes")) as usize;
    if payload_len != stored.len() - ENVELOPE_LEN {
        return Err(CodecError::Invalid(format!(
            "payload length mismatch: header {payload_len}, actual {}",
            stored.len() - ENVELOPE_LEN
        )));
    }
    Ok(&stored[ENVELOPE_LEN..])
}

fn require_schema(found: u16) -> CodecResult<()> {
    if found != PAYLOAD_SCHEMA {
        return Err(CodecError::Invalid(format!(
            "unsupported payload schema {found}"
        )));
    }
    Ok(())
}

fn require_policy(label: &'static str, found: u16, expected: u16) -> CodecResult<()> {
    if found != expected {
        return Err(CodecError::Invalid(format!(
            "unsupported {label} policy {found}"
        )));
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str, limit: usize) -> CodecResult<()> {
    validate_bytes(field, value.as_bytes(), limit)
}

fn validate_bytes(field: &'static str, value: &[u8], limit: usize) -> CodecResult<()> {
    if value.len() > limit {
        return Err(CodecError::Limit {
            field,
            actual: value.len(),
            limit,
        });
    }
    Ok(())
}

fn validate_count(field: &'static str, value: usize, limit: usize) -> CodecResult<()> {
    if value > limit {
        return Err(CodecError::Limit {
            field,
            actual: value,
            limit,
        });
    }
    Ok(())
}

fn usize_to_u32(field: &'static str, value: usize) -> CodecResult<u32> {
    u32::try_from(value).map_err(|_| CodecError::Limit {
        field,
        actual: value,
        limit: u32::MAX as usize,
    })
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.raw(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.raw(&value.to_be_bytes());
    }

    fn digest(&mut self, value: Digest32) {
        self.raw(value.as_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> CodecResult<()> {
        self.u32(usize_to_u32("byte string length", value.len())?);
        self.raw(value);
        Ok(())
    }

    fn string(&mut self, value: &str) -> CodecResult<()> {
        self.bytes(value.as_bytes())
    }

    fn summary(&mut self, summary: ManifestSummary) {
        self.u64(summary.entry_count);
        self.u64(summary.logical_size_bytes);
        self.u64(summary.total_exact_path_bytes);
    }
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take(&mut self, count: usize) -> CodecResult<&'a [u8]> {
        if self.remaining.len() < count {
            return Err(CodecError::Invalid(format!(
                "truncated payload: need {count} bytes, have {}",
                self.remaining.len()
            )));
        }
        let (taken, remaining) = self.remaining.split_at(count);
        self.remaining = remaining;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> CodecResult<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    fn u8(&mut self) -> CodecResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> CodecResult<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> CodecResult<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> CodecResult<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn i64(&mut self) -> CodecResult<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    fn digest(&mut self) -> CodecResult<Digest32> {
        Ok(Digest32::from_bytes(self.array()?))
    }

    fn bytes(&mut self, field: &'static str, limit: usize) -> CodecResult<Vec<u8>> {
        let length = self.u32()? as usize;
        if length > limit {
            return Err(CodecError::Limit {
                field,
                actual: length,
                limit,
            });
        }
        Ok(self.take(length)?.to_vec())
    }

    fn string(&mut self, field: &'static str, limit: usize) -> CodecResult<String> {
        let bytes = self.bytes(field, limit)?;
        String::from_utf8(bytes)
            .map_err(|_| CodecError::Invalid(format!("{field} is not valid UTF-8")))
    }

    fn count(&mut self, field: &'static str, limit: usize) -> CodecResult<usize> {
        let count = self.u32()? as usize;
        validate_count(field, count, limit)?;
        Ok(count)
    }

    fn summary(&mut self) -> CodecResult<ManifestSummary> {
        Ok(ManifestSummary {
            entry_count: self.u64()?,
            logical_size_bytes: self.u64()?,
            total_exact_path_bytes: self.u64()?,
        })
    }

    fn finish(self) -> CodecResult<()> {
        if !self.remaining.is_empty() {
            return Err(CodecError::Invalid(format!(
                "{} trailing payload bytes",
                self.remaining.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> Digest32 {
        Digest32::from_bytes([byte; 32])
    }

    #[test]
    fn root_round_trip_and_hash_are_stable() {
        let root = SnapshotRoot {
            project_id: [0x11; 16],
            parent: Some(digest(0x22)),
            created: Timestamp::new(1_700_000_000, 123_456_789).unwrap(),
            checkpoint_name: "checkpoint".to_string(),
            tool_version: "0.2.0".to_string(),
            manifest: Some(ManifestRef::new(ChunkKind::Leaf, digest(0x33)).unwrap()),
            summary: ManifestSummary {
                entry_count: 1,
                logical_size_bytes: 42,
                total_exact_path_bytes: 20,
            },
        };
        let bytes = encode_root(&root).unwrap();
        assert_eq!(decode_root(&bytes).unwrap(), root);
        assert_eq!(
            root_id(&bytes).to_hex(),
            "ad60da77605058dec625a4f4791420539bf0dd029d2eb863088c454a3543c8ff"
        );
    }

    #[test]
    fn leaf_round_trip_and_hash_are_stable() {
        let leaf = ManifestLeaf {
            portable_prefix: b"assets/".to_vec(),
            entries: vec![LeafEntry {
                exact_path: "Assets/Foo.asset".to_string(),
                size_bytes: 7,
                modified: Timestamp::new(-1, 999_999_999).unwrap(),
                object_id: digest(0x44),
            }],
        };
        let bytes = encode_leaf(&leaf).unwrap();
        assert_eq!(decode_leaf(&bytes).unwrap(), leaf);
        assert_eq!(
            manifest_id(ChunkKind::Leaf, &bytes).unwrap().to_hex(),
            "b6c456864cea643693eb3b4c924a6f97fc32c0c78a4c4dd852d6c7415e7c661e"
        );
    }

    #[test]
    fn node_round_trip_and_hash_are_stable() {
        let node = ManifestNode {
            portable_prefix: b"assets/".to_vec(),
            children: vec![
                ManifestChild {
                    edge: b'a',
                    child: ManifestRef::new(ChunkKind::Leaf, digest(0x55)).unwrap(),
                    summary: ManifestSummary {
                        entry_count: 1,
                        logical_size_bytes: 10,
                        total_exact_path_bytes: 15,
                    },
                },
                ManifestChild {
                    edge: b'b',
                    child: ManifestRef::new(ChunkKind::Node, digest(0x66)).unwrap(),
                    summary: ManifestSummary {
                        entry_count: 2,
                        logical_size_bytes: 20,
                        total_exact_path_bytes: 30,
                    },
                },
            ],
        };
        let bytes = encode_node(&node).unwrap();
        assert_eq!(decode_node(&bytes).unwrap(), node);
        assert_eq!(
            manifest_id(ChunkKind::Node, &bytes).unwrap().to_hex(),
            "aa4c0c78a0f6ece2d75a562b565e8cce4b3b5a1609f812d40d3de2ab24f0e6fe"
        );
    }

    #[test]
    fn envelope_rejects_trailing_bytes_flags_and_wrong_kind() {
        let leaf = ManifestLeaf {
            portable_prefix: Vec::new(),
            entries: Vec::new(),
        };
        let bytes = encode_leaf(&leaf).unwrap();

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_leaf(&trailing).is_err());

        let mut flags = bytes.clone();
        flags[11] = 1;
        assert!(decode_leaf(&flags).is_err());

        assert!(decode_node(&bytes).is_err());
    }

    #[test]
    fn root_hash_verification_fails_closed() {
        let root = SnapshotRoot {
            project_id: [0; 16],
            parent: None,
            created: Timestamp::new(0, 0).unwrap(),
            checkpoint_name: "checkpoint".to_string(),
            tool_version: "0.2.0".to_string(),
            manifest: None,
            summary: ManifestSummary::default(),
        };
        let bytes = encode_root(&root).unwrap();
        verify_root_id(root_id(&bytes), &bytes).unwrap();
        assert!(verify_root_id(Digest32::from_bytes([0; 32]), &bytes).is_err());
    }

    #[test]
    fn root_rejects_empty_or_untrimmed_checkpoint_names() {
        let mut root = SnapshotRoot {
            project_id: [0; 16],
            parent: None,
            created: Timestamp::new(0, 0).unwrap(),
            checkpoint_name: "checkpoint".to_string(),
            tool_version: "0.2.0".to_string(),
            manifest: None,
            summary: ManifestSummary::default(),
        };
        root.checkpoint_name.clear();
        assert!(encode_root(&root).is_err());
        root.checkpoint_name = " checkpoint".to_string();
        assert!(encode_root(&root).is_err());
    }

    #[test]
    fn decoder_checks_summary_and_allocation_lengths() {
        let leaf = ManifestLeaf {
            portable_prefix: Vec::new(),
            entries: Vec::new(),
        };
        let mut bytes = encode_leaf(&leaf).unwrap();
        // entryCount is after schema and the length-prefixed empty prefix.
        bytes[22..26].copy_from_slice(&(MAX_LEAF_ENTRIES as u32 + 1).to_be_bytes());
        assert!(matches!(decode_leaf(&bytes), Err(CodecError::Limit { .. })));
    }
}
