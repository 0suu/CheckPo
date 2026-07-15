use crate::{
    load_project_snapshot, read_latest_snapshot_id, snapshot_path, write_latest_snapshot_id,
    CheckPoError, ProjectContext, Result, SnapshotId,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CREATE_JOURNAL_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum CreateJournalState {
    Prepared,
    RootPublished,
    InventoryUpdated,
    LatestUpdated,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateJournal {
    schema_version: u32,
    transaction_id: String,
    state: CreateJournalState,
    checkpoint_id: SnapshotId,
    expected_old_latest: Option<SnapshotId>,
    inventory_head_before: String,
    created_at_utc: String,
    updated_at_utc: String,
}

pub(crate) struct CreateJournalHandle {
    repo_root: PathBuf,
    path: PathBuf,
    journal: CreateJournal,
}

impl CreateJournalHandle {
    pub(crate) fn prepare(
        project: &ProjectContext,
        checkpoint_id: SnapshotId,
        expected_old_latest: Option<SnapshotId>,
    ) -> Result<Self> {
        let base = project.repo_root.join("journals").join("checkpoint-create");
        let transaction_id = Uuid::new_v4().simple().to_string();
        let root = base.join(&transaction_id);
        let now = crate::now_utc_string();
        let inventory_head_before =
            crate::storage::inventory_head_id(&project.repo_root, &project.project_id)?;
        let journal = CreateJournal {
            schema_version: CREATE_JOURNAL_SCHEMA_VERSION,
            transaction_id,
            state: CreateJournalState::Prepared,
            checkpoint_id,
            expected_old_latest,
            inventory_head_before,
            created_at_utc: now.clone(),
            updated_at_utc: now,
        };
        let path = root.join("journal.json");
        let bytes =
            serde_json::to_vec(&journal).map_err(|error| crate::json_error(&path, error))?;
        crate::storage::prepare_and_publish_journal_transaction(
            &project.repo_root,
            crate::storage::JournalFamily::CheckpointCreate,
            &journal.transaction_id,
            &bytes,
        )?;
        Ok(Self {
            repo_root: project.repo_root.clone(),
            path,
            journal,
        })
    }

    pub(crate) fn mark_root_published(&mut self) -> Result<()> {
        self.transition(CreateJournalState::RootPublished)
    }

    pub(crate) fn update_inventory(&mut self, project: &ProjectContext) -> Result<()> {
        crate::storage::add_snapshot_to_inventory_if_head(
            &project.repo_root,
            &project.project_id,
            &self.journal.checkpoint_id,
            &self.journal.inventory_head_before,
            &self.journal.transaction_id,
        )?;
        self.transition(CreateJournalState::InventoryUpdated)
    }

    pub(crate) fn mark_latest_updated(&mut self) -> Result<()> {
        self.transition(CreateJournalState::LatestUpdated)
    }

    pub(crate) fn commit(mut self) -> Result<Option<String>> {
        self.transition(CreateJournalState::Committed)?;
        Ok(cleanup_committed_create_journal(
            &self.repo_root,
            &self.journal.transaction_id,
        ))
    }

    fn transition(&mut self, state: CreateJournalState) -> Result<()> {
        self.journal.state = state;
        self.journal.updated_at_utc = crate::now_utc_string();
        write_create_journal(&self.repo_root, &self.path, &self.journal)
    }
}

pub(crate) fn recover_checkpoint_creations_unlocked(project: &ProjectContext) -> Result<bool> {
    // Detached directories are no longer active transactions. Drain them
    // opaquely before inspecting any journal so an interrupted cleanup cannot
    // be mistaken for work that must be replayed.
    let recovered_cleanup = crate::storage::drain_journal_cleanup_trash(
        &project.repo_root,
        crate::storage::JournalFamily::CheckpointCreate,
    )?;
    let family_relative = Path::new("journals/checkpoint-create");
    let anchored_repo = crate::storage::AnchoredRoot::open(&project.repo_root)?;
    let family = match anchored_repo.open_directory(family_relative, false) {
        Ok(family) => family,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(recovered_cleanup)
        }
        Err(error) => return Err(error),
    };
    anchored_repo.verify_parent_binding(family_relative, &family)?;
    let mut transaction_ids = family
        .list_entry_names()?
        .into_iter()
        .filter(|leaf| !crate::storage::is_journal_cleanup_name(leaf))
        .collect::<Vec<_>>();
    transaction_ids.sort();
    let recovered_any = recovered_cleanup || !transaction_ids.is_empty();
    for transaction_id in transaction_ids {
        recover_one(project, &anchored_repo, &transaction_id)?;
    }
    Ok(recovered_any)
}

fn recover_one(
    project: &ProjectContext,
    anchored_repo: &crate::storage::AnchoredRoot,
    transaction_id: &std::ffi::OsStr,
) -> Result<()> {
    let relative = Path::new("journals/checkpoint-create").join(transaction_id);
    let root = project.repo_root.join(&relative);
    let transaction = anchored_repo.open_directory(&relative, false)?;
    anchored_repo.verify_parent_binding(&relative, &transaction)?;
    ensure_only_journal_file(&transaction)?;
    drop(transaction);
    let journal_path = root.join("journal.json");
    let mut journal: CreateJournal = anchored_repo.read_json_path(&journal_path)?;
    validate_journal(&root, &journal)?;
    let published_path = snapshot_path(&project.repo_root, &journal.checkpoint_id);
    let published = match fs::symlink_metadata(&published_path) {
        Ok(metadata) if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {
            true
        }
        Ok(_) => {
            return Err(CheckPoError::Corruption(format!(
                "unsafe published checkpoint root: {}",
                published_path.display()
            )))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(crate::io_error(&published_path, error)),
    };

    if journal.state == CreateJournalState::Prepared && !published {
        return cleanup_create_journal(&project.repo_root, &journal.transaction_id);
    }
    if !published {
        return Err(CheckPoError::Corruption(format!(
            "checkpoint creation journal {:?} has no published root {}",
            journal.state, journal.checkpoint_id
        )));
    }
    load_project_snapshot(project, &journal.checkpoint_id)?;

    if matches!(
        journal.state,
        CreateJournalState::Prepared | CreateJournalState::RootPublished
    ) {
        crate::storage::add_snapshot_to_inventory_if_head(
            &project.repo_root,
            &project.project_id,
            &journal.checkpoint_id,
            &journal.inventory_head_before,
            &journal.transaction_id,
        )?;
        journal.state = CreateJournalState::InventoryUpdated;
        journal.updated_at_utc = crate::now_utc_string();
        write_create_journal(&project.repo_root, &journal_path, &journal)?;
    }

    if matches!(
        journal.state,
        CreateJournalState::Prepared
            | CreateJournalState::RootPublished
            | CreateJournalState::InventoryUpdated
    ) {
        let current = read_latest_snapshot_id(&project.repo_root)?;
        if current.as_ref() == Some(&journal.checkpoint_id) {
            // The ref update was durable even if the journal transition was not.
        } else if current == journal.expected_old_latest {
            write_latest_snapshot_id(&project.repo_root, &journal.checkpoint_id)?;
        } else {
            crate::diagnostics::log_warning(
                "checkpoint-create-recovery",
                &format!(
                    "published checkpoint {} was retained as a branch because refs/latest changed",
                    journal.checkpoint_id
                ),
            );
        }
        journal.state = CreateJournalState::LatestUpdated;
        journal.updated_at_utc = crate::now_utc_string();
        write_create_journal(&project.repo_root, &journal_path, &journal)?;
    }

    if journal.state != CreateJournalState::Committed {
        journal.state = CreateJournalState::Committed;
        journal.updated_at_utc = crate::now_utc_string();
        write_create_journal(&project.repo_root, &journal_path, &journal)?;
    }
    let _ = cleanup_committed_create_journal(&project.repo_root, &journal.transaction_id);
    Ok(())
}

fn write_create_journal(repo_root: &Path, path: &Path, journal: &CreateJournal) -> Result<()> {
    crate::storage::AnchoredRoot::open(repo_root)?.write_json_atomic_path(path, journal)
}

fn validate_journal(root: &Path, journal: &CreateJournal) -> Result<()> {
    if journal.schema_version != CREATE_JOURNAL_SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "checkpoint creation journal schema".to_string(),
            found: journal.schema_version,
            supported: CREATE_JOURNAL_SCHEMA_VERSION,
        });
    }
    let directory_id = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CheckPoError::Corruption("invalid creation journal directory".into()))?;
    if directory_id != journal.transaction_id {
        return Err(CheckPoError::Corruption(format!(
            "checkpoint creation journal id mismatch: {directory_id} != {}",
            journal.transaction_id
        )));
    }
    Ok(())
}

fn ensure_only_journal_file(root: &crate::storage::AnchoredParent) -> Result<()> {
    for leaf in root.list_entry_names()? {
        if leaf != "journal.json" {
            return Err(CheckPoError::Corruption(format!(
                "unexpected checkpoint creation journal payload: {}",
                root.display_path().join(leaf).display()
            )));
        }
    }
    Ok(())
}

fn cleanup_create_journal(repo_root: &Path, transaction_id: &str) -> Result<()> {
    crate::storage::detach_and_cleanup_journal_transaction(
        repo_root,
        crate::storage::JournalFamily::CheckpointCreate,
        transaction_id,
    )
}

fn cleanup_committed_create_journal(repo_root: &Path, transaction_id: &str) -> Option<String> {
    cleanup_create_journal(repo_root, transaction_id)
        .err()
        .map(|error| {
            let warning = format!(
                "checkpoint was committed, but journal cleanup was deferred until recovery: {error}"
            );
            crate::diagnostics::log_warning("checkpoint-create-cleanup", &warning);
            warning
        })
}
