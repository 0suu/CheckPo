use checkpo_core as core;
use serde::Serialize;
use serde_json::{json, Value};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::{Error as UpdaterError, Update, UpdaterExt};

type ProgressFn = Box<dyn Fn(core::OperationProgress) + Send + Sync>;
type AppResult = Result<Value, AppError>;
const DEFAULT_INITIAL_CHECKPOINT_NAME: &str = "初回チェックポイント";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppError {
    kind: &'static str,
    message: String,
}

impl AppError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Default)]
struct OperationState {
    current: Arc<Mutex<Option<RunningOperation>>>,
}

struct RunningOperation {
    cancellation: Option<core::CancellationToken>,
}

#[derive(Default)]
struct PendingUpdate(Mutex<Option<Update>>);

#[tauri::command]
async fn pick_folder(title: Option<String>) -> Value {
    let mut dialog = rfd::AsyncFileDialog::new();
    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        dialog = dialog.set_title(title);
    }
    dialog
        .pick_folder()
        .await
        .map(|path| json!({ "path": path.path().to_string_lossy() }))
        .unwrap_or(Value::Null)
}

#[tauri::command]
async fn load_project(state: tauri::State<'_, OperationState>, project_path: String) -> AppResult {
    run_guarded_blocking(state, None, move || project_snapshot(project_path)).await
}

#[tauri::command]
async fn confirm_project_location(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::confirm_project_location(&project_path).map_err(to_app_error)?;
        project_snapshot(project_path)
    })
    .await
}

#[tauri::command]
async fn init_project(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    storage_root_path: Option<String>,
    create_initial_checkpoint: Option<bool>,
    initial_checkpoint_name: Option<String>,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        match storage_root_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        {
            Some(storage_root_path) => {
                core::init_project_with_storage_root(&project_path, storage_root_path)
            }
            None => core::init_project(&project_path),
        }
        .map_err(to_app_error)?;
        project_snapshot_after_start(
            project_path,
            create_initial_checkpoint.unwrap_or(false),
            initial_checkpoint_name,
            token,
            progress,
        )
    })
    .await
}

#[tauri::command]
async fn start_as_separate_project(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    storage_root_path: Option<String>,
    confirmed: bool,
    create_initial_checkpoint: Option<bool>,
    initial_checkpoint_name: Option<String>,
) -> AppResult {
    require_confirmation(
        confirmed,
        "starting as a separate project requires confirmation.",
    )?;
    run_cancellable_blocking(app, state, move |token, progress| {
        let project = core::load_project(&project_path).map_err(to_app_error)?;
        if project.location_status != core::ProjectLocationStatus::CopiedSuspected {
            return Err(AppError::new(
                "invalidOperation",
                "starting as a separate project is only allowed for copied-project warnings.",
            ));
        }
        match storage_root_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        {
            Some(storage_root_path) => {
                core::start_as_separate_project_with_storage_root(&project_path, storage_root_path)
            }
            None => core::start_as_separate_project(&project_path),
        }
        .map_err(to_app_error)?;
        project_snapshot_after_start(
            project_path,
            create_initial_checkpoint.unwrap_or(false),
            initial_checkpoint_name,
            token,
            progress,
        )
    })
    .await
}

#[tauri::command]
async fn set_storage_root(
    state: tauri::State<'_, OperationState>,
    project_path: String,
    storage_root_path: String,
    confirmed: bool,
) -> AppResult {
    require_confirmation(confirmed, "storage root change requires confirmation.")?;
    run_guarded_blocking(state, None, move || {
        core::set_project_storage_root(&project_path, &storage_root_path).map_err(to_app_error)?;
        project_snapshot(project_path)
    })
    .await
}

#[tauri::command]
async fn refresh_project(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || project_snapshot(project_path)).await
}

#[tauri::command]
async fn create_checkpoint(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    name: String,
    init_if_needed: Option<bool>,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        core::create_checkpoint(
            &project_path,
            &name,
            core::CreateCheckpointOptions {
                init_if_needed: init_if_needed.unwrap_or(false),
                progress: Some(std::sync::Arc::from(progress)),
                cancellation: Some(token),
            },
        )
        .map(|summary| json!(summary))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn list_checkpoints(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::list_checkpoints(&project_path)
            .map(|items| json!(items))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn delete_checkpoint(
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
    confirmed: bool,
) -> AppResult {
    require_confirmation(confirmed, "checkpoint delete requires confirmation.")?;
    run_guarded_blocking(state, None, move || {
        core::delete_checkpoint(&project_path, &checkpoint_id)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn rename_checkpoint(
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
    name: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::rename_checkpoint(&project_path, &checkpoint_id, &name)
            .map(|summary| json!(summary))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn open_project_in_file_manager(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        let project = core::load_project(&project_path).map_err(to_app_error)?;
        let path = project.project_root.as_path();
        if !path.is_dir() {
            return Err(AppError::new(
                "notFound",
                format!("project folder was not found: {}", path.display()),
            ));
        }
        open_folder_in_file_manager(path)?;
        Ok(json!({ "path": path }))
    })
    .await
}

#[tauri::command]
async fn diff_checkpoint(
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::diff_checkpoint(&project_path, &checkpoint_id)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn diff_checkpoint_metadata(
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::diff_checkpoint_metadata(&project_path, &checkpoint_id)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn diff_checkpoint_full(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        core::diff_checkpoint_with_options(
            &project_path,
            &checkpoint_id,
            core::DiffOptions {
                progress: Some(std::sync::Arc::from(progress)),
                cancellation: Some(token),
            },
        )
        .map(|result| json!(result))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn preview_restore(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        core::preview_restore_with_progress_and_cancellation(
            &project_path,
            &checkpoint_id,
            Some(progress.as_ref()),
            Some(&token),
        )
        .map(|plan| json!(plan))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn apply_restore(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: String,
    expected_plan: core::OperationPlan,
    confirmed: bool,
) -> AppResult {
    require_confirmation(confirmed, "restore apply requires confirmation.")?;
    run_cancellable_blocking(app, state, move |token, progress| {
        core::apply_restore_plan_with_progress_and_cancellation(
            &project_path,
            &checkpoint_id,
            expected_plan,
            core::ApplyOptions { yes: true },
            Some(progress.as_ref()),
            Some(&token),
        )
        .map(|result| json!(result))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn preview_discard_files(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    paths: Vec<String>,
    checkpoint_id: Option<String>,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        let tracked = core::parse_tracked_paths(&paths).map_err(to_app_error)?;
        core::preview_discard_with_progress_and_cancellation(
            &project_path,
            checkpoint_id.as_deref(),
            &tracked,
            Some(progress.as_ref()),
            Some(&token),
        )
        .map(|plan| json!(plan))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn apply_discard_files(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    paths: Vec<String>,
    checkpoint_id: Option<String>,
    expected_plan: core::OperationPlan,
    confirmed: bool,
) -> AppResult {
    require_confirmation(confirmed, "discard apply requires confirmation.")?;
    run_cancellable_blocking(app, state, move |token, progress| {
        let tracked = core::parse_tracked_paths(&paths).map_err(to_app_error)?;
        core::apply_discard_plan_with_progress_and_cancellation(
            &project_path,
            checkpoint_id.as_deref(),
            &tracked,
            expected_plan,
            core::ApplyOptions { yes: true },
            Some(progress.as_ref()),
            Some(&token),
        )
        .map(|result| json!(result))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn verify_project(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
    checkpoint_id: Option<String>,
    full: Option<bool>,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        match checkpoint_id {
            Some(checkpoint_id) => core::verify_checkpoint_with_progress_and_cancellation(
                &project_path,
                &checkpoint_id,
                full.unwrap_or(true),
                Some(progress.as_ref()),
                Some(&token),
            ),
            None => core::verify_project_with_progress_and_cancellation(
                &project_path,
                full.unwrap_or(true),
                Some(progress.as_ref()),
                Some(&token),
            ),
        }
        .map(|result| json!(result))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn rebuild_index(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_cancellable_blocking(app, state, move |token, progress| {
        let project = core::load_project(&project_path).map_err(to_app_error)?;
        core::rebuild_index_for_project_with_progress_and_cancellation(
            &project,
            Some(progress.as_ref()),
            Some(&token),
        )
        .map(|result| json!(result))
        .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn analyze_gc(state: tauri::State<'_, OperationState>, project_path: String) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::analyze_gc(&project_path)
            .map(|plan| json!(plan))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn apply_gc(
    state: tauri::State<'_, OperationState>,
    project_path: String,
    confirmed: bool,
) -> AppResult {
    require_confirmation(confirmed, "storage gc apply requires confirmation.")?;
    run_guarded_blocking(state, None, move || {
        core::apply_gc(&project_path)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn list_transactions(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::pending_transactions(&project_path)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn recover_transactions(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::recover_transactions(&project_path)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
async fn cleanup_journals(
    state: tauri::State<'_, OperationState>,
    project_path: String,
) -> AppResult {
    run_guarded_blocking(state, None, move || {
        core::cleanup_journals(&project_path)
            .map(|result| json!(result))
            .map_err(to_app_error)
    })
    .await
}

#[tauri::command]
fn cancel_current_operation(state: tauri::State<'_, OperationState>) -> AppResult {
    let token = state
        .current
        .lock()
        .map_err(|_| {
            AppError::new(
                "operationStatePoisoned",
                "Operation state lock is poisoned.",
            )
        })?
        .as_ref()
        .and_then(|operation| operation.cancellation.clone());
    if let Some(token) = token {
        token.cancel();
        Ok(json!({ "cancelled": true }))
    } else {
        Ok(json!({ "cancelled": false }))
    }
}

#[tauri::command]
async fn check_for_update(
    app: AppHandle,
    pending_update: tauri::State<'_, PendingUpdate>,
) -> AppResult {
    let update = app
        .updater()
        .map_err(to_update_error)?
        .check()
        .await
        .map_err(to_update_error)?;
    let metadata = update.as_ref().map(|update| {
        json!({
            "version": update.version,
            "currentVersion": update.current_version,
        })
    });
    *pending_update
        .0
        .lock()
        .map_err(|_| AppError::new("updateStatePoisoned", "Update state lock is poisoned."))? =
        update;
    Ok(metadata.unwrap_or(Value::Null))
}

#[tauri::command]
async fn install_update(
    app: AppHandle,
    pending_update: tauri::State<'_, PendingUpdate>,
) -> AppResult {
    let update = pending_update
        .0
        .lock()
        .map_err(|_| AppError::new("updateStatePoisoned", "Update state lock is poisoned."))?
        .take()
        .ok_or_else(|| AppError::new("updateNotFound", "No pending update is available."))?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(to_update_error)?;
    app.restart();
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(OperationState::default())
        .manage(PendingUpdate::default())
        .invoke_handler(tauri::generate_handler![
            pick_folder,
            load_project,
            confirm_project_location,
            init_project,
            start_as_separate_project,
            set_storage_root,
            refresh_project,
            create_checkpoint,
            list_checkpoints,
            delete_checkpoint,
            rename_checkpoint,
            open_project_in_file_manager,
            diff_checkpoint,
            diff_checkpoint_metadata,
            diff_checkpoint_full,
            preview_restore,
            apply_restore,
            preview_discard_files,
            apply_discard_files,
            verify_project,
            rebuild_index,
            analyze_gc,
            apply_gc,
            list_transactions,
            recover_transactions,
            cleanup_journals,
            cancel_current_operation,
            check_for_update,
            install_update
        ])
        .run(tauri::generate_context!())
        .expect("failed to run CheckPo Tauri app");
}

fn to_update_error(error: UpdaterError) -> AppError {
    match error {
        UpdaterError::TargetNotFound(target) => AppError::new(
            "updateTargetNotFound",
            format!("このOS/CPU向けの更新ファイルが latest.json にありません。target: {target}"),
        ),
        UpdaterError::TargetsNotFound(targets) => AppError::new(
            "updateTargetNotFound",
            format!(
                "このOS/CPU向けの更新ファイルが latest.json にありません。候補: {}",
                targets.join(", ")
            ),
        ),
        error => AppError::new("updater", error.to_string()),
    }
}

fn open_folder_in_file_manager(path: &std::path::Path) -> Result<(), AppError> {
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("explorer.exe");
        command.arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| AppError::new("io", error.to_string()))
}

fn project_snapshot(project_path: String) -> AppResult {
    let context = core::load_project(&project_path).map_err(to_app_error)?;
    let project = core::project_view(&context).map_err(to_app_error)?;
    let pending_transactions =
        core::pending_transactions_for_project(&context).map_err(to_app_error)?;
    let mut warnings = Vec::new();
    let checkpoints = match core::list_checkpoints_for_project(&context) {
        Ok(checkpoints) => checkpoints,
        Err(error)
            if context.location_status == core::ProjectLocationStatus::CopiedSuspected
                || !pending_transactions.is_empty() =>
        {
            warnings.push(format!("チェックポイント一覧を読み込めません: {error}"));
            Vec::new()
        }
        Err(error) => return Err(to_app_error(error)),
    };
    let storage = match core::storage_summary_from_index(&context) {
        Ok(storage) => Some(storage),
        Err(error)
            if context.location_status == core::ProjectLocationStatus::CopiedSuspected
                || !pending_transactions.is_empty() =>
        {
            let prefix = if pending_transactions.is_empty() {
                "保存容量の集計を読み込めません"
            } else {
                "復旧が完了するまで保存容量の集計を読み込めません"
            };
            warnings.push(format!("{prefix}: {error}"));
            None
        }
        Err(error) => return Err(to_app_error(error)),
    };
    Ok(json!({
        "project": project,
        "projectPath": project_path,
        "checkpoints": checkpoints,
        "storage": storage,
        "pendingTransactions": pending_transactions,
        "warnings": warnings
    }))
}

fn project_snapshot_after_start(
    project_path: String,
    create_initial_checkpoint: bool,
    initial_checkpoint_name: Option<String>,
    token: core::CancellationToken,
    progress: ProgressFn,
) -> AppResult {
    let initial_checkpoint_result = if create_initial_checkpoint {
        Some(core::create_checkpoint(
            &project_path,
            &initial_checkpoint_name_or_default(initial_checkpoint_name),
            core::CreateCheckpointOptions {
                init_if_needed: false,
                progress: Some(std::sync::Arc::from(progress)),
                cancellation: Some(token),
            },
        ))
    } else {
        None
    };
    let mut snapshot = project_snapshot(project_path)?;
    match initial_checkpoint_result {
        Some(Ok(summary)) => {
            if let Value::Object(map) = &mut snapshot {
                map.insert("initialCheckpoint".to_string(), json!(summary));
            }
        }
        Some(Err(core::CheckPoError::Cancelled)) => {
            if let Value::Object(map) = &mut snapshot {
                map.insert("initialCheckpointCancelled".to_string(), json!(true));
            }
        }
        Some(Err(error)) => {
            if let Value::Object(map) = &mut snapshot {
                let error = to_app_error(error);
                let message = format!(
                    "プロジェクトは開始しましたが、初回チェックポイント作成に失敗しました: {error}"
                );
                map.insert("initialCheckpointError".to_string(), json!(error));
                match map.get_mut("warnings").and_then(Value::as_array_mut) {
                    Some(warnings) => warnings.push(json!(message)),
                    None => {
                        map.insert("warnings".to_string(), json!([message]));
                    }
                }
            }
        }
        None => {}
    }
    Ok(snapshot)
}

fn initial_checkpoint_name_or_default(value: Option<String>) -> String {
    value
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_INITIAL_CHECKPOINT_NAME.to_string())
}

async fn run_guarded_blocking<F>(
    state: tauri::State<'_, OperationState>,
    cancellation: Option<core::CancellationToken>,
    operation: F,
) -> AppResult
where
    F: FnOnce() -> AppResult + Send + 'static,
{
    let guard = OperationGuard::begin(&state, cancellation)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        operation()
    })
    .await;
    result.map_err(|error| AppError::new("taskJoinError", error.to_string()))?
}

async fn run_cancellable_blocking<F>(
    app: AppHandle,
    state: tauri::State<'_, OperationState>,
    operation: F,
) -> AppResult
where
    F: FnOnce(core::CancellationToken, ProgressFn) -> AppResult + Send + 'static,
{
    let token = core::CancellationToken::new();
    run_guarded_blocking(state, Some(token.clone()), move || {
        operation(token, Box::new(progress_emitter(app)))
    })
    .await
}

struct OperationGuard {
    state: OperationState,
}

impl OperationGuard {
    fn begin(
        state: &OperationState,
        cancellation: Option<core::CancellationToken>,
    ) -> Result<Self, AppError> {
        let mut current = state.current.lock().map_err(|_| {
            AppError::new(
                "operationStatePoisoned",
                "Operation state lock is poisoned.",
            )
        })?;
        if current.is_some() {
            return Err(AppError::new(
                "operationBusy",
                "Another operation is already running.",
            ));
        }
        *current = Some(RunningOperation { cancellation });
        Ok(Self {
            state: state.clone(),
        })
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Ok(mut current) = self.state.current.lock() {
            *current = None;
        }
    }
}

fn to_app_error(error: core::CheckPoError) -> AppError {
    let kind = match &error {
        core::CheckPoError::User(_) => "user",
        core::CheckPoError::InvalidProject(_) => "invalidProject",
        core::CheckPoError::InvalidTrackedPath(_) => "invalidTrackedPath",
        core::CheckPoError::InvalidId(_) => "invalidId",
        core::CheckPoError::OutsideTrackedScope(_) => "outsideTrackedScope",
        core::CheckPoError::SnapshotNotFound(_) => "snapshotNotFound",
        core::CheckPoError::ObjectMissing(_) => "objectMissing",
        core::CheckPoError::ObjectHashMismatch(_) => "objectHashMismatch",
        core::CheckPoError::WorkingTreeChanged(_) => "workingTreeChanged",
        core::CheckPoError::RepositoryLocked(_) => "repositoryLocked",
        core::CheckPoError::PendingTransaction(_) => "pendingTransaction",
        core::CheckPoError::IndexUnavailable(_) => "indexUnavailable",
        core::CheckPoError::CopiedProjectSuspected(_) => "copiedProjectSuspected",
        core::CheckPoError::Cancelled => "cancelled",
        core::CheckPoError::Io { .. } => "io",
        core::CheckPoError::Json { .. } => "json",
        core::CheckPoError::Db { .. } => "database",
        core::CheckPoError::Corruption(_) => "corruption",
        core::CheckPoError::Unexpected(_) => "unexpected",
    };
    AppError::new(kind, error.to_string())
}

fn require_confirmation(confirmed: bool, message: &str) -> Result<(), AppError> {
    if confirmed {
        Ok(())
    } else {
        Err(AppError::new("confirmationRequired", message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_project_error_is_structured() {
        let error = to_app_error(core::CheckPoError::CopiedProjectSuspected(
            "This Unity project appears to be a copy.".to_string(),
        ));

        assert_eq!(error.kind, "copiedProjectSuspected");
        assert_eq!(error.message, "This Unity project appears to be a copy.");
    }

    #[test]
    fn core_errors_are_classified_for_tauri() {
        let cancelled = to_app_error(core::CheckPoError::Cancelled);
        let invalid = to_app_error(core::CheckPoError::InvalidProject(
            "missing marker".to_string(),
        ));

        assert_eq!(cancelled.kind, "cancelled");
        assert_eq!(cancelled.message, "operation cancelled");
        assert_eq!(invalid.kind, "invalidProject");
        assert_eq!(invalid.message, "invalid Unity project: missing marker");
    }

    #[test]
    fn frontend_diff_routes_keep_fast_diff_to_open_and_focus_only() {
        let app_js = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend/app.js"),
        )
        .unwrap();

        assert!(app_js.contains(
            r#"const diffCommand = options.metadataOnly ? "diff_checkpoint_metadata" : "diff_checkpoint";"#
        ));
        assert!(app_js.contains(r#"refreshLatestDiff({ silent: true, metadataOnly: true });"#));
        assert!(!app_js.contains(
            r#"refreshLatestDiff({ refreshProject: true, silent: true, metadataOnly: true });"#
        ));
        assert_eq!(
            app_js
                .matches(r#"refreshLatestDiff({ allowBusy: true, metadataOnly: true });"#)
                .count(),
            2
        );
        assert!(app_js.contains(r#"invokeCommand("diff_checkpoint_full""#));
        assert!(app_js.contains(r#"await refreshLatestDiff({ allowBusy: true });"#));
    }
}

fn progress_emitter(app: AppHandle) -> impl Fn(core::OperationProgress) + Send + Sync + 'static {
    let state = Mutex::new(ProgressEmitState::default());
    move |progress| {
        let Ok(mut state) = state.lock() else {
            let _ = app.emit("operation-progress", progress);
            return;
        };
        if state.should_emit(&progress) {
            let _ = app.emit("operation-progress", progress);
        }
    }
}

#[derive(Default)]
struct ProgressEmitState {
    last_emit_at: Option<Instant>,
    last_phase: Option<String>,
    last_total: usize,
    last_percent: Option<usize>,
}

impl ProgressEmitState {
    fn should_emit(&mut self, progress: &core::OperationProgress) -> bool {
        let now = Instant::now();
        let percent =
            (progress.total > 0).then(|| progress.completed.saturating_mul(100) / progress.total);
        let phase_changed = self.last_phase.as_deref() != Some(progress.phase.as_str());
        let total_changed = progress.total != self.last_total;
        let completed = progress.phase == "complete"
            || (progress.total > 0 && progress.completed >= progress.total);
        let percent_advanced = percent
            .zip(self.last_percent)
            .is_some_and(|(current, previous)| current > previous);
        let elapsed = self
            .last_emit_at
            .map(|last| now.duration_since(last) >= Duration::from_millis(80))
            .unwrap_or(true);
        let should_emit =
            phase_changed || total_changed || completed || percent_advanced || elapsed;
        if should_emit {
            self.last_emit_at = Some(now);
            self.last_phase = Some(progress.phase.clone());
            self.last_total = progress.total;
            self.last_percent = percent;
        }
        should_emit
    }
}
