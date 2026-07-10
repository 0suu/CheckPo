mod atomic_io;
mod db_file;
mod layout;
mod lock;
mod object_store;
mod platform;
mod snapshot_store;
#[cfg(test)]
mod tests;

use crate::{
    db_error, io_error, json_error, CheckPoError, ObjectId, ProjectContext, ProjectId,
    RepositoryConfig, Result, SnapshotFile, SnapshotId,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

use atomic_io::replace_file;
pub(crate) use atomic_io::{
    copy_file_no_replace, move_file_no_replace, reflink_or_copy_file_no_replace, sync_parent_chain,
    write_json_atomic_new, write_text_atomic, CopySourceDisposition,
};
pub use atomic_io::{read_json, sync_parent_dir, write_bytes_atomic, write_json_atomic};
pub use db_file::{db_path, open_db};
pub(crate) use layout::object_id_from_loose_relative_path;
pub use layout::{
    canonical_utc, checkpoint_names_path, init_repo_layout, load_repo_config, now_utc_string,
    object_path, refs_latest_path, repo_root, snapshot_path, snapshots_dir,
    validate_repository_config,
};
pub(crate) use lock::FileLock;
pub use lock::{acquire_repository_lock, RepositoryLock};
pub(crate) use object_store::copy_object_to_file;
pub use object_store::{
    hash_file, put_object_from_file_with_known_hash, verify_file_hash_and_size,
};
pub(crate) use platform::available_space_bytes;
pub use snapshot_store::{
    canonical_snapshot_bytes, list_snapshot_ids, load_project_snapshot, load_snapshot,
    read_latest_snapshot_id, save_snapshot, snapshot_id_from_bytes, write_latest_snapshot_id,
};
pub(crate) use snapshot_store::{load_project_snapshot_with_warnings, load_snapshot_with_warnings};
