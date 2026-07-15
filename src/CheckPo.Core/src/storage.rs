mod anchored_fs;
mod atomic_io;
pub(crate) mod chunk_store;
mod db_file;
mod fs_safety;
mod journal_cleanup;
mod layout;
mod lock;
pub(crate) mod merkle_codec;
mod object_store;
mod platform;
mod snapshot_inventory;
mod snapshot_store;
pub(crate) mod snapshot_v2;
#[cfg(test)]
mod tests;
mod windows_durability;

use crate::{
    db_error, io_error, json_error, CheckPoError, ObjectId, ProjectContext, ProjectId,
    RepositoryConfig, Result, SnapshotFile, SnapshotId,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub(crate) use anchored_fs::{
    AnchoredFile, AnchoredFileVersion, AnchoredParent, AnchoredParentSyncBatch, AnchoredRoot,
};
#[cfg(test)]
pub(crate) use atomic_io::move_file_no_replace;
#[allow(unused_imports)]
pub(crate) use atomic_io::{
    move_file_no_replace_deferred_dirs_profiled, move_file_no_replace_with_status_profiled,
    replace_file, sync_directory, sync_parent_chain,
};
#[cfg(test)]
pub(crate) use atomic_io::{
    move_file_to_tombstone, reflink_or_copy_file_no_replace, remove_file_durable,
};
pub use atomic_io::{read_json, sync_parent_dir};
pub(crate) use chunk_store::RepositoryManifestSource;
#[cfg(test)]
pub(crate) use db_file::open_db;
pub(crate) use db_file::open_file_fingerprint_db;
pub(crate) use db_file::remove_file_fingerprint_db_if_exists;
pub use db_file::{db_path, file_fingerprint_db_path};
#[allow(unused_imports)]
pub(crate) use fs_safety::{
    create_absolute_dir_all_no_follow, create_dir_all_no_follow, create_dir_all_no_follow_batched,
    create_dir_all_no_follow_profiled, ensure_regular_directory_no_follow,
    ensure_regular_file_no_follow, metadata_is_link_or_reparse,
    validate_repository_layout_no_follow, DirectorySyncBatch,
};
pub(crate) use journal_cleanup::{
    detach_and_cleanup_journal_transaction, drain_journal_cleanup_trash, is_journal_cleanup_name,
    prepare_and_publish_journal_transaction, JournalFamily,
};
pub use layout::{
    canonical_utc, checkpoint_names_path, init_repo_layout, load_repo_config, now_utc_string,
    object_path, refs_latest_path, repo_root, snapshot_path, snapshots_dir,
};
pub(crate) use layout::{
    manifest_leaf_path, manifest_leaves_dir, manifest_node_path, manifest_nodes_dir,
    object_id_from_loose_relative_path,
};
pub(crate) use lock::FileLock;
pub use lock::{acquire_repository_lock, acquire_repository_shared_lock, RepositoryLock};
#[cfg(test)]
pub(crate) use object_store::copy_object_to_file;
#[cfg(test)]
pub(crate) use object_store::hash_file;
#[cfg(test)]
pub(crate) use object_store::put_object_from_file_with_known_hash;
pub(crate) use object_store::{
    object_path_no_follow, put_object_from_anchored_file_with_known_hash_profiled_batched,
    verify_stored_object_profiled,
};
pub(crate) use platform::available_space_bytes;
pub(crate) use snapshot_inventory::{
    add_snapshot_to_inventory_if_head, inventory_head_id, inventory_snapshot_count,
    project_snapshot_removal, remove_snapshot_from_inventory_if_head,
    validate_physical_snapshot_inventory,
};
#[cfg(debug_assertions)]
pub(crate) use snapshot_store::save_snapshot;
pub use snapshot_store::{
    canonical_snapshot_bytes, list_snapshot_ids, load_project_snapshot, load_snapshot,
    read_latest_snapshot_id, snapshot_id_from_bytes, write_latest_snapshot_id,
};
pub(crate) use snapshot_store::{
    decode_snapshot_root_bytes, load_project_snapshot_with_manifest_references,
    load_project_snapshot_with_warnings, load_snapshot_root_header, prepare_snapshot,
    publish_prepared_snapshot_root, publish_prepared_snapshot_root_profiled,
    store_prepared_snapshot_chunks_profiled_batched, SnapshotRootHeader,
};
