use super::merkle_codec::{manifest_id, root_id, ChunkKind, ManifestRef, MAX_MANIFEST_CHUNK_BYTES};
use super::snapshot_v2::{BuiltManifest, ManifestChunkSource, SnapshotV2Error, SnapshotV2Result};
use super::*;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const DEFAULT_MANIFEST_WRITE_CONCURRENCY: usize = 8;
const DEFAULT_MANIFEST_CACHE_MAX_ENTRIES: usize = 4_096;
const DEFAULT_MANIFEST_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

struct ManifestChunkCache {
    entries: BTreeMap<ManifestRef, Vec<u8>>,
    insertion_order: VecDeque<ManifestRef>,
    total_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl ManifestChunkCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            insertion_order: VecDeque::new(),
            total_bytes: 0,
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
        }
    }

    fn get(&self, reference: &ManifestRef) -> Option<Vec<u8>> {
        self.entries.get(reference).cloned()
    }

    fn insert(&mut self, reference: ManifestRef, bytes: Vec<u8>) {
        if bytes.len() > self.max_bytes || self.entries.contains_key(&reference) {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        self.entries.insert(reference, bytes);
        self.insertion_order.push_back(reference);
        while self.entries.len() > self.max_entries || self.total_bytes > self.max_bytes {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.len());
            }
        }
    }
}

pub(crate) struct RepositoryManifestSource<'a> {
    repo_root: &'a Path,
    anchored_repo: AnchoredRoot,
    cache: RefCell<ManifestChunkCache>,
}

impl<'a> RepositoryManifestSource<'a> {
    pub(crate) fn new(repo_root: &'a Path) -> Result<Self> {
        Ok(Self {
            repo_root,
            anchored_repo: AnchoredRoot::open(repo_root)?,
            cache: RefCell::new(ManifestChunkCache::new(
                DEFAULT_MANIFEST_CACHE_MAX_ENTRIES,
                DEFAULT_MANIFEST_CACHE_MAX_BYTES,
            )),
        })
    }
}

impl ManifestChunkSource for RepositoryManifestSource<'_> {
    fn load_manifest_chunk(&self, reference: ManifestRef) -> SnapshotV2Result<Vec<u8>> {
        if let Some(bytes) = self.cache.borrow().get(&reference) {
            return Ok(bytes);
        }
        let path = manifest_path(self.repo_root, reference).map_err(core_to_v2_error)?;
        let bytes = self
            .anchored_repo
            .read_bytes_bounded_path(&path, MAX_MANIFEST_CHUNK_BYTES as u64)
            .map_err(core_to_v2_error)?;
        self.cache.borrow_mut().insert(reference, bytes.clone());
        Ok(bytes)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn reference(byte: u8) -> ManifestRef {
        ManifestRef {
            kind: ChunkKind::Leaf,
            id: crate::storage::merkle_codec::Digest32::from_bytes([byte; 32]),
        }
    }

    #[test]
    fn manifest_chunk_cache_is_bounded_by_entry_count_and_bytes() {
        let mut cache = ManifestChunkCache::new(3, 10);
        cache.insert(reference(1), vec![1; 4]);
        cache.insert(reference(2), vec![2; 4]);
        cache.insert(reference(3), vec![3; 4]);
        assert!(cache.entries.len() <= 3);
        assert!(cache.total_bytes <= 10);
        assert!(cache.get(&reference(1)).is_none());

        cache.insert(reference(4), vec![4; 2]);
        cache.insert(reference(5), vec![5; 2]);
        assert!(cache.entries.len() <= 3);
        assert!(cache.total_bytes <= 10);
        assert_eq!(cache.get(&reference(5)), Some(vec![5; 2]));

        cache.insert(reference(6), vec![6; 11]);
        assert!(cache.get(&reference(6)).is_none());
        assert!(cache.entries.len() <= 3);
        assert!(cache.total_bytes <= 10);
    }
}

#[cfg(debug_assertions)]
pub(crate) fn store_built_manifest(repo_root: &Path, manifest: &BuiltManifest) -> Result<()> {
    store_built_manifest_profiled(repo_root, manifest, None)
}

#[cfg(debug_assertions)]
pub(crate) fn store_built_manifest_profiled(
    repo_root: &Path,
    manifest: &BuiltManifest,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<()> {
    store_built_manifest_profiled_impl(repo_root, manifest, recorder, None, &BTreeSet::new())
}

pub(crate) fn store_built_manifest_profiled_batched(
    repo_root: &Path,
    manifest: &BuiltManifest,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: &mut AnchoredParentSyncBatch,
    known_durable: &BTreeSet<ManifestRef>,
) -> Result<()> {
    store_built_manifest_profiled_impl(
        repo_root,
        manifest,
        recorder,
        Some(sync_batch),
        known_durable,
    )
}

fn store_built_manifest_profiled_impl(
    repo_root: &Path,
    manifest: &BuiltManifest,
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: Option<&mut AnchoredParentSyncBatch>,
    known_durable: &BTreeSet<ManifestRef>,
) -> Result<()> {
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let chunks = manifest.chunks.iter().collect::<Vec<_>>();
    if let Some(sync_batch) = sync_batch {
        for group in chunks.chunks(manifest_write_parallelism()) {
            let outcomes = group
                .par_iter()
                .map(|(reference, bytes)| {
                    let mut local_batch = AnchoredParentSyncBatch::new();
                    let result = store_manifest_chunk(
                        &anchored_repo,
                        repo_root,
                        **reference,
                        bytes,
                        recorder,
                        Some(&mut local_batch),
                        known_durable.contains(reference),
                    );
                    (result, local_batch)
                })
                .collect::<Vec<_>>();
            let mut first_error = None;
            for (result, local_batch) in outcomes {
                sync_batch.merge(local_batch)?;
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
            }
            if let Some(error) = first_error {
                // Successful siblings may already have published immutable
                // chunks. Finish their directory barrier before propagating
                // the unrelated worker error.
                sync_batch.flush()?;
                return Err(error);
            }
        }
    } else {
        for (reference, bytes) in chunks {
            store_manifest_chunk(
                &anchored_repo,
                repo_root,
                *reference,
                bytes,
                recorder,
                None,
                known_durable.contains(reference),
            )?;
        }
    }
    Ok(())
}

fn store_manifest_chunk(
    anchored_repo: &AnchoredRoot,
    repo_root: &Path,
    reference: ManifestRef,
    bytes: &[u8],
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    sync_batch: Option<&mut AnchoredParentSyncBatch>,
    existing_is_known_durable: bool,
) -> Result<()> {
    let actual = measure_io(
        recorder,
        crate::checkpoint_metrics::IoTimingKind::Hash,
        || manifest_id(reference.kind, bytes),
    )
    .map_err(codec_error)?;
    if actual != reference.id {
        return Err(CheckPoError::Corruption(format!(
            "prepared manifest chunk digest mismatch: expected {}, found {actual}",
            reference.id
        )));
    }
    let path = manifest_path(repo_root, reference)?;
    anchored_repo.store_content_addressed_bytes_profiled(
        &path,
        bytes,
        recorder,
        sync_batch,
        existing_is_known_durable,
    )
}

fn manifest_write_parallelism() -> usize {
    std::env::var("CHECKPO_MANIFEST_WRITE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .min(DEFAULT_MANIFEST_WRITE_CONCURRENCY)
        })
}

pub(crate) fn publish_snapshot_root(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    bytes: &[u8],
) -> Result<()> {
    publish_snapshot_root_profiled(repo_root, snapshot_id, bytes, None)
}

pub(crate) fn publish_snapshot_root_profiled(
    repo_root: &Path,
    snapshot_id: &SnapshotId,
    bytes: &[u8],
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
) -> Result<()> {
    let actual = SnapshotId::from_digest_bytes(*root_id(bytes).as_bytes());
    if &actual != snapshot_id {
        return Err(CheckPoError::Corruption(format!(
            "prepared snapshot root digest mismatch: expected {snapshot_id}, found {actual}"
        )));
    }
    let path = snapshot_path(repo_root, snapshot_id);
    AnchoredRoot::open(repo_root)?
        .store_content_addressed_bytes_profiled(&path, bytes, recorder, None, false)
}

fn manifest_path(repo_root: &Path, reference: ManifestRef) -> Result<PathBuf> {
    let id = reference.id.to_hex();
    match reference.kind {
        ChunkKind::Node => Ok(manifest_node_path(repo_root, &id)),
        ChunkKind::Leaf => Ok(manifest_leaf_path(repo_root, &id)),
        ChunkKind::Root => Err(CheckPoError::Corruption(
            "root cannot be loaded from the manifest chunk store".to_string(),
        )),
    }
}

fn measure_io<T>(
    recorder: Option<&crate::checkpoint_metrics::ArtifactIoRecorder>,
    kind: crate::checkpoint_metrics::IoTimingKind,
    operation: impl FnOnce() -> T,
) -> T {
    match recorder {
        Some(recorder) => recorder.measure(kind, operation),
        None => operation(),
    }
}

fn codec_error(error: super::merkle_codec::CodecError) -> CheckPoError {
    CheckPoError::Corruption(error.to_string())
}

fn core_to_v2_error(error: CheckPoError) -> SnapshotV2Error {
    SnapshotV2Error::Source(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_root_cas_repairs_conflicting_existing_bytes_from_verified_content() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let actual = SnapshotId::from_digest_bytes(*root_id(b"expected").as_bytes());
        publish_snapshot_root(&repo, &actual, b"expected").unwrap();
        fs::write(snapshot_path(&repo, &actual), b"conflict").unwrap();
        publish_snapshot_root(&repo, &actual, b"expected").unwrap();

        assert_eq!(
            fs::read(snapshot_path(&repo, &actual)).unwrap(),
            b"expected"
        );
    }

    #[test]
    fn anchored_cas_defers_parent_barrier_to_held_batch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let path = repo.join("manifests/v2/leaves/ab/id.chunk");
        let anchored = AnchoredRoot::open(&repo).unwrap();
        let mut batch = AnchoredParentSyncBatch::new();

        anchored
            .store_content_addressed_bytes_profiled(&path, b"chunk", None, Some(&mut batch), false)
            .unwrap();

        assert!(batch.pending_count() > 0);
        assert_eq!(fs::read(&path).unwrap(), b"chunk");
        batch.flush().unwrap();
        assert_eq!(batch.pending_count(), 0);
    }
}
