//! Canonical compressed-radix manifest for snapshot v2.
//!
//! The initial writer rebuilds the full tree in O(N), but stores every node and
//! leaf by content ID. Unchanged chunks therefore remain shared across roots.

use super::merkle_codec::{
    decode_leaf, decode_node, encode_leaf, encode_node, manifest_id, verify_manifest_id, ChunkKind,
    CodecError, Digest32, LeafEntry, ManifestChild, ManifestLeaf, ManifestNode, ManifestRef,
    ManifestSummary, MAX_EXACT_PATH_BYTES,
};
use crate::TrackedUnityFilePath;
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

/// Persistent v2 format constant. Changing it requires a new path-key policy.
pub const PATH_KEY_UNICODE_VERSION: (u8, u8, u8) = (16, 0, 0);
/// Frozen for repository format v5 after 8/16/32/64 KiB real-project comparison.
pub const MANIFEST_LEAF_TARGET_BYTES: usize = 32 * 1024;
/// Bounds adversarial trees independently of the 128 KiB exact-path limit.
pub const MAX_MANIFEST_DEPTH: usize = 1024;
/// Hard upper bound for one validation traversal and its reusable cache.
pub const MAX_MANIFEST_VALIDATION_CHUNKS: usize = 100_000;
/// Hard upper bound for estimated heap retained by one validation cache.
pub const MAX_MANIFEST_VALIDATION_CACHE_BYTES: usize = 64 * 1024 * 1024;

const _: () = {
    let normal = unicode_normalization::UNICODE_VERSION;
    assert!(normal.0 == PATH_KEY_UNICODE_VERSION.0);
    assert!(normal.1 == PATH_KEY_UNICODE_VERSION.1);
    assert!(normal.2 == PATH_KEY_UNICODE_VERSION.2);
    let mapping = unicode_case_mapping::UNICODE_VERSION;
    assert!(mapping.0 == PATH_KEY_UNICODE_VERSION.0 as u64);
    assert!(mapping.1 == PATH_KEY_UNICODE_VERSION.1 as u64);
    assert!(mapping.2 == PATH_KEY_UNICODE_VERSION.2 as u64);
};

#[derive(Debug, thiserror::Error)]
pub enum SnapshotV2Error {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("invalid snapshot v2 manifest: {0}")]
    Invalid(String),
    #[error("snapshot v2 manifest source: {0}")]
    Source(String),
}

pub type SnapshotV2Result<T> = std::result::Result<T, SnapshotV2Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub exact_path: String,
    pub size_bytes: u64,
    pub modified: super::merkle_codec::Timestamp,
    pub object_id: Digest32,
}

impl From<LeafEntry> for ManifestEntry {
    fn from(entry: LeafEntry) -> Self {
        Self {
            exact_path: entry.exact_path,
            size_bytes: entry.size_bytes,
            modified: entry.modified,
            object_id: entry.object_id,
        }
    }
}

impl From<ManifestEntry> for LeafEntry {
    fn from(entry: ManifestEntry) -> Self {
        Self {
            exact_path: entry.exact_path,
            size_bytes: entry.size_bytes,
            modified: entry.modified,
            object_id: entry.object_id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuiltManifest {
    pub root: Option<ManifestRef>,
    pub summary: ManifestSummary,
    /// Canonical stored envelope bytes, keyed by domain-separated chunk ID.
    pub chunks: BTreeMap<ManifestRef, Vec<u8>>,
}

pub trait ManifestChunkSource {
    fn load_manifest_chunk(&self, reference: ManifestRef) -> SnapshotV2Result<Vec<u8>>;
}

impl ManifestChunkSource for BuiltManifest {
    fn load_manifest_chunk(&self, reference: ManifestRef) -> SnapshotV2Result<Vec<u8>> {
        self.chunks.get(&reference).cloned().ok_or_else(|| {
            SnapshotV2Error::Source(format!(
                "missing {:?} chunk {}",
                reference.kind, reference.id
            ))
        })
    }
}

/// Frozen Unicode 16 path policy: NFC, locale-independent full lowercase, NFC.
pub fn portable_path_key_v1(exact_path: &str) -> SnapshotV2Result<Vec<u8>> {
    if exact_path.len() > MAX_EXACT_PATH_BYTES {
        return Err(CodecError::Limit {
            field: "exact path",
            actual: exact_path.len(),
            limit: MAX_EXACT_PATH_BYTES,
        }
        .into());
    }
    TrackedUnityFilePath::parse(exact_path)
        .map_err(|error| SnapshotV2Error::Invalid(error.to_string()))?;

    let normalized = exact_path.nfc().collect::<String>();
    let mut lowered = String::with_capacity(normalized.len());
    for character in normalized.chars() {
        let mapping = unicode_case_mapping::to_lowercase(character);
        if mapping == [0, 0] {
            lowered.push(character);
            continue;
        }
        for scalar in mapping.into_iter().take_while(|scalar| *scalar != 0) {
            let mapped = char::from_u32(scalar).ok_or_else(|| {
                SnapshotV2Error::Invalid(format!(
                    "Unicode 16 lowercase table returned invalid scalar U+{scalar:04X}"
                ))
            })?;
            lowered.push(mapped);
        }
    }
    let key = lowered.nfc().collect::<String>().into_bytes();
    if key.len() > MAX_EXACT_PATH_BYTES {
        return Err(CodecError::Limit {
            field: "portable path key",
            actual: key.len(),
            limit: MAX_EXACT_PATH_BYTES,
        }
        .into());
    }
    Ok(key)
}

pub fn build_manifest(entries: Vec<ManifestEntry>) -> SnapshotV2Result<BuiltManifest> {
    build_manifest_with_leaf_target(entries, MANIFEST_LEAF_TARGET_BYTES)
}

fn build_manifest_with_leaf_target(
    entries: Vec<ManifestEntry>,
    leaf_target_bytes: usize,
) -> SnapshotV2Result<BuiltManifest> {
    if entries.is_empty() {
        return Ok(BuiltManifest::default());
    }

    let mut keyed = entries
        .into_iter()
        .map(|entry| {
            let key = portable_path_key_v1(&entry.exact_path)?;
            Ok(KeyedEntry { key, entry })
        })
        .collect::<SnapshotV2Result<Vec<_>>>()?;
    keyed.sort_by(|left, right| left.key.cmp(&right.key));
    validate_ordered_keys(&keyed)?;

    let summary = keyed
        .iter()
        .try_fold(ManifestSummary::default(), |summary, keyed_entry| {
            summary
                .checked_add(entry_summary(&keyed_entry.entry)?)
                .map_err(SnapshotV2Error::from)
        })?;
    let mut chunks = BTreeMap::new();
    let root = build_range(&keyed, 0, leaf_target_bytes, &mut chunks)?;
    Ok(BuiltManifest {
        root: Some(root),
        summary,
        chunks,
    })
}

#[derive(Debug)]
struct KeyedEntry {
    key: Vec<u8>,
    entry: ManifestEntry,
}

fn build_range(
    entries: &[KeyedEntry],
    depth: usize,
    leaf_target_bytes: usize,
    chunks: &mut BTreeMap<ManifestRef, Vec<u8>>,
) -> SnapshotV2Result<ManifestRef> {
    if depth > MAX_MANIFEST_DEPTH {
        return Err(SnapshotV2Error::Invalid(format!(
            "manifest exceeds maximum depth {MAX_MANIFEST_DEPTH}"
        )));
    }
    let first = entries
        .first()
        .ok_or_else(|| SnapshotV2Error::Invalid("cannot build an empty chunk".to_string()))?;
    let last = entries.last().expect("non-empty checked");
    let prefix_len = common_prefix_len(&first.key, &last.key);
    let prefix = first.key[..prefix_len].to_vec();
    let summary = entries
        .iter()
        .try_fold(ManifestSummary::default(), |summary, keyed| {
            summary
                .checked_add(entry_summary(&keyed.entry)?)
                .map_err(SnapshotV2Error::from)
        })?;
    if leaf_encoded_len(&prefix, summary)? <= leaf_target_bytes || entries.len() == 1 {
        let leaf_bytes = encode_leaf(&ManifestLeaf {
            portable_prefix: prefix.clone(),
            entries: entries
                .iter()
                .map(|keyed| keyed.entry.clone().into())
                .collect(),
        })?;
        return insert_chunk(ChunkKind::Leaf, leaf_bytes, chunks);
    }

    let mut children = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let edge = edge_at(&entries[start].key, prefix_len);
        let mut end = start + 1;
        while end < entries.len() && edge_at(&entries[end].key, prefix_len) == edge {
            end += 1;
        }
        let child = build_range(&entries[start..end], depth + 1, leaf_target_bytes, chunks)?;
        let child_summary =
            entries[start..end]
                .iter()
                .try_fold(ManifestSummary::default(), |summary, keyed| {
                    summary
                        .checked_add(entry_summary(&keyed.entry)?)
                        .map_err(SnapshotV2Error::from)
                })?;
        children.push(ManifestChild {
            edge,
            child,
            summary: child_summary,
        });
        start = end;
    }
    if children.len() < 2 {
        return Err(SnapshotV2Error::Invalid(
            "canonical radix split produced a unary node".to_string(),
        ));
    }
    let node_bytes = encode_node(&ManifestNode {
        portable_prefix: prefix,
        children,
    })?;
    insert_chunk(ChunkKind::Node, node_bytes, chunks)
}

fn insert_chunk(
    kind: ChunkKind,
    bytes: Vec<u8>,
    chunks: &mut BTreeMap<ManifestRef, Vec<u8>>,
) -> SnapshotV2Result<ManifestRef> {
    let reference = ManifestRef::new(kind, manifest_id(kind, &bytes)?)?;
    if let Some(existing) = chunks.get(&reference) {
        if existing != &bytes {
            return Err(SnapshotV2Error::Invalid(format!(
                "domain hash collision for {:?} {}",
                kind, reference.id
            )));
        }
    } else {
        chunks.insert(reference, bytes);
    }
    Ok(reference)
}

fn entry_summary(entry: &ManifestEntry) -> SnapshotV2Result<ManifestSummary> {
    Ok(ManifestSummary {
        entry_count: 1,
        logical_size_bytes: entry.size_bytes,
        total_exact_path_bytes: u64::try_from(entry.exact_path.len())
            .map_err(|_| SnapshotV2Error::Invalid("exact path length overflow".to_string()))?,
    })
}

fn validate_ordered_keys(entries: &[KeyedEntry]) -> SnapshotV2Result<()> {
    for pair in entries.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.key == current.key {
            return Err(SnapshotV2Error::Invalid(format!(
                "portable path collision between {:?} and {:?}",
                previous.entry.exact_path, current.entry.exact_path
            )));
        }
        if previous.key > current.key {
            return Err(SnapshotV2Error::Invalid(format!(
                "portable path keys are not strictly increasing: {:?} before {:?}",
                previous.entry.exact_path, current.entry.exact_path
            )));
        }
        if is_path_ancestor(&previous.key, &current.key) {
            return Err(SnapshotV2Error::Invalid(format!(
                "file/directory ancestor conflict between {:?} and {:?}",
                previous.entry.exact_path, current.entry.exact_path
            )));
        }
    }
    Ok(())
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn edge_at(key: &[u8], prefix_len: usize) -> u8 {
    key.get(prefix_len).copied().unwrap_or(0)
}

fn is_path_ancestor(parent: &[u8], child: &[u8]) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && child.get(parent.len()).copied() == Some(b'/')
}

pub(crate) enum DecodedChunk {
    Node(ManifestNode, Vec<u8>),
    Leaf(ManifestLeaf, Vec<u8>),
}

pub(crate) fn load_chunk<S: ManifestChunkSource>(
    source: &S,
    reference: ManifestRef,
) -> SnapshotV2Result<DecodedChunk> {
    let bytes = source.load_manifest_chunk(reference)?;
    verify_manifest_id(reference, &bytes)?;
    match reference.kind {
        ChunkKind::Node => Ok(DecodedChunk::Node(decode_node(&bytes)?, bytes)),
        ChunkKind::Leaf => Ok(DecodedChunk::Leaf(decode_leaf(&bytes)?, bytes)),
        ChunkKind::Root => Err(SnapshotV2Error::Invalid(
            "manifest points to a snapshot root".to_string(),
        )),
    }
}

/// Fully verifies hash, summaries, ordering, ranges and the unique canonical shape.
#[cfg(test)]
pub fn validate_manifest<S: ManifestChunkSource>(
    source: &S,
    root: Option<ManifestRef>,
    expected_summary: ManifestSummary,
) -> SnapshotV2Result<()> {
    validate_manifest_cached(
        source,
        root,
        expected_summary,
        &mut ManifestValidationCache::default(),
    )
}

#[derive(Default)]
pub struct ManifestValidationCache {
    validated: BTreeMap<ManifestRef, ValidationInfo>,
    estimated_heap_bytes: usize,
}

impl ManifestValidationCache {
    pub(crate) fn len(&self) -> usize {
        self.validated.len()
    }

    pub(crate) fn estimated_heap_bytes(&self) -> usize {
        self.estimated_heap_bytes
    }

    pub(crate) fn validated_references(&self) -> BTreeSet<ManifestRef> {
        self.validated.keys().copied().collect()
    }
}

/// Validates a root while reusing already verified immutable subtrees.
///
/// A fresh per-root visited set catches repeated direct references. Cached
/// descendant overlap remains invalid because each parent rechecks child key
/// ranges using the cached first/last keys.
pub fn validate_manifest_cached<S: ManifestChunkSource>(
    source: &S,
    root: Option<ManifestRef>,
    expected_summary: ManifestSummary,
    cache: &mut ManifestValidationCache,
) -> SnapshotV2Result<()> {
    let Some(root) = root else {
        if expected_summary != ManifestSummary::default() {
            return Err(SnapshotV2Error::Invalid(
                "empty root has a non-empty summary".to_string(),
            ));
        }
        return Ok(());
    };
    let mut visited = BTreeSet::new();
    let info = validate_subtree(source, root, 0, &mut visited, cache)?;
    if info.summary != expected_summary {
        return Err(SnapshotV2Error::Invalid(
            "root manifest summary mismatch".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct ValidationInfo {
    summary: ManifestSummary,
    first_key: Vec<u8>,
    last_key: Vec<u8>,
    maximum_relative_depth: usize,
}

fn validate_subtree<S: ManifestChunkSource>(
    source: &S,
    reference: ManifestRef,
    depth: usize,
    visited: &mut BTreeSet<ManifestRef>,
    cache: &mut ManifestValidationCache,
) -> SnapshotV2Result<ValidationInfo> {
    if depth > MAX_MANIFEST_DEPTH {
        return Err(SnapshotV2Error::Invalid(format!(
            "manifest exceeds maximum depth {MAX_MANIFEST_DEPTH}"
        )));
    }
    if visited.len() >= MAX_MANIFEST_VALIDATION_CHUNKS {
        return Err(SnapshotV2Error::Invalid(format!(
            "manifest exceeds maximum validated chunk count {MAX_MANIFEST_VALIDATION_CHUNKS}"
        )));
    }
    if !visited.insert(reference) {
        return Err(SnapshotV2Error::Invalid(format!(
            "cycle or duplicate manifest range at {:?} {}",
            reference.kind, reference.id
        )));
    }
    if let Some(info) = cache.validated.get(&reference) {
        if depth.saturating_add(info.maximum_relative_depth) > MAX_MANIFEST_DEPTH {
            return Err(SnapshotV2Error::Invalid(format!(
                "manifest exceeds maximum depth {MAX_MANIFEST_DEPTH}"
            )));
        }
        return Ok(info.clone());
    }
    let info = match load_chunk(source, reference)? {
        DecodedChunk::Leaf(leaf, stored_bytes) => validate_leaf(leaf, &stored_bytes),
        DecodedChunk::Node(node, stored_bytes) => {
            // Round-trip equality is a defense against future permissive decoder changes.
            if encode_node(&node)? != stored_bytes {
                return Err(SnapshotV2Error::Invalid(
                    "node has a non-canonical binary encoding".to_string(),
                ));
            }
            if node.children.len() < 2 {
                return Err(SnapshotV2Error::Invalid(
                    "canonical node must contain at least two children".to_string(),
                ));
            }
            for pair in node.children.windows(2) {
                if pair[0].edge >= pair[1].edge {
                    return Err(SnapshotV2Error::Invalid(
                        "node edges are not strictly increasing".to_string(),
                    ));
                }
            }

            let mut first_key = None;
            let mut last_key: Option<Vec<u8>> = None;
            let mut summary = ManifestSummary::default();
            let mut maximum_relative_depth = 0_usize;
            for child in &node.children {
                let child_info = validate_subtree(source, child.child, depth + 1, visited, cache)?;
                if child_info.summary != child.summary {
                    return Err(SnapshotV2Error::Invalid(
                        "node child summary mismatch".to_string(),
                    ));
                }
                if !child_info.first_key.starts_with(&node.portable_prefix)
                    || edge_at(&child_info.first_key, node.portable_prefix.len()) != child.edge
                    || edge_at(&child_info.last_key, node.portable_prefix.len()) != child.edge
                {
                    return Err(SnapshotV2Error::Invalid(
                        "child key range does not match its radix edge".to_string(),
                    ));
                }
                if let Some(previous) = &last_key {
                    if previous >= &child_info.first_key {
                        return Err(SnapshotV2Error::Invalid(
                            "manifest child ranges overlap or are unordered".to_string(),
                        ));
                    }
                    if is_path_ancestor(previous, &child_info.first_key) {
                        return Err(SnapshotV2Error::Invalid(
                            "manifest contains a file/directory ancestor conflict".to_string(),
                        ));
                    }
                }
                first_key.get_or_insert_with(|| child_info.first_key.clone());
                last_key = Some(child_info.last_key);
                summary = summary.checked_add(child_info.summary)?;
                maximum_relative_depth =
                    maximum_relative_depth.max(child_info.maximum_relative_depth.saturating_add(1));
            }
            let first_key = first_key.expect("node has at least two children");
            let last_key = last_key.expect("node has at least two children");
            let canonical_prefix_len = common_prefix_len(&first_key, &last_key);
            if node.portable_prefix != first_key[..canonical_prefix_len] {
                return Err(SnapshotV2Error::Invalid(
                    "node prefix is not the exact descendant LCP".to_string(),
                ));
            }
            if leaf_encoded_len(&node.portable_prefix, summary)? <= MANIFEST_LEAF_TARGET_BYTES {
                return Err(SnapshotV2Error::Invalid(
                    "node is non-canonical because it can be flattened into one leaf".to_string(),
                ));
            }
            Ok(ValidationInfo {
                summary,
                first_key,
                last_key,
                maximum_relative_depth,
            })
        }
    }?;
    let estimated_bytes = std::mem::size_of::<ManifestRef>()
        .saturating_add(std::mem::size_of::<ValidationInfo>())
        .saturating_add(info.first_key.capacity())
        .saturating_add(info.last_key.capacity())
        // Approximate BTree node/link allocation without relying on its
        // private implementation. The byte limit is a guardrail, not an ABI.
        .saturating_add(64);
    if !cache.validated.contains_key(&reference) {
        if cache.validated.len() >= MAX_MANIFEST_VALIDATION_CHUNKS {
            return Err(SnapshotV2Error::Invalid(format!(
                "manifest validation cache exceeds {MAX_MANIFEST_VALIDATION_CHUNKS} chunks"
            )));
        }
        if cache.estimated_heap_bytes.saturating_add(estimated_bytes)
            > MAX_MANIFEST_VALIDATION_CACHE_BYTES
        {
            return Err(SnapshotV2Error::Invalid(format!(
                "manifest validation cache exceeds {} bytes",
                MAX_MANIFEST_VALIDATION_CACHE_BYTES
            )));
        }
        cache.validated.insert(reference, info.clone());
        cache.estimated_heap_bytes = cache.estimated_heap_bytes.saturating_add(estimated_bytes);
    }
    Ok(info)
}

fn validate_leaf(leaf: ManifestLeaf, stored_bytes: &[u8]) -> SnapshotV2Result<ValidationInfo> {
    let canonical_bytes = encode_leaf(&leaf)?;
    if canonical_bytes != stored_bytes {
        return Err(SnapshotV2Error::Invalid(
            "leaf has a non-canonical binary encoding".to_string(),
        ));
    }
    if leaf.entries.is_empty() {
        return Err(SnapshotV2Error::Invalid(
            "empty manifests must not contain an empty leaf".to_string(),
        ));
    }
    if stored_bytes.len() > MANIFEST_LEAF_TARGET_BYTES && leaf.entries.len() != 1 {
        return Err(SnapshotV2Error::Invalid(format!(
            "leaf exceeds {MANIFEST_LEAF_TARGET_BYTES} byte target"
        )));
    }
    let keyed = leaf
        .entries
        .iter()
        .cloned()
        .map(|entry| {
            let key = portable_path_key_v1(&entry.exact_path)?;
            Ok(KeyedEntry {
                key,
                entry: entry.into(),
            })
        })
        .collect::<SnapshotV2Result<Vec<_>>>()?;
    validate_ordered_keys(&keyed)?;
    let first_key = keyed.first().expect("non-empty checked").key.clone();
    let last_key = keyed.last().expect("non-empty checked").key.clone();
    let canonical_prefix_len = common_prefix_len(&first_key, &last_key);
    if leaf.portable_prefix != first_key[..canonical_prefix_len] {
        return Err(SnapshotV2Error::Invalid(
            "leaf prefix is not the exact entry LCP".to_string(),
        ));
    }
    Ok(ValidationInfo {
        summary: leaf.summary()?,
        first_key,
        last_key,
        maximum_relative_depth: 0,
    })
}

fn leaf_encoded_len(prefix: &[u8], summary: ManifestSummary) -> SnapshotV2Result<usize> {
    // envelope + schema + prefix length + count + summary + fixed entry fields + paths
    let fixed = 16_usize
        .checked_add(2 + 4 + prefix.len() + 4 + 24)
        .ok_or_else(|| SnapshotV2Error::Invalid("leaf length overflow".to_string()))?;
    let entry_count = usize::try_from(summary.entry_count)
        .map_err(|_| SnapshotV2Error::Invalid("entry count overflow".to_string()))?;
    let paths = usize::try_from(summary.total_exact_path_bytes)
        .map_err(|_| SnapshotV2Error::Invalid("path byte count overflow".to_string()))?;
    fixed
        .checked_add(
            entry_count
                .checked_mul(56)
                .ok_or_else(|| SnapshotV2Error::Invalid("leaf length overflow".to_string()))?,
        )
        .and_then(|value| value.checked_add(paths))
        .ok_or_else(|| SnapshotV2Error::Invalid("leaf length overflow".to_string()))
}

pub struct ManifestIter<'a, S> {
    source: &'a S,
    pending: Vec<ManifestRef>,
    visited: BTreeSet<ManifestRef>,
    leaf_entries: std::vec::IntoIter<LeafEntry>,
    failed: bool,
}

/// Iterates a previously validated manifest in portable-path order.
///
/// Chunk IDs are still verified while streaming, so post-validation mutation
/// fails closed. Call [`validate_manifest`] once before consuming an untrusted
/// repository root to establish the semantic ordering invariant.
pub fn manifest_iter<'a, S: ManifestChunkSource>(
    source: &'a S,
    root: Option<ManifestRef>,
) -> ManifestIter<'a, S> {
    ManifestIter {
        source,
        pending: root.into_iter().collect(),
        visited: BTreeSet::new(),
        leaf_entries: Vec::new().into_iter(),
        failed: false,
    }
}

impl<S: ManifestChunkSource> Iterator for ManifestIter<'_, S> {
    type Item = SnapshotV2Result<ManifestEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(entry) = self.leaf_entries.next() {
                return Some(Ok(entry.into()));
            }
            let reference = self.pending.pop()?;
            if !self.visited.insert(reference) {
                self.failed = true;
                return Some(Err(SnapshotV2Error::Invalid(format!(
                    "cycle or duplicate manifest range at {:?} {}",
                    reference.kind, reference.id
                ))));
            }
            match load_chunk(self.source, reference) {
                Ok(DecodedChunk::Leaf(leaf, _)) => {
                    self.leaf_entries = leaf.entries.into_iter();
                }
                Ok(DecodedChunk::Node(node, _)) => {
                    self.pending
                        .extend(node.children.into_iter().rev().map(|child| child.child));
                }
                Err(error) => {
                    self.failed = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

#[allow(dead_code)] // Kept as the bounded lookup primitive for streaming diff/restore work.
pub fn lookup_manifest_entry<S: ManifestChunkSource>(
    source: &S,
    root: Option<ManifestRef>,
    exact_path: &str,
) -> SnapshotV2Result<Option<ManifestEntry>> {
    let key = portable_path_key_v1(exact_path)?;
    let Some(mut reference) = root else {
        return Ok(None);
    };
    for _ in 0..=MAX_MANIFEST_DEPTH {
        match load_chunk(source, reference)? {
            DecodedChunk::Leaf(leaf, _) => {
                let mut low = 0;
                let mut high = leaf.entries.len();
                while low < high {
                    let middle = low + (high - low) / 2;
                    let stored_key = portable_path_key_v1(&leaf.entries[middle].exact_path)?;
                    match stored_key.cmp(&key) {
                        std::cmp::Ordering::Less => low = middle + 1,
                        std::cmp::Ordering::Greater => high = middle,
                        std::cmp::Ordering::Equal => {
                            return Ok(Some(leaf.entries[middle].clone().into()))
                        }
                    }
                }
                return Ok(None);
            }
            DecodedChunk::Node(node, _) => {
                if !key.starts_with(&node.portable_prefix) {
                    return Ok(None);
                }
                let edge = edge_at(&key, node.portable_prefix.len());
                let Ok(index) = node
                    .children
                    .binary_search_by_key(&edge, |child| child.edge)
                else {
                    return Ok(None);
                };
                reference = node.children[index].child;
            }
        }
    }
    Err(SnapshotV2Error::Invalid(format!(
        "manifest exceeds maximum depth {MAX_MANIFEST_DEPTH}"
    )))
}

#[cfg(test)]
mod tests {
    use super::super::merkle_codec::Timestamp;
    use super::*;

    fn entry(path: impl Into<String>, seed: u8) -> ManifestEntry {
        ManifestEntry {
            exact_path: path.into(),
            size_bytes: seed as u64 + 1,
            modified: Timestamp::new(seed as i64, seed as u32).unwrap(),
            object_id: Digest32::from_bytes([seed; 32]),
        }
    }

    fn large_entries() -> Vec<ManifestEntry> {
        (0..900)
            .map(|index| {
                entry(
                    format!("Assets/Group{:02}/File{index:05}.asset", index % 37),
                    index as u8,
                )
            })
            .collect()
    }

    #[test]
    fn unicode_16_path_key_is_frozen_and_normalized() {
        assert_eq!(unicode_normalization::UNICODE_VERSION, (16, 0, 0));
        assert_eq!(unicode_case_mapping::UNICODE_VERSION, (16, 0, 0));
        assert_eq!(
            portable_path_key_v1("Assets/\u{0130}/Cafe\u{301}.asset").unwrap(),
            "assets/i\u{307}/caf\u{e9}.asset".as_bytes()
        );
    }

    #[test]
    fn builder_is_independent_of_input_order_and_validates() {
        let entries = large_entries();
        let first = build_manifest(entries.clone()).unwrap();
        let mut reversed = entries;
        reversed.reverse();
        let second = build_manifest(reversed).unwrap();

        assert_eq!(first.root, second.root);
        assert_eq!(first.summary, second.summary);
        assert_eq!(first.chunks, second.chunks);
        assert!(first.chunks.len() > 1);
        validate_manifest(&first, first.root, first.summary).unwrap();
    }

    #[test]
    fn canonical_root_is_stable_across_many_deterministic_permutations() {
        let entries = (0..120)
            .map(|index| {
                entry(
                    format!("Assets/P{:02}/F{index:04}.asset", index % 19),
                    index as u8,
                )
            })
            .collect::<Vec<_>>();
        let expected = build_manifest(entries.clone()).unwrap().root;
        for round in 1..32 {
            let mut permuted = entries.clone();
            let length = permuted.len();
            for index in 0..length {
                let other = (index * 73 + round * 41) % length;
                permuted.swap(index, other);
            }
            assert_eq!(build_manifest(permuted).unwrap().root, expected);
        }
    }

    #[test]
    fn builder_splits_thirty_thousand_entries_before_leaf_allocation_limits() {
        let entries = (0..30_000)
            .map(|index| {
                entry(
                    format!("Assets/G{:03}/F{index:05}.asset", index % 521),
                    index as u8,
                )
            })
            .collect();
        let built = build_manifest(entries).unwrap();
        assert_eq!(built.summary.entry_count, 30_000);
        assert!(matches!(
            built.root,
            Some(ManifestRef {
                kind: ChunkKind::Node,
                ..
            })
        ));
        validate_manifest(&built, built.root, built.summary).unwrap();
    }

    #[test]
    #[ignore = "requires CHECKPO_BENCH_PROJECT and its isolated CHECKPO_DATA_DIR"]
    fn compare_real_project_leaf_targets_and_change_density() {
        let project_path = std::env::var("CHECKPO_BENCH_PROJECT").unwrap();
        let project = crate::load_project(&project_path).unwrap();
        let latest = crate::read_latest_snapshot_id(&project.repo_root)
            .unwrap()
            .unwrap();
        let snapshot = crate::load_project_snapshot(&project, &latest).unwrap();
        let entries = snapshot
            .files
            .into_iter()
            .map(|file| {
                let modified = chrono::DateTime::parse_from_rfc3339(&file.modified_at_utc).unwrap();
                ManifestEntry {
                    exact_path: file.path.to_string(),
                    size_bytes: file.size_bytes,
                    modified: Timestamp::new(
                        modified.timestamp(),
                        modified.timestamp_subsec_nanos(),
                    )
                    .unwrap(),
                    object_id: Digest32::from_bytes(file.content_hash().digest_bytes()),
                }
            })
            .collect::<Vec<_>>();

        for target in [8, 16, 32, 64].map(|kib| kib * 1024) {
            let before = build_manifest_with_leaf_target(entries.clone(), target).unwrap();
            let before_ids = before.chunks.keys().copied().collect::<BTreeSet<_>>();
            let node_count = before
                .chunks
                .keys()
                .filter(|reference| reference.kind == ChunkKind::Node)
                .count();
            let leaf_count = before.chunks.len() - node_count;
            let stored_bytes = before.chunks.values().map(Vec::len).sum::<usize>();
            let maximum_bytes = before.chunks.values().map(Vec::len).max().unwrap_or(0);
            let mut churn = Vec::new();
            for changed_count in [1, 30, 300] {
                let mut changed = entries.clone();
                for index in 0..changed_count.min(changed.len()) {
                    let selected = index * changed.len() / changed_count;
                    let mut digest = *changed[selected].object_id.as_bytes();
                    digest[0] ^= 0x80;
                    digest[31] ^= (index as u8).wrapping_add(1);
                    changed[selected].object_id = Digest32::from_bytes(digest);
                }
                let after = build_manifest_with_leaf_target(changed, target).unwrap();
                let created_chunks = after
                    .chunks
                    .keys()
                    .filter(|reference| !before_ids.contains(reference))
                    .count();
                let created_bytes = after
                    .chunks
                    .iter()
                    .filter(|(reference, _)| !before_ids.contains(reference))
                    .map(|(_, bytes)| bytes.len())
                    .sum::<usize>();
                churn.push((changed_count, created_chunks, created_bytes));
            }
            eprintln!(
                "leaf_target={}KiB entries={} chunks={} nodes={} leaves={} stored_bytes={} max_chunk_bytes={} churn={churn:?}",
                target / 1024,
                entries.len(),
                before.chunks.len(),
                node_count,
                leaf_count,
                stored_bytes,
                maximum_bytes,
            );
        }
    }

    #[test]
    fn iterator_is_sorted_and_lookup_descends_without_materializing_manifest() {
        let built = build_manifest(large_entries()).unwrap();
        validate_manifest(&built, built.root, built.summary).unwrap();
        let paths = manifest_iter(&built, built.root)
            .map(|entry| entry.unwrap().exact_path)
            .collect::<Vec<_>>();
        let keys = paths
            .iter()
            .map(|path| portable_path_key_v1(path).unwrap())
            .collect::<Vec<_>>();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));

        let found = lookup_manifest_entry(&built, built.root, "Assets/Group05/File00005.asset")
            .unwrap()
            .unwrap();
        assert_eq!(found.exact_path, "Assets/Group05/File00005.asset");
        assert!(
            lookup_manifest_entry(&built, built.root, "Assets/Missing.asset")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_portable_collision_and_ancestor_conflict() {
        let collision = build_manifest(vec![
            entry("Assets/Foo.asset", 1),
            entry("Assets/foo.asset", 2),
        ]);
        assert!(collision.unwrap_err().to_string().contains("collision"));

        let ancestor = build_manifest(vec![
            entry("Assets/Foo", 1),
            entry("Assets/Foo/Bar.asset", 2),
        ]);
        assert!(ancestor.unwrap_err().to_string().contains("ancestor"));
    }

    #[test]
    fn validation_rejects_a_leaf_with_descending_portable_keys() {
        let leaf = ManifestLeaf {
            portable_prefix: b"assets/".to_vec(),
            entries: vec![
                entry("Assets/B.asset", 1).into(),
                entry("Assets/A.asset", 2).into(),
            ],
        };
        let bytes = encode_leaf(&leaf).unwrap();
        let reference = ManifestRef::new(
            ChunkKind::Leaf,
            manifest_id(ChunkKind::Leaf, &bytes).unwrap(),
        )
        .unwrap();
        let summary = leaf.summary().unwrap();
        let built = BuiltManifest {
            root: Some(reference),
            summary,
            chunks: BTreeMap::from([(reference, bytes)]),
        };

        assert!(validate_manifest(&built, built.root, built.summary)
            .unwrap_err()
            .to_string()
            .contains("strictly increasing"));
    }

    #[test]
    fn a_local_content_change_reuses_unaffected_chunks() {
        let entries = large_entries();
        let before = build_manifest(entries.clone()).unwrap();
        let mut after_entries = entries;
        after_entries[321].object_id = Digest32::from_bytes([0xEE; 32]);
        let after = build_manifest(after_entries).unwrap();
        assert_ne!(before.root, after.root);

        let before_ids = before.chunks.keys().copied().collect::<BTreeSet<_>>();
        let after_ids = after.chunks.keys().copied().collect::<BTreeSet<_>>();
        assert!(before_ids.intersection(&after_ids).count() > 0);
    }

    #[test]
    fn hash_tampering_fails_closed() {
        let mut built = build_manifest(large_entries()).unwrap();
        let target = *built.chunks.keys().next().unwrap();
        built.chunks.get_mut(&target).unwrap()[20] ^= 1;
        assert!(validate_manifest(&built, built.root, built.summary)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch"));
    }

    #[test]
    fn empty_manifest_is_canonical() {
        let built = build_manifest(Vec::new()).unwrap();
        assert!(built.root.is_none());
        assert!(built.chunks.is_empty());
        validate_manifest(&built, built.root, built.summary).unwrap();
        assert_eq!(manifest_iter(&built, built.root).count(), 0);
    }
}
