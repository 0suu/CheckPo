use super::AnchoredRoot;
use crate::{CheckPoError, Result};
use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const PREPARE_PREFIX: &str = ".prepare-";
const CLEANUP_PREFIX: &str = ".cleanup-";

#[derive(Debug, Clone, Copy)]
pub(crate) enum JournalFamily {
    CheckpointCreate,
    CheckpointDelete,
}

impl JournalFamily {
    fn active_relative(self) -> &'static Path {
        match self {
            Self::CheckpointCreate => Path::new("journals/checkpoint-create"),
            Self::CheckpointDelete => Path::new("journals/checkpoint-delete"),
        }
    }
}

/// Drains hidden prepare/cleanup directories without interpreting their payload.
///
/// Both namespaces live beside active transaction ids in the same held family
/// directory. A crash can leave a full, partially drained, or empty hidden
/// directory; each state is safe to retry and is never parsed as active work.
pub(crate) fn drain_journal_cleanup_trash(repo_root: &Path, family: JournalFamily) -> Result<bool> {
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let family_root = match open_family_root_for_mutation(&anchored_repo, family, false) {
        Ok(root) => root,
        Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    anchored_repo.verify_parent_binding(family.active_relative(), &family_root)?;

    let hidden_leaves = family_root
        .list_entry_names()?
        .into_iter()
        .filter(|leaf| is_journal_cleanup_name(leaf))
        .collect::<Vec<_>>();
    let had_trash = !hidden_leaves.is_empty();

    for leaf in hidden_leaves {
        let transaction = match family_root.open_directory_for_mutation(&leaf) {
            Ok(transaction) => transaction,
            Err(CheckPoError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
                continue
            }
            Err(error) => return Err(error),
        };
        transaction.remove_tree_contents()?;
        family_root.unlink_dir_if_bound(&leaf, transaction)?;
        // Persist each completed removal. A crash before this barrier may make
        // the hidden directory reappear, which is harmless because draining is
        // deliberately idempotent.
        family_root.sync_all()?;
    }

    anchored_repo.verify_parent_binding(family.active_relative(), &family_root)?;
    Ok(had_trash)
}

/// Detaches an active transaction with a same-parent atomic rename.
///
/// No payload is deleted under its active id. Once the single family-directory
/// barrier completes, recovery sees only the hidden cleanup name; a crash before
/// it completes sees either active work or opaque cleanup work in that same
/// directory, with no cross-parent durability ordering to reconcile.
fn detach_transaction_dir_to_trash(
    repo_root: &Path,
    family: JournalFamily,
    transaction_id: &str,
) -> Result<OsString> {
    validate_transaction_id(transaction_id)?;
    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let family_root = open_family_root_for_mutation(&anchored_repo, family, false)?;
    anchored_repo.verify_parent_binding(family.active_relative(), &family_root)?;

    let active_leaf = OsStr::new(transaction_id);
    let cleanup_leaf = OsString::from(format!(
        "{CLEANUP_PREFIX}{transaction_id}-{}",
        Uuid::new_v4().simple()
    ));
    let transaction = family_root.open_directory_for_mutation(active_leaf)?;
    family_root.rename_directory_no_replace_to_owned(
        active_leaf,
        transaction,
        &family_root,
        &cleanup_leaf,
    )?;
    family_root.sync_all()?;
    anchored_repo.verify_parent_binding(family.active_relative(), &family_root)?;
    Ok(cleanup_leaf)
}

pub(crate) fn detach_and_cleanup_journal_transaction(
    repo_root: &Path,
    family: JournalFamily,
    transaction_id: &str,
) -> Result<()> {
    drain_journal_cleanup_trash(repo_root, family)?;
    detach_transaction_dir_to_trash(repo_root, family, transaction_id)?;
    drain_journal_cleanup_trash(repo_root, family)?;
    Ok(())
}

/// Builds a complete hidden prepare directory and atomically publishes it under
/// the active transaction id only after `journal.json` is durable.
///
/// A crash before the same-parent rename leaves opaque prepare trash. After the
/// rename, active state always contains a complete journal. An empty active
/// directory is therefore corruption rather than an initialization artifact.
pub(crate) fn prepare_and_publish_journal_transaction(
    repo_root: &Path,
    family: JournalFamily,
    transaction_id: &str,
    journal_bytes: &[u8],
) -> Result<PathBuf> {
    validate_transaction_id(transaction_id)?;
    drain_journal_cleanup_trash(repo_root, family)?;

    let anchored_repo = AnchoredRoot::open(repo_root)?;
    let family_root = open_family_root_for_mutation(&anchored_repo, family, true)?;
    anchored_repo.verify_parent_binding(family.active_relative(), &family_root)?;

    let prepare_leaf = OsString::from(format!("{PREPARE_PREFIX}{transaction_id}"));
    let active_leaf = OsStr::new(transaction_id);
    let staged = family_root.create_directory(&prepare_leaf)?;
    family_root.sync_all()?;
    let mut journal = staged.create_new_file(OsStr::new("journal.json"))?;
    journal
        .write_all(journal_bytes)
        .map_err(|error| crate::io_error(staged.display_path().join("journal.json"), error))?;
    journal.sync_all()?;
    staged.sync_all()?;
    drop(journal);

    family_root.rename_directory_no_replace_to_owned(
        &prepare_leaf,
        staged,
        &family_root,
        active_leaf,
    )?;
    family_root.sync_all()?;
    anchored_repo.verify_parent_binding(family.active_relative(), &family_root)?;

    Ok(repo_root
        .join(family.active_relative())
        .join(transaction_id))
}

pub(crate) fn is_journal_cleanup_name(leaf: &OsStr) -> bool {
    leaf.to_str()
        .is_some_and(|leaf| leaf.starts_with(PREPARE_PREFIX) || leaf.starts_with(CLEANUP_PREFIX))
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if transaction_id.len() != 32
        || !transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(CheckPoError::Corruption(format!(
            "invalid journal transaction id: {transaction_id:?}"
        )));
    }
    Ok(())
}

fn open_family_root_for_mutation(
    anchored_repo: &AnchoredRoot,
    family: JournalFamily,
    create_missing: bool,
) -> Result<super::anchored_fs::AnchoredParent> {
    let synthetic = family.active_relative().join(".checkpo-anchor-leaf");
    anchored_repo
        .open_parent_for_mutation(&synthetic, create_missing)
        .map(|(parent, _)| parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_family(family: JournalFamily) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let active = repo.join(family.active_relative());
        fs::create_dir_all(&active).unwrap();
        (temp, repo, active)
    }

    fn hidden_entries(family_root: &Path) -> Vec<PathBuf> {
        let mut entries = fs::read_dir(family_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| is_journal_cleanup_name(path.file_name().unwrap()))
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[test]
    fn cleanup_trash_drains_full_partial_and_empty_directories_idempotently() {
        for family in [
            JournalFamily::CheckpointCreate,
            JournalFamily::CheckpointDelete,
        ] {
            let (_temp, repo, family_root) = setup_family(family);
            fs::create_dir_all(family_root.join(".cleanup-full/nested")).unwrap();
            fs::write(
                family_root.join(".cleanup-full/journal.json"),
                b"not parsed",
            )
            .unwrap();
            fs::write(family_root.join(".cleanup-full/nested/payload"), b"payload").unwrap();
            fs::create_dir_all(family_root.join(".cleanup-partial/nested")).unwrap();
            fs::write(
                family_root.join(".cleanup-partial/nested/remainder"),
                b"remainder",
            )
            .unwrap();
            fs::create_dir_all(family_root.join(".prepare-empty")).unwrap();

            assert!(drain_journal_cleanup_trash(&repo, family).unwrap());
            assert!(hidden_entries(&family_root).is_empty());
            assert!(!drain_journal_cleanup_trash(&repo, family).unwrap());
        }
    }

    #[test]
    fn crash_after_detach_is_recovered_before_next_transaction() {
        for family in [
            JournalFamily::CheckpointCreate,
            JournalFamily::CheckpointDelete,
        ] {
            let (_temp, repo, family_root) = setup_family(family);
            let first = "11111111111111111111111111111111";
            fs::create_dir_all(family_root.join(first).join("nested")).unwrap();
            fs::write(family_root.join(first).join("journal.json"), b"opaque").unwrap();
            fs::write(family_root.join(first).join("nested/payload"), b"payload").unwrap();

            let cleanup_leaf = detach_transaction_dir_to_trash(&repo, family, first).unwrap();
            assert!(!family_root.join(first).exists());
            assert!(family_root
                .join(&cleanup_leaf)
                .join("journal.json")
                .exists());

            assert!(drain_journal_cleanup_trash(&repo, family).unwrap());
            assert!(!family_root.join(cleanup_leaf).exists());
            assert!(!drain_journal_cleanup_trash(&repo, family).unwrap());

            let second = "22222222222222222222222222222222";
            fs::create_dir_all(family_root.join(second)).unwrap();
            fs::write(family_root.join(second).join("journal.json"), b"next").unwrap();
            detach_and_cleanup_journal_transaction(&repo, family, second).unwrap();
            assert!(!family_root.join(second).exists());
            assert!(hidden_entries(&family_root).is_empty());
        }
    }

    #[test]
    fn prepare_publishes_only_a_complete_active_journal() {
        for family in [
            JournalFamily::CheckpointCreate,
            JournalFamily::CheckpointDelete,
        ] {
            let (_temp, repo, family_root) = setup_family(family);
            let transaction_id = "33333333333333333333333333333333";
            let active_path = prepare_and_publish_journal_transaction(
                &repo,
                family,
                transaction_id,
                br#"{"state":"prepared"}"#,
            )
            .unwrap();

            assert_eq!(active_path, family_root.join(transaction_id));
            assert_eq!(
                std::fs::read(active_path.join("journal.json")).unwrap(),
                br#"{"state":"prepared"}"#
            );
            assert!(hidden_entries(&family_root).is_empty());
            detach_and_cleanup_journal_transaction(&repo, family, transaction_id).unwrap();
        }
    }
}
