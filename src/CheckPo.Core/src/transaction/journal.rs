use super::*;

pub(super) const JOURNAL_STATE_UNREADABLE: &str = "unreadable";
pub(super) const JOURNAL_STATE_UNSUPPORTED_SCHEMA: &str = "unsupportedSchema";
const TRANSACTION_JOURNAL_SCHEMA_VERSION_V1: u32 = 1;

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
    if journal.schema_version > TRANSACTION_JOURNAL_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction journal schema".to_string(),
            found: journal.schema_version,
            supported: TRANSACTION_JOURNAL_SCHEMA_VERSION_V1,
        });
    }
    if journal.schema_version != TRANSACTION_JOURNAL_SCHEMA_VERSION_V1 {
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
    let bytes = fs::read(path).map_err(|error| crate::io_error(path, error))?;
    let envelope: TransactionJournalEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))?;
    if envelope.schema_version > TRANSACTION_JOURNAL_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "transaction journal schema".to_string(),
            found: envelope.schema_version,
            supported: TRANSACTION_JOURNAL_SCHEMA_VERSION_V1,
        });
    }
    if envelope.schema_version != TRANSACTION_JOURNAL_SCHEMA_VERSION_V1 {
        return Err(CheckPoError::Corruption(format!(
            "invalid transaction journal schema: {}",
            envelope.schema_version
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| crate::json_error(path, error))
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

pub fn cleanup_journals(
    project_path: impl AsRef<Path>,
    options: ApplyOptions,
) -> Result<TransactionCleanupResult> {
    if !options.yes {
        return Err(crate::user_error("journal cleanup requires --yes."));
    }
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
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| crate::io_error(&dir, error))? {
        let entry = entry.map_err(|error| crate::io_error(&dir, error))?;
        let journal_path = entry.path().join("journal.json");
        if !journal_path.is_file() {
            continue;
        }
        let journal = read_transaction_journal(&journal_path)?;
        validate_transaction_journal_identity(&entry.path(), &journal)?;
        if journal.state == JournalState::Committed || journal.state == JournalState::Recovered {
            let tx_root = entry.path();
            candidates.push((tx_root.clone(), dir_size(&tx_root)?));
        }
    }
    for (tx_root, size_bytes) in candidates {
        fs::remove_dir_all(&tx_root).map_err(|error| crate::io_error(&tx_root, error))?;
        deleted_bytes += size_bytes;
        deleted_directory_count += 1;
    }
    Ok(TransactionCleanupResult {
        deleted_directory_count,
        deleted_bytes,
    })
}

pub(super) fn write_journal(path: &Path, journal: &TransactionJournal) -> Result<()> {
    write_json_atomic(path, journal)?;
    if let Some(transaction_root) = path.parent() {
        crate::sync_parent_dir(transaction_root)?;
    }
    Ok(())
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
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(CheckPoError::Corruption(format!(
                "transaction payload path is not a regular directory: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(crate::io_error(path, error)),
    }
    match fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
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
