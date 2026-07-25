use super::*;

pub(super) const QUARANTINE_RECORD_SCHEMA_VERSION_V1: u32 = 1;
pub(super) const MAX_QUARANTINE_RECORD_BYTES: u64 = 1024 * 1024;
pub(super) const RECOVERY_RESCUE_RECORD_SCHEMA_VERSION: u32 = 1;
pub(super) const RECOVERY_EXPORT_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_RECOVERY_RESCUE_RECORD_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const RECOVERY_EXPORT_MANIFEST_FILE: &str = "CheckPo-Recovery.json";
pub(super) const RECOVERY_EXPORT_COMPLETE_FILE: &str = "保存が完了しました.txt";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum RecoveryRescueState {
    Prepared,
    Resolving,
    Recovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RecoveryRescueEntry {
    pub(super) conflict: TransactionRecoveryConflict,
    pub(super) exported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RecoveryRescueRecord {
    pub(super) schema_version: u32,
    pub(super) transaction_id: String,
    pub(super) checkpoint_id: SnapshotId,
    pub(super) plan_id: String,
    pub(super) created_at_utc: String,
    pub(super) state: RecoveryRescueState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) export_directory: Option<PathBuf>,
    pub(super) entries: Vec<RecoveryRescueEntry>,
    pub(super) completed_paths: Vec<TrackedUnityFilePath>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RecoveryExportManifest<'a> {
    pub(super) schema_version: u32,
    pub(super) transaction_id: &'a str,
    pub(super) created_at_utc: String,
    pub(super) files: Vec<&'a TransactionRecoveryConflict>,
}

pub(super) struct RecoveryExportStage {
    pub(super) export_root: PathBuf,
    pub(super) staging_name: std::ffi::OsString,
    pub(super) final_name: std::ffi::OsString,
    pub(super) staging_directory: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuarantineRecordEnvelope {
    pub(super) schema_version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuarantineRecord {
    pub(super) schema_version: u32,
    pub(super) transaction_id: String,
    pub(super) quarantined_at_utc: String,
    pub(super) original_journal_path: PathBuf,
    pub(super) project_was_verified_in_before_state: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resolved_at_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resolved_checkpoint_id: Option<SnapshotId>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct QuarantineResolutionRecord {
    pub(super) schema_version: u32,
    pub(super) resolved_at_utc: String,
    pub(super) resolved_checkpoint_id: SnapshotId,
    pub(super) quarantine_record_digest: String,
}
