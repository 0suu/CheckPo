use crate::{CheckPoError, ObjectId, ProjectId, Result, SnapshotId};
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const STATE_MAGIC: &[u8; 8] = b"CHKPINV2";
const ROOT_MAGIC: &[u8; 8] = b"CHKPINR2";
const LEAF_MAGIC: &[u8; 8] = b"CHKPINL2";
const SCHEMA_VERSION: u32 = 2;
const STATE_BYTES: usize = 174;
const ROOT_HEADER_BYTES: usize = 36;
const ROOT_SLOT_BYTES: usize = 37;
const ROOT_BYTES: usize = ROOT_HEADER_BYTES + 256 * ROOT_SLOT_BYTES;
const LEAF_HEADER_BYTES: usize = 33;
const MAX_LEAF_BYTES: u64 = 256 * 1024 * 1024;
const HEAD_BYTES_MAX: u64 = 65;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InventoryOperation {
    Initialize,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InventoryState {
    project_id: ProjectId,
    generation: u64,
    count: u64,
    operation: InventoryOperation,
    parent: Option<ObjectId>,
    snapshot_id: Option<SnapshotId>,
    operation_id: [u8; 32],
    set_root: ObjectId,
}

#[derive(Debug, Clone)]
struct InventoryHead {
    id: ObjectId,
    state: InventoryState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeafRef {
    id: ObjectId,
    count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetRoot {
    project_id: ProjectId,
    count: u64,
    leaves: Vec<Option<LeafRef>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetLeaf {
    project_id: ProjectId,
    prefix: u8,
    ids: Vec<SnapshotId>,
}

pub(crate) fn initialize_snapshot_inventory(
    repo_root: &Path,
    project_id: &ProjectId,
) -> Result<()> {
    let head_path = inventory_head_path(repo_root);
    match fs::symlink_metadata(&head_path) {
        Ok(metadata) if metadata.is_file() && !crate::metadata_is_link_or_reparse(&metadata) => {
            inventory_head(repo_root, project_id).map(|_| ())
        }
        Ok(_) => Err(CheckPoError::Corruption(format!(
            "snapshot inventory head is not a regular file: {}",
            head_path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if !crate::list_snapshot_ids(repo_root)?.is_empty() {
                return Err(CheckPoError::Corruption(
                    "snapshot inventory is missing for a non-empty repository".to_string(),
                ));
            }
            let anchored_repo = super::AnchoredRoot::open(repo_root)?;
            for relative in [
                Path::new("inventory/snapshots/states"),
                Path::new("inventory/snapshots/sets/roots"),
                Path::new("inventory/snapshots/sets/leaves"),
            ] {
                let directory = anchored_repo.open_directory(relative, true)?;
                anchored_repo.verify_parent_binding(relative, &directory)?;
            }
            anchored_repo.verify_root_binding()?;
            let root = SetRoot {
                project_id: project_id.clone(),
                count: 0,
                leaves: vec![None; 256],
            };
            let set_root = store_set_root(repo_root, &root)?;
            let state = InventoryState {
                project_id: project_id.clone(),
                generation: 0,
                count: 0,
                operation: InventoryOperation::Initialize,
                parent: None,
                snapshot_id: None,
                operation_id: [0; 32],
                set_root,
            };
            let id = store_state(repo_root, &state)?;
            write_head(repo_root, &id)
        }
        Err(error) => Err(crate::io_error(&head_path, error)),
    }
}

pub(crate) fn inventory_head_id(repo_root: &Path, project_id: &ProjectId) -> Result<String> {
    Ok(inventory_head(repo_root, project_id)?.id.to_string())
}

pub(crate) fn inventory_snapshot_count(repo_root: &Path, project_id: &ProjectId) -> Result<u64> {
    Ok(inventory_head(repo_root, project_id)?.state.count)
}

/// Projects the canonical inventory state after removing one snapshot.
///
/// The common latest-delete path is O(1): a surviving snapshot parent is the
/// prior canonical head. Only a broken/deleted lineage falls back to walking
/// immutable inventory generations and selecting the newest surviving Add.
pub(crate) fn project_snapshot_removal(
    repo_root: &Path,
    project_id: &ProjectId,
    removed: &SnapshotId,
    current_latest: Option<&SnapshotId>,
    preferred_parent: Option<&SnapshotId>,
) -> Result<(usize, Option<SnapshotId>)> {
    let head = inventory_head(repo_root, project_id)?;
    if head.state.count == 0 {
        return Err(CheckPoError::Corruption(
            "cannot remove a snapshot from an empty inventory".to_string(),
        ));
    }
    let root = load_set_root(repo_root, &head.state.set_root, project_id)?;
    if root.count != head.state.count {
        return Err(CheckPoError::Corruption(
            "snapshot inventory state count does not match its set root".to_string(),
        ));
    }
    if !set_root_contains(repo_root, project_id, &root, removed)? {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory does not contain root {removed}"
        )));
    }
    let remaining_count_u64 = head.state.count.checked_sub(1).ok_or_else(|| {
        CheckPoError::Corruption("snapshot inventory count underflow".to_string())
    })?;
    let remaining_count = usize::try_from(remaining_count_u64).map_err(|_| {
        CheckPoError::Corruption("snapshot inventory count exceeds platform limits".to_string())
    })?;

    if current_latest != Some(removed) {
        if let Some(latest) = current_latest {
            if !set_root_contains(repo_root, project_id, &root, latest)? {
                return Err(CheckPoError::Corruption(format!(
                    "refs/latest points outside the snapshot inventory: {latest}"
                )));
            }
        }
        return Ok((remaining_count, current_latest.cloned()));
    }
    if remaining_count == 0 {
        return Ok((0, None));
    }
    if let Some(parent) = preferred_parent {
        if parent != removed && set_root_contains(repo_root, project_id, &root, parent)? {
            return Ok((remaining_count, Some(parent.clone())));
        }
    }

    let mut remaining = load_inventory_ids(repo_root, project_id, &head.state)?;
    remaining.remove(removed);
    let mut cursor = head;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(cursor.id.clone()) {
            return Err(CheckPoError::Corruption(
                "snapshot inventory state chain contains a cycle".to_string(),
            ));
        }
        if cursor.state.operation == InventoryOperation::Add {
            let candidate = cursor.state.snapshot_id.as_ref().ok_or_else(|| {
                CheckPoError::Corruption("snapshot inventory Add has no snapshot id".to_string())
            })?;
            if remaining.contains(candidate) {
                return Ok((remaining_count, Some(candidate.clone())));
            }
        }
        let Some(parent_id) = cursor.state.parent.as_ref() else {
            break;
        };
        let parent = load_state(repo_root, parent_id, project_id)?;
        if parent.state.generation.checked_add(1) != Some(cursor.state.generation) {
            return Err(CheckPoError::Corruption(
                "snapshot inventory state generations are not contiguous".to_string(),
            ));
        }
        cursor = parent;
    }
    Err(CheckPoError::Corruption(
        "snapshot inventory cannot identify the newest surviving checkpoint".to_string(),
    ))
}

fn set_root_contains(
    repo_root: &Path,
    project_id: &ProjectId,
    root: &SetRoot,
    snapshot_id: &SnapshotId,
) -> Result<bool> {
    let prefix = snapshot_id.digest_bytes()[0];
    let Some(reference) = root.leaves[usize::from(prefix)].as_ref() else {
        return Ok(false);
    };
    let leaf = load_set_leaf(repo_root, &reference.id, project_id, prefix)?;
    if leaf.ids.len() != reference.count as usize {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory leaf {prefix:02x} count mismatch"
        )));
    }
    Ok(leaf.ids.binary_search(snapshot_id).is_ok())
}

pub(crate) fn add_snapshot_to_inventory_if_head(
    repo_root: &Path,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    expected_head_id: &str,
    operation_id: &str,
) -> Result<String> {
    let root = crate::storage::load_snapshot_root_header(repo_root, snapshot_id)?;
    if &root.project_id != project_id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot {snapshot_id} belongs to project {}, expected {project_id}",
            root.project_id
        )));
    }
    transition_inventory(
        repo_root,
        project_id,
        InventoryOperation::Add,
        snapshot_id,
        expected_head_id,
        operation_id,
    )
}

pub(crate) fn remove_snapshot_from_inventory_if_head(
    repo_root: &Path,
    project_id: &ProjectId,
    snapshot_id: &SnapshotId,
    expected_head_id: &str,
    operation_id: &str,
) -> Result<String> {
    let root_path = crate::snapshot_path(repo_root, snapshot_id);
    match fs::symlink_metadata(&root_path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(crate::io_error(&root_path, error)),
        Ok(_) => {
            return Err(CheckPoError::Corruption(format!(
                "cannot remove {snapshot_id} from inventory while its physical root exists"
            )))
        }
    }
    transition_inventory(
        repo_root,
        project_id,
        InventoryOperation::Remove,
        snapshot_id,
        expected_head_id,
        operation_id,
    )
}

pub(crate) fn validate_physical_snapshot_inventory(
    repo_root: &Path,
    project_id: &ProjectId,
) -> Result<Vec<SnapshotId>> {
    let head = inventory_head(repo_root, project_id)?;
    let inventory_ids = load_inventory_ids(repo_root, project_id, &head.state)?;
    let physical_ids = crate::list_snapshot_ids(repo_root)?;
    let physical = physical_ids.iter().cloned().collect::<BTreeSet<_>>();
    if inventory_ids != physical {
        let missing = inventory_ids
            .difference(&physical)
            .take(8)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let unexpected = physical
            .difference(&inventory_ids)
            .take(8)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory does not match physical roots (missing: [{}], unexpected: [{}])",
            missing.join(", "),
            unexpected.join(", ")
        )));
    }
    if inventory_head(repo_root, project_id)?.id != head.id {
        return Err(CheckPoError::Corruption(
            "snapshot inventory changed while physical roots were inspected".to_string(),
        ));
    }
    Ok(physical_ids)
}

fn transition_inventory(
    repo_root: &Path,
    project_id: &ProjectId,
    operation: InventoryOperation,
    snapshot_id: &SnapshotId,
    expected_head_id: &str,
    operation_id: &str,
) -> Result<String> {
    debug_assert!(operation != InventoryOperation::Initialize);
    if operation_id.is_empty() {
        return Err(CheckPoError::Corruption(
            "snapshot inventory operation id is empty".to_string(),
        ));
    }
    let expected_id = ObjectId::parse(expected_head_id).map_err(|_| {
        CheckPoError::Corruption("snapshot inventory expected head id is invalid".to_string())
    })?;
    let expected = load_state(repo_root, &expected_id, project_id)?;
    let operation_id = inventory_operation_id(operation_id);
    let (set_root, count) = mutate_set(
        repo_root,
        project_id,
        &expected.state,
        operation,
        snapshot_id,
    )?;
    let state = InventoryState {
        project_id: project_id.clone(),
        generation: expected.state.generation.checked_add(1).ok_or_else(|| {
            CheckPoError::Corruption("snapshot inventory generation overflow".to_string())
        })?,
        count,
        operation,
        parent: Some(expected.id.clone()),
        snapshot_id: Some(snapshot_id.clone()),
        operation_id,
        set_root,
    };
    let new_id = state_id(&encode_state(&state))?;
    let current = inventory_head(repo_root, project_id)?;
    if current.id == new_id {
        if current.state != state {
            return Err(CheckPoError::Corruption(
                "snapshot inventory replay state does not match its operation".to_string(),
            ));
        }
        return Ok(new_id.to_string());
    }
    if current.id != expected.id {
        return Err(CheckPoError::WorkingTreeChanged(
            inventory_head_path(repo_root).display().to_string(),
        ));
    }
    let stored = store_state(repo_root, &state)?;
    debug_assert_eq!(stored, new_id);
    // Re-read immediately before the commit point. The repository lock makes
    // this a logical CAS between CheckPo operations; the durable state and set
    // nodes already exist if a crash occurs before the atomic head replace.
    if inventory_head(repo_root, project_id)?.id != expected.id {
        return Err(CheckPoError::WorkingTreeChanged(
            inventory_head_path(repo_root).display().to_string(),
        ));
    }
    write_head(repo_root, &stored)?;
    if inventory_head(repo_root, project_id)?.id != stored {
        return Err(CheckPoError::Corruption(
            "snapshot inventory head commit did not persist".to_string(),
        ));
    }
    Ok(stored.to_string())
}

fn mutate_set(
    repo_root: &Path,
    project_id: &ProjectId,
    state: &InventoryState,
    operation: InventoryOperation,
    snapshot_id: &SnapshotId,
) -> Result<(ObjectId, u64)> {
    let mut root = load_set_root(repo_root, &state.set_root, project_id)?;
    if root.count != state.count {
        return Err(CheckPoError::Corruption(
            "snapshot inventory state count does not match its set root".to_string(),
        ));
    }
    let prefix = snapshot_id.digest_bytes()[0];
    let slot = usize::from(prefix);
    let mut leaf = match &root.leaves[slot] {
        Some(reference) => {
            let leaf = load_set_leaf(repo_root, &reference.id, project_id, prefix)?;
            if leaf.ids.len() != reference.count as usize {
                return Err(CheckPoError::Corruption(format!(
                    "snapshot inventory leaf {prefix:02x} count mismatch"
                )));
            }
            leaf
        }
        None => SetLeaf {
            project_id: project_id.clone(),
            prefix,
            ids: Vec::new(),
        },
    };
    match (operation, leaf.ids.binary_search(snapshot_id)) {
        (InventoryOperation::Add, Ok(_)) => {
            return Err(CheckPoError::Corruption(format!(
                "snapshot inventory adds duplicate root {snapshot_id}"
            )))
        }
        (InventoryOperation::Add, Err(index)) => leaf.ids.insert(index, snapshot_id.clone()),
        (InventoryOperation::Remove, Ok(index)) => {
            leaf.ids.remove(index);
        }
        (InventoryOperation::Remove, Err(_)) => {
            return Err(CheckPoError::Corruption(format!(
                "snapshot inventory removes unknown root {snapshot_id}"
            )))
        }
        (InventoryOperation::Initialize, _) => unreachable!(),
    }
    let count = match operation {
        InventoryOperation::Add => state.count.checked_add(1),
        InventoryOperation::Remove => state.count.checked_sub(1),
        InventoryOperation::Initialize => unreachable!(),
    }
    .ok_or_else(|| CheckPoError::Corruption("snapshot inventory count overflow".to_string()))?;
    root.count = count;
    root.leaves[slot] = if leaf.ids.is_empty() {
        None
    } else {
        let leaf_count = u32::try_from(leaf.ids.len()).map_err(|_| {
            CheckPoError::Corruption(format!(
                "snapshot inventory leaf {prefix:02x} exceeds u32 entries"
            ))
        })?;
        Some(LeafRef {
            id: store_set_leaf(repo_root, &leaf)?,
            count: leaf_count,
        })
    };
    Ok((store_set_root(repo_root, &root)?, count))
}

fn load_inventory_ids(
    repo_root: &Path,
    project_id: &ProjectId,
    state: &InventoryState,
) -> Result<BTreeSet<SnapshotId>> {
    let root = load_set_root(repo_root, &state.set_root, project_id)?;
    if root.count != state.count {
        return Err(CheckPoError::Corruption(
            "snapshot inventory state count does not match its set root".to_string(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut total = 0_u64;
    for (slot, reference) in root.leaves.iter().enumerate() {
        let Some(reference) = reference else {
            continue;
        };
        let prefix = slot as u8;
        let leaf = load_set_leaf(repo_root, &reference.id, project_id, prefix)?;
        if leaf.ids.len() != reference.count as usize {
            return Err(CheckPoError::Corruption(format!(
                "snapshot inventory leaf {prefix:02x} count mismatch"
            )));
        }
        total = total.checked_add(reference.count as u64).ok_or_else(|| {
            CheckPoError::Corruption("snapshot inventory set count overflow".to_string())
        })?;
        for id in leaf.ids {
            if !ids.insert(id.clone()) {
                return Err(CheckPoError::Corruption(format!(
                    "snapshot inventory set contains duplicate root {id}"
                )));
            }
        }
    }
    if total != root.count || ids.len() as u64 != root.count {
        return Err(CheckPoError::Corruption(
            "snapshot inventory set root count mismatch".to_string(),
        ));
    }
    Ok(ids)
}

fn inventory_head(repo_root: &Path, project_id: &ProjectId) -> Result<InventoryHead> {
    validate_inventory_directories(repo_root)?;
    let path = inventory_head_path(repo_root);
    let bytes =
        super::AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, HEAD_BYTES_MAX)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        CheckPoError::Corruption(format!(
            "snapshot inventory head is not UTF-8: {}",
            path.display()
        ))
    })?;
    let id_text = text.strip_suffix('\n').ok_or_else(|| {
        CheckPoError::Corruption(format!(
            "snapshot inventory head is not canonical: {}",
            path.display()
        ))
    })?;
    if id_text.contains(['\r', '\n']) {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory head contains trailing data: {}",
            path.display()
        )));
    }
    let id = ObjectId::parse(id_text).map_err(|_| {
        CheckPoError::Corruption(format!(
            "snapshot inventory head id is invalid: {}",
            path.display()
        ))
    })?;
    load_state(repo_root, &id, project_id)
}

fn load_state(repo_root: &Path, id: &ObjectId, project_id: &ProjectId) -> Result<InventoryHead> {
    let path = inventory_state_path(repo_root, id);
    let bytes =
        super::AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, STATE_BYTES as u64)?;
    if bytes.len() != STATE_BYTES {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory state has invalid length: {}",
            path.display()
        )));
    }
    let actual = state_id(&bytes)?;
    if &actual != id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory state digest mismatch: expected {id}, got {actual}"
        )));
    }
    let state = decode_state(&bytes)?;
    if &state.project_id != project_id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory belongs to project {}, expected {}",
            state.project_id, project_id
        )));
    }
    Ok(InventoryHead {
        id: id.clone(),
        state,
    })
}

fn store_state(repo_root: &Path, state: &InventoryState) -> Result<ObjectId> {
    let bytes = encode_state(state);
    let id = state_id(&bytes)?;
    let path = inventory_state_path(repo_root, &id);
    super::AnchoredRoot::open(repo_root)?
        .store_content_addressed_bytes_profiled(&path, &bytes, None, None, false)?;
    Ok(id)
}

fn load_set_root(repo_root: &Path, id: &ObjectId, project_id: &ProjectId) -> Result<SetRoot> {
    let path = inventory_set_root_path(repo_root, id);
    let bytes =
        super::AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, ROOT_BYTES as u64)?;
    if bytes.len() != ROOT_BYTES {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory set root has invalid length: {}",
            path.display()
        )));
    }
    let actual = set_root_id(&bytes)?;
    if &actual != id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory set root digest mismatch: expected {id}, got {actual}"
        )));
    }
    let root = decode_set_root(&bytes)?;
    if &root.project_id != project_id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory set root belongs to project {}, expected {}",
            root.project_id, project_id
        )));
    }
    Ok(root)
}

fn store_set_root(repo_root: &Path, root: &SetRoot) -> Result<ObjectId> {
    let bytes = encode_set_root(root)?;
    let id = set_root_id(&bytes)?;
    let path = inventory_set_root_path(repo_root, &id);
    super::AnchoredRoot::open(repo_root)?
        .store_content_addressed_bytes_profiled(&path, &bytes, None, None, false)?;
    Ok(id)
}

fn load_set_leaf(
    repo_root: &Path,
    id: &ObjectId,
    project_id: &ProjectId,
    expected_prefix: u8,
) -> Result<SetLeaf> {
    let path = inventory_set_leaf_path(repo_root, id);
    let bytes =
        super::AnchoredRoot::open(repo_root)?.read_bytes_bounded_path(&path, MAX_LEAF_BYTES)?;
    let actual = set_leaf_id(&bytes)?;
    if &actual != id {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory set leaf digest mismatch: expected {id}, got {actual}"
        )));
    }
    let leaf = decode_set_leaf(&bytes)?;
    if &leaf.project_id != project_id || leaf.prefix != expected_prefix {
        return Err(CheckPoError::Corruption(format!(
            "snapshot inventory set leaf {id} has the wrong project or prefix"
        )));
    }
    Ok(leaf)
}

fn store_set_leaf(repo_root: &Path, leaf: &SetLeaf) -> Result<ObjectId> {
    let bytes = encode_set_leaf(leaf)?;
    let id = set_leaf_id(&bytes)?;
    let path = inventory_set_leaf_path(repo_root, &id);
    super::AnchoredRoot::open(repo_root)?
        .store_content_addressed_bytes_profiled(&path, &bytes, None, None, false)?;
    Ok(id)
}

fn write_head(repo_root: &Path, id: &ObjectId) -> Result<()> {
    super::AnchoredRoot::open(repo_root)?.write_bytes_atomic(
        Path::new("inventory/snapshots/head"),
        format!("{id}\n").as_bytes(),
    )
}

fn encode_state(state: &InventoryState) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(STATE_BYTES);
    bytes.extend_from_slice(STATE_MAGIC);
    bytes.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
    bytes.extend_from_slice(&state.project_id.uuid_bytes());
    bytes.extend_from_slice(&state.generation.to_be_bytes());
    bytes.extend_from_slice(&state.count.to_be_bytes());
    bytes.push(match state.operation {
        InventoryOperation::Initialize => 0,
        InventoryOperation::Add => 1,
        InventoryOperation::Remove => 2,
    });
    bytes.push(u8::from(state.parent.is_some()));
    bytes.extend_from_slice(
        &state
            .parent
            .as_ref()
            .map(ObjectId::digest_bytes)
            .unwrap_or([0; 32]),
    );
    bytes.extend_from_slice(
        &state
            .snapshot_id
            .as_ref()
            .map(SnapshotId::digest_bytes)
            .unwrap_or([0; 32]),
    );
    bytes.extend_from_slice(&state.operation_id);
    bytes.extend_from_slice(&state.set_root.digest_bytes());
    debug_assert_eq!(bytes.len(), STATE_BYTES);
    bytes
}

fn decode_state(bytes: &[u8]) -> Result<InventoryState> {
    if bytes.len() != STATE_BYTES || &bytes[..8] != STATE_MAGIC {
        return Err(CheckPoError::Corruption(
            "invalid snapshot inventory state header".to_string(),
        ));
    }
    let schema = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed slice"));
    if schema != SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "snapshot inventory state schema".to_string(),
            found: schema,
            supported: SCHEMA_VERSION,
        });
    }
    let project_id = ProjectId::from_uuid_bytes(bytes[12..28].try_into().expect("fixed slice"));
    let generation = u64::from_be_bytes(bytes[28..36].try_into().expect("fixed slice"));
    let count = u64::from_be_bytes(bytes[36..44].try_into().expect("fixed slice"));
    let operation = match bytes[44] {
        0 => InventoryOperation::Initialize,
        1 => InventoryOperation::Add,
        2 => InventoryOperation::Remove,
        value => {
            return Err(CheckPoError::Corruption(format!(
                "unknown snapshot inventory operation {value}"
            )))
        }
    };
    let parent = match bytes[45] {
        0 if bytes[46..78].iter().all(|byte| *byte == 0) => None,
        1 => Some(ObjectId::from_digest_bytes(
            bytes[46..78].try_into().expect("fixed slice"),
        )),
        _ => {
            return Err(CheckPoError::Corruption(
                "invalid snapshot inventory parent encoding".to_string(),
            ))
        }
    };
    let snapshot_bytes: [u8; 32] = bytes[78..110].try_into().expect("fixed slice");
    let snapshot_id = match operation {
        InventoryOperation::Initialize if snapshot_bytes == [0; 32] => None,
        InventoryOperation::Initialize => {
            return Err(CheckPoError::Corruption(
                "snapshot inventory initialization has a snapshot id".to_string(),
            ))
        }
        InventoryOperation::Add | InventoryOperation::Remove => {
            Some(SnapshotId::from_digest_bytes(snapshot_bytes))
        }
    };
    let operation_id: [u8; 32] = bytes[110..142].try_into().expect("fixed slice");
    let set_root = ObjectId::from_digest_bytes(bytes[142..174].try_into().expect("fixed slice"));
    let valid_shape = match operation {
        InventoryOperation::Initialize => {
            generation == 0
                && count == 0
                && parent.is_none()
                && snapshot_id.is_none()
                && operation_id == [0; 32]
        }
        InventoryOperation::Add => {
            generation > 0
                && count > 0
                && parent.is_some()
                && snapshot_id.is_some()
                && operation_id != [0; 32]
        }
        InventoryOperation::Remove => {
            generation > 0 && parent.is_some() && snapshot_id.is_some() && operation_id != [0; 32]
        }
    };
    if !valid_shape {
        return Err(CheckPoError::Corruption(
            "snapshot inventory state has an invalid shape".to_string(),
        ));
    }
    Ok(InventoryState {
        project_id,
        generation,
        count,
        operation,
        parent,
        snapshot_id,
        operation_id,
        set_root,
    })
}

fn encode_set_root(root: &SetRoot) -> Result<Vec<u8>> {
    if root.leaves.len() != 256 {
        return Err(CheckPoError::Corruption(
            "snapshot inventory set root must have 256 slots".to_string(),
        ));
    }
    let mut bytes = Vec::with_capacity(ROOT_BYTES);
    bytes.extend_from_slice(ROOT_MAGIC);
    bytes.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
    bytes.extend_from_slice(&root.project_id.uuid_bytes());
    bytes.extend_from_slice(&root.count.to_be_bytes());
    for reference in &root.leaves {
        bytes.push(u8::from(reference.is_some()));
        bytes.extend_from_slice(
            &reference
                .as_ref()
                .map(|reference| reference.id.digest_bytes())
                .unwrap_or([0; 32]),
        );
        bytes.extend_from_slice(
            &reference
                .as_ref()
                .map(|value| value.count)
                .unwrap_or(0)
                .to_be_bytes(),
        );
    }
    debug_assert_eq!(bytes.len(), ROOT_BYTES);
    Ok(bytes)
}

fn decode_set_root(bytes: &[u8]) -> Result<SetRoot> {
    if bytes.len() != ROOT_BYTES || &bytes[..8] != ROOT_MAGIC {
        return Err(CheckPoError::Corruption(
            "invalid snapshot inventory set root header".to_string(),
        ));
    }
    let schema = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed slice"));
    if schema != SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "snapshot inventory set root schema".to_string(),
            found: schema,
            supported: SCHEMA_VERSION,
        });
    }
    let project_id = ProjectId::from_uuid_bytes(bytes[12..28].try_into().expect("fixed slice"));
    let count = u64::from_be_bytes(bytes[28..36].try_into().expect("fixed slice"));
    let mut leaves = Vec::with_capacity(256);
    let mut offset = ROOT_HEADER_BYTES;
    for _ in 0..256 {
        let present = bytes[offset];
        let digest: [u8; 32] = bytes[offset + 1..offset + 33]
            .try_into()
            .expect("fixed slice");
        let leaf_count = u32::from_be_bytes(
            bytes[offset + 33..offset + 37]
                .try_into()
                .expect("fixed slice"),
        );
        leaves.push(match (present, digest == [0; 32], leaf_count) {
            (0, true, 0) => None,
            (1, false, count) if count > 0 => Some(LeafRef {
                id: ObjectId::from_digest_bytes(digest),
                count,
            }),
            _ => {
                return Err(CheckPoError::Corruption(
                    "snapshot inventory set root has an invalid slot".to_string(),
                ))
            }
        });
        offset += ROOT_SLOT_BYTES;
    }
    Ok(SetRoot {
        project_id,
        count,
        leaves,
    })
}

fn encode_set_leaf(leaf: &SetLeaf) -> Result<Vec<u8>> {
    if leaf.ids.is_empty() {
        return Err(CheckPoError::Corruption(
            "snapshot inventory set leaf cannot be empty".to_string(),
        ));
    }
    if !leaf.ids.windows(2).all(|pair| pair[0] < pair[1])
        || leaf
            .ids
            .iter()
            .any(|id| id.digest_bytes()[0] != leaf.prefix)
    {
        return Err(CheckPoError::Corruption(
            "snapshot inventory set leaf is not canonical".to_string(),
        ));
    }
    let count = u32::try_from(leaf.ids.len()).map_err(|_| {
        CheckPoError::Corruption("snapshot inventory set leaf exceeds u32 entries".to_string())
    })?;
    let mut bytes = Vec::with_capacity(LEAF_HEADER_BYTES + leaf.ids.len() * 32);
    bytes.extend_from_slice(LEAF_MAGIC);
    bytes.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
    bytes.extend_from_slice(&leaf.project_id.uuid_bytes());
    bytes.push(leaf.prefix);
    bytes.extend_from_slice(&count.to_be_bytes());
    for id in &leaf.ids {
        bytes.extend_from_slice(&id.digest_bytes());
    }
    Ok(bytes)
}

fn decode_set_leaf(bytes: &[u8]) -> Result<SetLeaf> {
    if bytes.len() < LEAF_HEADER_BYTES || &bytes[..8] != LEAF_MAGIC {
        return Err(CheckPoError::Corruption(
            "invalid snapshot inventory set leaf header".to_string(),
        ));
    }
    let schema = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed slice"));
    if schema != SCHEMA_VERSION {
        return Err(CheckPoError::UnsupportedFormat {
            artifact: "snapshot inventory set leaf schema".to_string(),
            found: schema,
            supported: SCHEMA_VERSION,
        });
    }
    let project_id = ProjectId::from_uuid_bytes(bytes[12..28].try_into().expect("fixed slice"));
    let prefix = bytes[28];
    let count = u32::from_be_bytes(bytes[29..33].try_into().expect("fixed slice")) as usize;
    let expected_bytes = LEAF_HEADER_BYTES
        .checked_add(count.checked_mul(32).ok_or_else(|| {
            CheckPoError::Corruption("snapshot inventory set leaf length overflow".to_string())
        })?)
        .ok_or_else(|| {
            CheckPoError::Corruption("snapshot inventory set leaf length overflow".to_string())
        })?;
    if count == 0 || bytes.len() != expected_bytes {
        return Err(CheckPoError::Corruption(
            "snapshot inventory set leaf has an invalid length".to_string(),
        ));
    }
    let mut ids = Vec::with_capacity(count);
    for chunk in bytes[LEAF_HEADER_BYTES..].chunks_exact(32) {
        let id = SnapshotId::from_digest_bytes(chunk.try_into().expect("fixed slice"));
        if id.digest_bytes()[0] != prefix || ids.last().is_some_and(|previous| previous >= &id) {
            return Err(CheckPoError::Corruption(
                "snapshot inventory set leaf is not canonical".to_string(),
            ));
        }
        ids.push(id);
    }
    Ok(SetLeaf {
        project_id,
        prefix,
        ids,
    })
}

fn inventory_operation_id(operation_id: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"checkpo.snapshot-inventory-operation.v2\0");
    hasher.update(operation_id.as_bytes());
    *hasher.finalize().as_bytes()
}

fn state_id(bytes: &[u8]) -> Result<ObjectId> {
    content_id(b"checkpo.snapshot-inventory-state.v2\0", bytes)
}

fn set_root_id(bytes: &[u8]) -> Result<ObjectId> {
    content_id(b"checkpo.snapshot-inventory-set-root.v2\0", bytes)
}

fn set_leaf_id(bytes: &[u8]) -> Result<ObjectId> {
    content_id(b"checkpo.snapshot-inventory-set-leaf.v2\0", bytes)
}

fn content_id(domain: &[u8], bytes: &[u8]) -> Result<ObjectId> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(bytes);
    ObjectId::parse(hasher.finalize().to_hex().as_ref())
}

fn inventory_root(repo_root: &Path) -> PathBuf {
    repo_root.join("inventory").join("snapshots")
}

fn inventory_head_path(repo_root: &Path) -> PathBuf {
    inventory_root(repo_root).join("head")
}

fn inventory_state_path(repo_root: &Path, id: &ObjectId) -> PathBuf {
    inventory_root(repo_root)
        .join("states")
        .join(&id.as_str()[..2])
        .join(format!("{}.state", id.as_str()))
}

fn inventory_set_root_path(repo_root: &Path, id: &ObjectId) -> PathBuf {
    inventory_root(repo_root)
        .join("sets/roots")
        .join(&id.as_str()[..2])
        .join(format!("{}.root", id.as_str()))
}

fn inventory_set_leaf_path(repo_root: &Path, id: &ObjectId) -> PathBuf {
    inventory_root(repo_root)
        .join("sets/leaves")
        .join(&id.as_str()[..2])
        .join(format!("{}.leaf", id.as_str()))
}

fn validate_inventory_directories(repo_root: &Path) -> Result<()> {
    crate::ensure_regular_directory_no_follow(repo_root)?;
    for target in [
        inventory_root(repo_root).join("states"),
        inventory_root(repo_root).join("sets/roots"),
        inventory_root(repo_root).join("sets/leaves"),
    ] {
        let relative = target.strip_prefix(repo_root).map_err(|_| {
            CheckPoError::Corruption(format!(
                "snapshot inventory is outside repository: {}",
                target.display()
            ))
        })?;
        let mut current = repo_root.to_path_buf();
        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                return Err(CheckPoError::Corruption(format!(
                    "snapshot inventory has an unsafe path: {}",
                    target.display()
                )));
            };
            current.push(component);
            crate::ensure_regular_directory_no_follow(&current)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, ProjectId) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(crate::snapshots_dir(&repo)).unwrap();
        let project_id = ProjectId::parse("0123456789abcdef0123456789abcdef").unwrap();
        initialize_snapshot_inventory(&repo, &project_id).unwrap();
        (temp, repo, project_id)
    }

    fn snapshot(byte: u8) -> SnapshotId {
        SnapshotId::from_digest_bytes([byte; 32])
    }

    fn write_physical_root(repo: &Path, id: &SnapshotId) {
        let path = crate::snapshot_path(repo, id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"root").unwrap();
    }

    fn add(repo: &Path, project_id: &ProjectId, id: &SnapshotId, operation_id: &str) -> String {
        let expected = inventory_head_id(repo, project_id).unwrap();
        transition_inventory(
            repo,
            project_id,
            InventoryOperation::Add,
            id,
            &expected,
            operation_id,
        )
        .unwrap()
    }

    #[test]
    fn state_changes_and_reconstructs_exact_membership() {
        let (_temp, repo, project_id) = setup();
        let initial = inventory_head_id(&repo, &project_id).unwrap();
        let first = snapshot(1);
        let second = snapshot(2);
        write_physical_root(&repo, &first);
        let after_first = add(&repo, &project_id, &first, "add-first");
        write_physical_root(&repo, &second);
        let after_second = add(&repo, &project_id, &second, "add-second");
        fs::remove_file(crate::snapshot_path(&repo, &first)).unwrap();
        let after_remove = remove_snapshot_from_inventory_if_head(
            &repo,
            &project_id,
            &first,
            &after_second,
            "remove-first",
        )
        .unwrap();

        assert_ne!(initial, after_first);
        assert_ne!(after_first, after_second);
        assert_ne!(after_second, after_remove);
        assert_eq!(
            validate_physical_snapshot_inventory(&repo, &project_id).unwrap(),
            vec![second]
        );
    }

    #[test]
    fn removal_projection_uses_lineage_then_inventory_generation() {
        let (_temp, repo, project_id) = setup();
        let first = snapshot(21);
        let second = snapshot(22);
        let third = snapshot(23);
        for (id, operation) in [
            (&first, "add-first"),
            (&second, "add-second"),
            (&third, "add-third"),
        ] {
            write_physical_root(&repo, id);
            add(&repo, &project_id, id, operation);
        }

        let projected =
            project_snapshot_removal(&repo, &project_id, &third, Some(&third), Some(&second))
                .unwrap();
        assert_eq!(projected, (2, Some(second.clone())));

        fs::remove_file(crate::snapshot_path(&repo, &second)).unwrap();
        let before_remove = inventory_head_id(&repo, &project_id).unwrap();
        remove_snapshot_from_inventory_if_head(
            &repo,
            &project_id,
            &second,
            &before_remove,
            "remove-second",
        )
        .unwrap();
        let fallback =
            project_snapshot_removal(&repo, &project_id, &third, Some(&third), Some(&second))
                .unwrap();
        assert_eq!(fallback, (1, Some(first)));
    }

    #[test]
    fn committed_transition_replay_is_exact_and_idempotent() {
        let (_temp, repo, project_id) = setup();
        let id = snapshot(3);
        write_physical_root(&repo, &id);
        let expected = inventory_head_id(&repo, &project_id).unwrap();
        let first = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Add,
            &id,
            &expected,
            "same-operation",
        )
        .unwrap();
        let retry = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Add,
            &id,
            &expected,
            "same-operation",
        )
        .unwrap();
        assert_eq!(first, retry);
    }

    #[test]
    fn duplicate_add_and_absent_remove_are_rejected() {
        let (_temp, repo, project_id) = setup();
        let id = snapshot(4);
        write_physical_root(&repo, &id);
        add(&repo, &project_id, &id, "first-add");
        let current = inventory_head_id(&repo, &project_id).unwrap();
        let duplicate = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Add,
            &id,
            &current,
            "second-add",
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("duplicate"));

        let absent = snapshot(5);
        let absent_error = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Remove,
            &absent,
            &current,
            "absent-remove",
        )
        .unwrap_err();
        assert!(absent_error.to_string().contains("unknown"));
    }

    #[test]
    fn stale_expected_head_is_rejected_before_commit() {
        let (_temp, repo, project_id) = setup();
        let stale = inventory_head_id(&repo, &project_id).unwrap();
        let first = snapshot(6);
        let second = snapshot(7);
        write_physical_root(&repo, &first);
        write_physical_root(&repo, &second);
        add(&repo, &project_id, &first, "first");
        let error = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Add,
            &second,
            &stale,
            "stale",
        )
        .unwrap_err();
        assert!(matches!(error, CheckPoError::WorkingTreeChanged(_)));
        assert_eq!(inventory_snapshot_count(&repo, &project_id).unwrap(), 1);
    }

    #[test]
    fn current_state_does_not_depend_on_lifetime_state_chain() {
        let (_temp, repo, project_id) = setup();
        let first = snapshot(8);
        let second = snapshot(9);
        write_physical_root(&repo, &first);
        add(&repo, &project_id, &first, "first");
        write_physical_root(&repo, &second);
        add(&repo, &project_id, &second, "second");

        let head = inventory_head(&repo, &project_id).unwrap();
        let parent = head.state.parent.expect("non-initial state has a parent");
        fs::remove_file(inventory_state_path(&repo, &parent)).unwrap();

        assert_eq!(
            validate_physical_snapshot_inventory(&repo, &project_id).unwrap(),
            vec![first, second]
        );
    }

    #[test]
    fn same_count_physical_substitution_is_detected() {
        let (_temp, repo, project_id) = setup();
        let tracked = snapshot(10);
        let replacement = snapshot(11);
        write_physical_root(&repo, &tracked);
        add(&repo, &project_id, &tracked, "tracked");
        fs::remove_file(crate::snapshot_path(&repo, &tracked)).unwrap();
        write_physical_root(&repo, &replacement);

        let error = validate_physical_snapshot_inventory(&repo, &project_id).unwrap_err();
        assert!(error.to_string().contains("missing"));
        assert!(error.to_string().contains("unexpected"));
    }

    #[test]
    fn corrupt_content_addressed_state_and_set_nodes_are_rejected() {
        let (_temp, repo, project_id) = setup();
        let id = snapshot(12);
        write_physical_root(&repo, &id);
        add(&repo, &project_id, &id, "add");
        let head = inventory_head(&repo, &project_id).unwrap();
        let state_path = inventory_state_path(&repo, &head.id);
        let mut state_bytes = fs::read(&state_path).unwrap();
        state_bytes[STATE_BYTES - 1] ^= 1;
        fs::write(&state_path, state_bytes).unwrap();
        assert!(inventory_head_id(&repo, &project_id)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch"));
    }

    #[test]
    fn corrupt_head_is_rejected() {
        let (_temp, repo, project_id) = setup();
        fs::write(inventory_head_path(&repo), b"not-an-id\n").unwrap();
        assert!(inventory_head_id(&repo, &project_id).is_err());
        initialize_snapshot_inventory(&repo, &project_id).unwrap_err();
    }

    #[test]
    fn generation_overflow_leaves_the_committed_head_unchanged() {
        let (_temp, repo, project_id) = setup();
        let first = snapshot(13);
        let second = snapshot(14);
        write_physical_root(&repo, &first);
        add(&repo, &project_id, &first, "first");

        let mut saturated = inventory_head(&repo, &project_id).unwrap().state;
        saturated.generation = u64::MAX;
        let saturated_id = store_state(&repo, &saturated).unwrap();
        write_head(&repo, &saturated_id).unwrap();
        let committed = inventory_head_id(&repo, &project_id).unwrap();

        let error = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Add,
            &second,
            &committed,
            "overflow-generation",
        )
        .unwrap_err();
        assert!(error.to_string().contains("generation overflow"));
        assert_eq!(inventory_head_id(&repo, &project_id).unwrap(), committed);
    }

    #[test]
    fn count_overflow_leaves_the_committed_head_unchanged() {
        let (_temp, repo, project_id) = setup();
        let initial = inventory_head(&repo, &project_id).unwrap();
        let saturated_root = SetRoot {
            project_id: project_id.clone(),
            count: u64::MAX,
            leaves: vec![None; 256],
        };
        let saturated_root_id = store_set_root(&repo, &saturated_root).unwrap();
        let saturated = InventoryState {
            project_id: project_id.clone(),
            generation: 1,
            count: u64::MAX,
            operation: InventoryOperation::Add,
            parent: Some(initial.id),
            snapshot_id: Some(snapshot(15)),
            operation_id: inventory_operation_id("saturated-count"),
            set_root: saturated_root_id,
        };
        let saturated_id = store_state(&repo, &saturated).unwrap();
        write_head(&repo, &saturated_id).unwrap();
        let committed = inventory_head_id(&repo, &project_id).unwrap();

        let error = transition_inventory(
            &repo,
            &project_id,
            InventoryOperation::Add,
            &snapshot(16),
            &committed,
            "overflow-count",
        )
        .unwrap_err();
        assert!(error.to_string().contains("count overflow"));
        assert_eq!(inventory_head_id(&repo, &project_id).unwrap(), committed);
    }
}
