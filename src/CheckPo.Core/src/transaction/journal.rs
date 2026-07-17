use super::*;

pub(super) const JOURNAL_STATE_UNREADABLE: &str = "unreadable";
pub(super) const JOURNAL_STATE_UNSUPPORTED_SCHEMA: &str = "unsupportedSchema";
pub(super) const TRANSACTION_JOURNAL_SCHEMA_VERSION: u32 = 3;
const MAX_TRANSACTION_JOURNAL_BYTES: u64 = 512 * 1024 * 1024;
const CLEANUP_LOCATION_ACTIVE: &str = "active";
const CLEANUP_LOCATION_TRASH: &str = "cleanupTrash";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionJournalEnvelope {
    schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TransactionJournal {
    pub(super) schema_version: u32,
    pub(super) transaction_id: String,
    pub(super) state: JournalState,
    pub(super) checkpoint_id: SnapshotId,
    pub(super) kind: OperationPlanKind,
    pub(super) operations: Vec<FileOperation>,
    pub(super) directories_to_remove: Vec<TrackedUnityFilePath>,
    pub(super) directories_to_create: Vec<TrackedUnityFilePath>,
    pub(super) created_at_utc: String,
    pub(super) updated_at_utc: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum JournalState {
    Created,
    Staged,
    Applying,
    Committed,
    Recovered,
}

pub(super) fn validate_transaction_journal_identity(
    tx_root: &Path,
    journal: &TransactionJournal,
) -> Result<()> {
    if journal.schema_version > TRANSACTION_JOURNAL_SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction journal schema".to_string(),
            found: journal.schema_version,
            supported: TRANSACTION_JOURNAL_SCHEMA_VERSION,
        });
    }
    if journal.schema_version != TRANSACTION_JOURNAL_SCHEMA_VERSION {
        return Err(CheckPoError::Corruption(format!(
            "invalid transaction journal schema: {}",
            journal.schema_version
        )));
    }
    let directory_id = tx_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| CheckPoError::Corruption("invalid transaction directory name".into()))?;
    if directory_id != journal.transaction_id {
        return Err(CheckPoError::Corruption(format!(
            "transaction id does not match journal directory: {} != {}",
            journal.transaction_id, directory_id
        )));
    }
    Ok(())
}

pub(super) fn read_transaction_journal(path: &Path) -> Result<TransactionJournal> {
    let repo_root = transaction_repo_root_for_journal(path)?;
    let bytes = crate::storage::AnchoredRoot::open(repo_root)?
        .read_bytes_bounded_path(path, MAX_TRANSACTION_JOURNAL_BYTES)?;
    let envelope: TransactionJournalEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))?;
    if envelope.schema_version > TRANSACTION_JOURNAL_SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction journal schema".to_string(),
            found: envelope.schema_version,
            supported: TRANSACTION_JOURNAL_SCHEMA_VERSION,
        });
    }
    if envelope.schema_version != TRANSACTION_JOURNAL_SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction journal schema".to_string(),
            found: envelope.schema_version,
            supported: TRANSACTION_JOURNAL_SCHEMA_VERSION,
        });
    }
    serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))
}

pub fn pending_transactions(project_path: impl AsRef<Path>) -> Result<Vec<PendingTransaction>> {
    let project = crate::load_project(project_path)?;
    let _lock = crate::acquire_project_repository_shared_lock(&project, "transaction-status")?;
    pending_transactions_for_project(&project)
}

pub fn pending_transactions_for_project(
    project: &ProjectContext,
) -> Result<Vec<PendingTransaction>> {
    crate::validate_repository_layout_no_follow(&project.repo_root)?;
    let mut pending = Vec::new();
    let dir = journals_dir(&project.repo_root);
    if !dir.exists() {
        return Ok(pending);
    }
    for entry in fs::read_dir(&dir).map_err(|error| crate::io_error(&dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(&dir, error))?;
        let entry_metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&entry_metadata) {
            return Err(CheckPoError::Corruption(format!(
                "transaction directory is a symbolic link or reparse point: {}",
                entry.path().display()
            )));
        }
        if !entry_metadata.is_dir() {
            continue;
        }
        let journal_path = entry.path().join("journal.json");
        let journal_is_regular = fs::symlink_metadata(&journal_path)
            .map(|metadata| metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata))
            .unwrap_or(false);
        if !journal_is_regular {
            pending.push(PendingTransaction {
                transaction_id: entry.file_name().to_string_lossy().to_string(),
                state: "unknown".to_string(),
                journal_path,
            });
            continue;
        }
        let journal = match read_transaction_journal(&journal_path) {
            Ok(journal) => journal,
            Err(crate::CheckPoError::UnsupportedFormat { found, .. }) => {
                pending.push(PendingTransaction {
                    transaction_id: entry.file_name().to_string_lossy().to_string(),
                    state: format!("{JOURNAL_STATE_UNSUPPORTED_SCHEMA}:{found}"),
                    journal_path,
                });
                continue;
            }
            Err(crate::CheckPoError::Json { .. }) => {
                pending.push(PendingTransaction {
                    transaction_id: entry.file_name().to_string_lossy().to_string(),
                    state: JOURNAL_STATE_UNREADABLE.to_string(),
                    journal_path,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        if journal.state != JournalState::Committed && journal.state != JournalState::Recovered {
            pending.push(PendingTransaction {
                transaction_id: journal.transaction_id,
                state: format!("{:?}", journal.state),
                journal_path,
            });
        }
    }
    Ok(pending)
}

pub fn ensure_no_pending_transactions(project: &ProjectContext) -> Result<()> {
    let pending = pending_transactions_for_project(project)?;
    if pending.is_empty() {
        Ok(())
    } else {
        Err(CheckPoError::PendingTransaction(format!(
            "{} pending transaction(s)",
            pending.len()
        )))
    }
}

pub fn analyze_transaction_cleanup(
    project_path: impl AsRef<Path>,
) -> Result<TransactionCleanupPlan> {
    let project = crate::load_project(project_path)?;
    let _lock =
        crate::acquire_project_repository_shared_lock(&project, "transaction-cleanup-analyze")?;
    transaction_cleanup_plan_for_project(&project)
}

pub fn cleanup_journals_with_expected_plan(
    project_path: impl AsRef<Path>,
    expected_plan: &TransactionCleanupPlan,
    options: ApplyOptions,
) -> Result<TransactionCleanupResult> {
    if !options.yes {
        return Err(crate::user_error("journal cleanup requires --yes."));
    }
    if expected_plan.schema_version != crate::TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION {
        return Err(CheckPoError::Corruption(format!(
            "unsupported transaction cleanup plan schema version: found {}, supported {}",
            expected_plan.schema_version,
            crate::TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION
        )));
    }
    let project = crate::load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = crate::acquire_project_repository_lock(&project, "transaction-cleanup")?;
    let current_plan = transaction_cleanup_plan_for_project(&project)?;
    if &current_plan != expected_plan {
        return Err(CheckPoError::WorkingTreeChanged(
            "transaction cleanup targets changed after preview".to_string(),
        ));
    }
    apply_transaction_cleanup_plan(&project, &current_plan)
}

fn transaction_cleanup_plan_for_project(
    project: &ProjectContext,
) -> Result<TransactionCleanupPlan> {
    crate::validate_repository_layout_no_follow(&project.repo_root)?;
    let dir = journals_dir(&project.repo_root);
    if !dir.exists() {
        return Ok(TransactionCleanupPlan {
            schema_version: crate::TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION,
            project_id: project.project_id.clone(),
            directory_count: 0,
            file_count: 0,
            total_bytes: 0,
            candidates: Vec::new(),
        });
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| crate::io_error(&dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(&dir, error))?;
        let entry_metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&entry_metadata) || !entry_metadata.is_dir() {
            continue;
        }
        let journal_path = entry.path().join("journal.json");
        let journal_metadata = match fs::symlink_metadata(&journal_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(crate::io_error(&journal_path, error)),
        };
        if crate::metadata_is_link_or_reparse(&journal_metadata) || !journal_metadata.is_file() {
            continue;
        }
        let journal = read_transaction_journal(&journal_path)?;
        validate_transaction_journal_identity(&entry.path(), &journal)?;
        if journal.state == JournalState::Committed || journal.state == JournalState::Recovered {
            candidates.push(transaction_cleanup_candidate(
                &entry.path(),
                &journal_path,
                &journal,
            )?);
        }
    }
    let trash_dir = transaction_cleanup_trash_dir(&project.repo_root);
    if trash_dir.exists() {
        let trash_metadata =
            fs::symlink_metadata(&trash_dir).map_err(|error| crate::io_error(&trash_dir, error))?;
        if crate::metadata_is_link_or_reparse(&trash_metadata) || !trash_metadata.is_dir() {
            return Err(CheckPoError::Corruption(format!(
                "transaction cleanup trash is not a regular directory: {}",
                trash_dir.display()
            )));
        }
        for entry in fs::read_dir(&trash_dir).map_err(|error| crate::io_error(&trash_dir, error))? {
            let entry = entry.map_err(|error| crate::io_error(&trash_dir, error))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| crate::io_error(entry.path(), error))?;
            if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(CheckPoError::Corruption(format!(
                    "transaction cleanup trash contains an unsafe entry: {}",
                    entry.path().display()
                )));
            }
            candidates.push(cleanup_trash_candidate(&entry.path())?);
        }
    }
    candidates.sort_by(|left, right| {
        left.location
            .cmp(&right.location)
            .then_with(|| left.transaction_id.cmp(&right.transaction_id))
    });
    let file_count = candidates.iter().try_fold(0_usize, |total, candidate| {
        total
            .checked_add(candidate.file_count)
            .ok_or_else(|| CheckPoError::Corruption("transaction file count overflow".into()))
    })?;
    let total_bytes = candidates.iter().try_fold(0_u64, |total, candidate| {
        total
            .checked_add(candidate.size_bytes)
            .ok_or_else(|| CheckPoError::Corruption("transaction payload size overflow".into()))
    })?;
    Ok(TransactionCleanupPlan {
        schema_version: crate::TRANSACTION_CLEANUP_PLAN_SCHEMA_VERSION,
        project_id: project.project_id.clone(),
        directory_count: candidates.len(),
        file_count,
        total_bytes,
        candidates,
    })
}

fn apply_transaction_cleanup_plan(
    project: &ProjectContext,
    plan: &TransactionCleanupPlan,
) -> Result<TransactionCleanupResult> {
    let dir = journals_dir(&project.repo_root);
    let trash_dir = transaction_cleanup_trash_dir(&project.repo_root);
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let mut active_roots = Vec::new();
    let mut trash_roots = Vec::new();
    for expected_candidate in &plan.candidates {
        let root = match expected_candidate.location.as_str() {
            CLEANUP_LOCATION_ACTIVE => dir.join(&expected_candidate.transaction_id),
            CLEANUP_LOCATION_TRASH => trash_dir.join(&expected_candidate.transaction_id),
            value => {
                return Err(CheckPoError::Corruption(format!(
                    "unknown transaction cleanup location: {value}"
                )))
            }
        };
        let metadata =
            fs::symlink_metadata(&root).map_err(|error| crate::io_error(&root, error))?;
        if crate::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(CheckPoError::Corruption(format!(
                "transaction cleanup target is not a regular directory: {}",
                root.display()
            )));
        }
        let current_candidate = if expected_candidate.location == CLEANUP_LOCATION_ACTIVE {
            let journal_path = root.join("journal.json");
            let journal = read_transaction_journal(&journal_path)?;
            validate_transaction_journal_identity(&root, &journal)?;
            transaction_cleanup_candidate(&root, &journal_path, &journal)?
        } else {
            cleanup_trash_candidate(&root)?
        };
        if &current_candidate != expected_candidate {
            return Err(CheckPoError::WorkingTreeChanged(
                "transaction cleanup target changed during apply".to_string(),
            ));
        }
        if expected_candidate.location == CLEANUP_LOCATION_ACTIVE {
            active_roots.push(root);
        } else {
            trash_roots.push(root);
        }
    }

    let mut new_batch_root = None;
    if !active_roots.is_empty() {
        let trash_relative = trash_dir.strip_prefix(&project.repo_root).map_err(|_| {
            CheckPoError::Corruption("transaction cleanup trash escaped repository".into())
        })?;
        ensure_anchored_directory(&anchored_repo, trash_relative)?;
        let batch_root = trash_dir.join(Uuid::new_v4().simple().to_string());
        let batch_relative = batch_root.strip_prefix(&project.repo_root).map_err(|_| {
            CheckPoError::Corruption("transaction cleanup batch escaped repository".into())
        })?;
        ensure_anchored_directory(&anchored_repo, batch_relative)?;
        for source in &active_roots {
            let file_name = source.file_name().ok_or_else(|| {
                CheckPoError::Corruption("transaction cleanup target has no name".into())
            })?;
            let destination = batch_root.join(file_name);
            move_repo_directory_anchored(&anchored_repo, &project.repo_root, source, &destination)?;
        }
        new_batch_root = Some(batch_root);
    }

    if let Some(batch_root) = new_batch_root {
        trash_roots.push(batch_root);
    }
    for root in trash_roots {
        remove_repo_tree_anchored(&anchored_repo, &project.repo_root, &root)?;
    }

    anchored_repo.verify_root_binding()?;

    Ok(TransactionCleanupResult {
        deleted_directory_count: plan.directory_count,
        deleted_bytes: plan.total_bytes,
    })
}

fn ensure_anchored_directory(
    anchored_repo: &crate::storage::AnchoredRoot,
    relative: &Path,
) -> Result<()> {
    let (parent, leaf) = anchored_repo.open_parent_for_mutation(relative, true)?;
    match parent.open_directory(&leaf) {
        Ok(directory) => {
            drop(directory);
            Ok(())
        }
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            let directory = parent.create_directory(&leaf)?;
            directory.sync_all()?;
            parent.sync_all()
        }
        Err(error) => Err(error),
    }
}

fn move_repo_directory_anchored(
    anchored_repo: &crate::storage::AnchoredRoot,
    repo_root: &Path,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    let source_relative = source.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption("transaction cleanup source escaped repository".into())
    })?;
    let destination_relative = destination.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption("transaction cleanup destination escaped repository".into())
    })?;
    let (source_parent, source_leaf) =
        anchored_repo.open_parent_for_mutation(source_relative, false)?;
    let source_directory = source_parent.open_directory(&source_leaf)?;
    let (destination_parent, destination_leaf) =
        anchored_repo.open_parent_for_mutation(destination_relative, false)?;
    source_parent.rename_directory_no_replace_to_owned(
        &source_leaf,
        source_directory,
        &destination_parent,
        &destination_leaf,
    )?;
    destination_parent.sync_all()?;
    source_parent.sync_all()
}

fn remove_repo_tree_anchored(
    anchored_repo: &crate::storage::AnchoredRoot,
    repo_root: &Path,
    root: &Path,
) -> Result<()> {
    let relative = root.strip_prefix(repo_root).map_err(|_| {
        CheckPoError::Corruption("transaction cleanup target escaped repository".into())
    })?;
    let (parent, leaf) = anchored_repo.open_parent_for_mutation(relative, false)?;
    let directory = parent.open_directory_for_mutation(&leaf)?;
    directory.remove_tree_contents()?;
    drop(directory);
    parent.unlink_dir(&leaf)?;
    parent.sync_all()
}

fn transaction_cleanup_candidate(
    tx_root: &Path,
    journal_path: &Path,
    journal: &TransactionJournal,
) -> Result<TransactionCleanupCandidate> {
    let journal_bytes =
        fs::read(journal_path).map_err(|error| crate::io_error(journal_path, error))?;
    let journal_digest = blake3::hash(&journal_bytes).to_hex().to_string();
    let (file_count, size_bytes, tree_metadata_digest) = transaction_tree_metadata(tx_root)?;
    let state = match journal.state {
        JournalState::Committed => "committed",
        JournalState::Recovered => "recovered",
        JournalState::Created => "created",
        JournalState::Staged => "staged",
        JournalState::Applying => "applying",
    };
    Ok(TransactionCleanupCandidate {
        location: CLEANUP_LOCATION_ACTIVE.to_string(),
        transaction_id: journal.transaction_id.clone(),
        state: state.to_string(),
        journal_digest,
        file_count,
        size_bytes,
        tree_metadata_digest,
    })
}

fn cleanup_trash_candidate(root: &Path) -> Result<TransactionCleanupCandidate> {
    let transaction_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| CheckPoError::Corruption("invalid cleanup trash directory name".into()))?;
    let (file_count, size_bytes, tree_metadata_digest) = transaction_tree_metadata(root)?;
    Ok(TransactionCleanupCandidate {
        location: CLEANUP_LOCATION_TRASH.to_string(),
        transaction_id: transaction_id.to_string(),
        state: "cleanupTrash".to_string(),
        journal_digest: String::new(),
        file_count,
        size_bytes,
        tree_metadata_digest,
    })
}

fn transaction_tree_metadata(root: &Path) -> Result<(usize, u64, String)> {
    let mut tree_hasher = blake3::Hasher::new();
    tree_hasher.update(b"checkpo-transaction-cleanup-tree-v2\0");
    let mut file_count = 0_usize;
    let mut size_bytes = 0_u64;
    let mut entries = walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter();
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(|error| CheckPoError::Unexpected(error.to_string()))?;
        if entry.path() == root {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&metadata) {
            if metadata.is_dir() {
                entries.skip_current_dir();
            }
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a symbolic link or reparse point: {}",
                entry.path().display()
            )));
        }
        let relative = entry.path().strip_prefix(root).map_err(|_| {
            CheckPoError::Corruption("transaction payload escaped its directory".into())
        })?;
        hash_cleanup_relative_path(&mut tree_hasher, relative)?;
        if metadata.is_dir() {
            tree_hasher.update(b"d");
        } else if metadata.is_file() {
            tree_hasher.update(b"f");
            tree_hasher.update(&metadata.len().to_le_bytes());
            hash_cleanup_file_identity(&mut tree_hasher, entry.path(), &metadata)?;
            file_count = file_count.checked_add(1).ok_or_else(|| {
                CheckPoError::Corruption("transaction file count overflow".into())
            })?;
            size_bytes = size_bytes.checked_add(metadata.len()).ok_or_else(|| {
                CheckPoError::Corruption("transaction payload size overflow".into())
            })?;
        } else {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains an unsupported entry: {}",
                entry.path().display()
            )));
        }
    }
    Ok((
        file_count,
        size_bytes,
        tree_hasher.finalize().to_hex().to_string(),
    ))
}

fn hash_cleanup_file_identity(
    hasher: &mut blake3::Hasher,
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<()> {
    if let Some(fingerprint) = crate::scanner::file_fingerprint(path, metadata)? {
        let bytes = fingerprint.as_bytes();
        let length = u64::try_from(bytes.len())
            .map_err(|_| CheckPoError::Corruption("cleanup fingerprint length overflow".into()))?;
        hasher.update(&length.to_le_bytes());
        hasher.update(bytes);
        return Ok(());
    }

    let modified = metadata
        .modified()
        .map_err(|error| crate::io_error(path, error))?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            CheckPoError::Corruption(format!(
                "transaction payload has a pre-epoch modification time: {}: {error}",
                path.display()
            ))
        })?;
    hasher.update(&modified.as_secs().to_le_bytes());
    hasher.update(&modified.subsec_nanos().to_le_bytes());
    Ok(())
}

#[cfg(unix)]
fn hash_cleanup_relative_path(hasher: &mut blake3::Hasher, path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let length = u64::try_from(bytes.len())
        .map_err(|_| CheckPoError::Corruption("transaction path length overflow".into()))?;
    hasher.update(&length.to_le_bytes());
    hasher.update(bytes);
    Ok(())
}

#[cfg(windows)]
fn hash_cleanup_relative_path(hasher: &mut blake3::Hasher, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let byte_length = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u64::try_from(length).ok())
        .ok_or_else(|| CheckPoError::Corruption("transaction path length overflow".into()))?;
    hasher.update(&byte_length.to_le_bytes());
    for value in wide {
        hasher.update(&value.to_le_bytes());
    }
    Ok(())
}

pub(super) fn write_journal(path: &Path, journal: &TransactionJournal) -> Result<()> {
    let repo_root = transaction_repo_root_for_journal(path)?;
    crate::storage::AnchoredRoot::open(repo_root)?.write_json_atomic_path(path, journal)
}

fn transaction_repo_root_for_journal(path: &Path) -> Result<&Path> {
    let transaction_root = path.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "transaction journal has no parent: {}",
            path.display()
        ))
    })?;
    let transactions = transaction_root.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "transaction journal has no transactions root: {}",
            path.display()
        ))
    })?;
    let journals = transactions.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "transaction journal has no journals root: {}",
            path.display()
        ))
    })?;
    let repo_root = journals.parent().ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "transaction journal has no repository root: {}",
            path.display()
        ))
    })?;
    if path.file_name() != Some(std::ffi::OsStr::new("journal.json"))
        || transactions.file_name() != Some(std::ffi::OsStr::new("transactions"))
        || journals.file_name() != Some(std::ffi::OsStr::new("journals"))
    {
        return Err(CheckPoError::Corruption(format!(
            "transaction journal is outside the canonical repository namespace: {}",
            path.display()
        )));
    }
    Ok(repo_root)
}

pub(super) fn journals_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("journals").join("transactions")
}

fn transaction_cleanup_trash_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("journals").join("transaction-cleanup-trash")
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    let mut entries = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(|error| CheckPoError::Unexpected(error.to_string()))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| crate::io_error(entry.path(), error))?;
        if crate::metadata_is_link_or_reparse(&metadata) {
            if metadata.is_dir() {
                entries.skip_current_dir();
            }
            return Err(CheckPoError::Corruption(format!(
                "transaction payload contains a symbolic link or reparse point: {}",
                entry.path().display()
            )));
        }
        if metadata.is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

pub(super) fn dir_size(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    for file in walk_files(root)? {
        total = total
            .checked_add(
                fs::symlink_metadata(&file)
                    .map_err(|error| crate::io_error(&file, error))?
                    .len(),
            )
            .ok_or_else(|| CheckPoError::Corruption("transaction payload size overflow".into()))?;
    }
    Ok(total)
}
