use super::*;

pub(super) const JOURNAL_STATE_UNREADABLE: &str = "unreadable";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TransactionJournal {
    pub(super) schema_version: u32,
    pub(super) transaction_id: String,
    pub(super) state: JournalState,
    pub(super) checkpoint_id: SnapshotId,
    pub(super) kind: OperationPlanKind,
    pub(super) operations: Vec<FileOperation>,
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

pub fn pending_transactions(project_path: impl AsRef<Path>) -> Result<Vec<PendingTransaction>> {
    let project = crate::load_project(project_path)?;
    pending_transactions_for_project(&project)
}

pub fn pending_transactions_for_project(
    project: &ProjectContext,
) -> Result<Vec<PendingTransaction>> {
    let mut pending = Vec::new();
    let dir = journals_dir(&project.repo_root);
    if !dir.exists() {
        return Ok(pending);
    }
    for entry in fs::read_dir(&dir).map_err(|error| crate::io_error(&dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(&dir, error))?;
        if !entry.path().is_dir() {
            continue;
        }
        let journal_path = entry.path().join("journal.json");
        if !journal_path.is_file() {
            pending.push(PendingTransaction {
                transaction_id: entry.file_name().to_string_lossy().to_string(),
                state: "unknown".to_string(),
                journal_path,
            });
            continue;
        }
        let journal: TransactionJournal = match crate::read_json(&journal_path) {
            Ok(journal) => journal,
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

pub fn cleanup_journals(project_path: impl AsRef<Path>) -> Result<TransactionCleanupResult> {
    let project = crate::load_project(project_path)?;
    crate::ensure_project_location_allows_mutation(&project)?;
    let _lock = acquire_repository_lock(&project.repo_root, "transaction-cleanup")?;
    let mut deleted_directory_count = 0_usize;
    let mut deleted_bytes = 0_u64;
    let dir = journals_dir(&project.repo_root);
    if !dir.exists() {
        return Ok(TransactionCleanupResult {
            deleted_directory_count,
            deleted_bytes,
        });
    }
    for entry in fs::read_dir(&dir).map_err(|error| crate::io_error(&dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(&dir, error))?;
        let journal_path = entry.path().join("journal.json");
        if !journal_path.is_file() {
            continue;
        }
        let journal: TransactionJournal = crate::read_json(&journal_path)?;
        if journal.state == JournalState::Committed || journal.state == JournalState::Recovered {
            let tx_root = entry.path();
            deleted_bytes += dir_size(&tx_root)?;
            fs::remove_dir_all(&tx_root).map_err(|error| crate::io_error(&tx_root, error))?;
            deleted_directory_count += 1;
        }
    }
    Ok(TransactionCleanupResult {
        deleted_directory_count,
        deleted_bytes,
    })
}

pub(super) fn write_journal(path: &Path, journal: &TransactionJournal) -> Result<()> {
    write_json_atomic(path, journal)
}

pub(super) fn journals_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("journals")
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| CheckPoError::Unexpected(error.to_string()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

pub(super) fn directory_is_empty_or_missing(path: &Path) -> Result<bool> {
    match fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(crate::io_error(path, error)),
    }
}

pub(super) fn dir_size(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    for file in walk_files(root)? {
        total += fs::metadata(&file)
            .map_err(|error| crate::io_error(&file, error))?
            .len();
    }
    Ok(total)
}
