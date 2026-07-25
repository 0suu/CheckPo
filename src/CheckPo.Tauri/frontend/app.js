const tauriInvoke = window.__TAURI__?.core?.invoke;
const tauriListen = window.__TAURI__?.event?.listen;
const RESULT_OUTPUT_MAX_CHARS = 20000;
const ROLLBACK_OPERATION_RENDER_LIMIT = 500;
const CONFIRM_PATH_RENDER_LIMIT = 30;
const OPERATION_BUSY_RETRY_DELAYS_MS = [150, 300, 600, 1000, 1500];
const AUTO_REFRESH_WAIT_INTERVAL_MS = 100;
const AUTO_REFRESH_PREEMPT_WAIT_TIMEOUT_MS = 1000;

function readLocalSetting(key, fallback = "") {
  const current = localStorage.getItem(`checkPo.${key}`);
  if (current !== null) return current;
  const legacy = localStorage.getItem(`avatarCheckpoint.${key}`);
  if (legacy !== null) {
    localStorage.setItem(`checkPo.${key}`, legacy);
    return legacy;
  }
  return fallback;
}

function writeLocalSetting(key, value) {
  localStorage.setItem(`checkPo.${key}`, value);
}

function removeLocalSetting(key) {
  localStorage.removeItem(`checkPo.${key}`);
  localStorage.removeItem(`avatarCheckpoint.${key}`);
}

const state = {
  language: readLocalSetting("language", "ja"),
  theme: readLocalSetting("theme", "system"),
  defaultStorageRootPath: readLocalSetting("defaultStorageRootPath"),
  projectPath: "",
  project: null,
  projectLocationStatus: "current",
  projectWarnings: [],
  projectHistory: readProjectHistory(),
  hiddenProjectPaths: new Set(),
  checkpointIndex: { state: "current", rebuildable: false, detail: null },
  checkpoints: [],
  selectedCheckpointId: null,
  renamingCheckpointId: null,
  storage: null,
  storageSizeLoadedProjectPath: null,
  storageSizeLoadingProjectPath: null,
  gcPlan: null,
  tempCleanupPlan: null,
  transactionCleanupPlan: null,
  rollbackPlan: null,
  rollbackPlanContext: null,
  rollbackRequestSerial: 0,
  pendingTransactions: [],
  unresolvedQuarantines: [],
  failedTransactions: [],
  recoveryConflictPlan: null,
  confirming: false,
  currentDiff: null,
  diffRefreshFailure: null,
  latestDiffCheckpointId: null,
  latestDiffChangeCount: null,
  latestDiffExact: false,
  latestDiffRefreshFailure: null,
  latestDiffWarnings: [],
  snapshotWarnings: [],
  operationWarnings: [],
  diffRequestSerial: 0,
  currentDiffFilter: "all",
  diffTreeOpenPaths: new Set(),
  diffTreeTouched: false,
  currentDiffSelectedPaths: new Set(),
  lastSelectedChangePath: null,
  busy: false,
  autoRefreshInFlight: false,
  autoRefreshGeneration: 0,
  queuedDiffRefreshOptions: null,
  lastAutoRefreshAt: 0,
  userOperationSerial: 0,
  activeCommand: null,
  cancelRequested: false,
  currentOperationCancellable: false,
  pendingProgress: null,
  progressFrame: null,
  availableUpdate: null,
};

const $ = (id) => document.getElementById(id);

function readProjectHistory() {
  try {
    const parsed = JSON.parse(readLocalSetting("projects", "[]"));
    return Array.isArray(parsed) ? parsed.filter((item) => item?.path) : [];
  } catch (_) {
    return [];
  }
}

function writeProjectHistory() {
  writeLocalSetting("projects", JSON.stringify(state.projectHistory.slice(0, 12)));
}

function forgetProjectFromHistory(projectPath) {
  state.projectHistory = CheckPoFrontendState.removeProjectFromHistory(
    state.projectHistory,
    projectPath,
  );
  state.hiddenProjectPaths.add(projectPath);
  if (readLocalSetting("lastProjectPath") === projectPath) {
    removeLocalSetting("lastProjectPath");
  }
  writeProjectHistory();
  renderProjectHistory();
  setStatus("プロジェクトを一覧から消しました。チェックポイントやプロジェクトのファイルは削除していません。");
}

function setDefaultStorageRootPath(path) {
  state.defaultStorageRootPath = String(path || "").trim();
  if (state.defaultStorageRootPath) {
    writeLocalSetting("defaultStorageRootPath", state.defaultStorageRootPath);
  } else {
    removeLocalSetting("defaultStorageRootPath");
  }
  renderDefaultStorageRootPath();
}

function renderDefaultStorageRootPath() {
  const value = state.defaultStorageRootPath || "";
  if ($("settingsDefaultStorageRootPath")) $("settingsDefaultStorageRootPath").value = value;
  if (!$("projectRegistrationOverlay")?.hidden && !$("registrationStorageRootPath").value.trim()) {
    $("registrationStorageRootPath").value = value;
  }
}

function t(key) {
  return messages[state.language]?.[key] || messages.ja[key] || key;
}

function tf(key, values = {}) {
  return Object.entries(values).reduce(
    (text, [name, value]) => text.replaceAll(`{${name}}`, String(value)),
    t(key),
  );
}

function applyI18n() {
  document.querySelectorAll("[data-i18n]").forEach((element) => {
    element.textContent = t(element.dataset.i18n);
  });
  document.querySelectorAll("[data-i18n-placeholder]").forEach((element) => {
    element.placeholder = t(element.dataset.i18nPlaceholder);
  });
  document.querySelectorAll("[data-i18n-title]").forEach((element) => {
    element.title = t(element.dataset.i18nTitle);
  });
  document.querySelectorAll("[data-i18n-aria-label]").forEach((element) => {
    element.setAttribute("aria-label", t(element.dataset.i18nAriaLabel));
  });
  updateThemeControls();
}

async function invokeCommand(command, args = {}, options = {}) {
  if (!tauriInvoke) {
    throw new Error("Tauri invoke API is not available.");
  }
  const trackOperation = command !== "cancel_current_operation" && !options.fromAutoRefresh;
  if (trackOperation && options.preemptAutoRefresh !== false) {
    if (state.autoRefreshInFlight && state.busy) {
      setBusyIndeterminate("背景の差分確認を中止中");
    }
    await preemptAutoRefreshForForeground();
  }
  if (trackOperation) {
    state.activeCommand = command;
    state.cancelRequested = false;
  }
  if (trackOperation) {
    state.currentOperationCancellable = immediatelyCancellableCommands.has(command);
  }
  updateControls();
  try {
    let busyRetryIndex = 0;
    while (true) {
      try {
        const result = await tauriInvoke(command, args);
        if (!options.silentResult) setResult(result);
        return result;
      } catch (error) {
        const canRetryBusy = command !== "cancel_current_operation"
          && errorKind(error) === "operationBusy";
        if (!canRetryBusy) throw error;
        if (state.cancelRequested) {
          throw { kind: "cancelled", message: "Operation cancelled" };
        }
        if (busyRetryIndex >= OPERATION_BUSY_RETRY_DELAYS_MS.length) throw error;
        await sleep(OPERATION_BUSY_RETRY_DELAYS_MS[busyRetryIndex]);
        busyRetryIndex += 1;
      }
    }
  } finally {
    if (trackOperation && state.activeCommand === command) {
      state.activeCommand = null;
      state.currentOperationCancellable = false;
    }
    updateControls();
  }
}

function sleep(ms) {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

async function preemptAutoRefreshForForeground() {
  state.autoRefreshGeneration += 1;
  state.queuedDiffRefreshOptions = null;
  if (!state.autoRefreshInFlight) return;
  await CheckPoFrontendState.cancelAndWaitForIdle({
    isActive: () => state.autoRefreshInFlight,
    cancel: () => tauriInvoke("cancel_current_operation"),
    sleep,
    timeoutMs: AUTO_REFRESH_PREEMPT_WAIT_TIMEOUT_MS,
    intervalMs: AUTO_REFRESH_WAIT_INTERVAL_MS,
  });
}

async function run(title, task, options = {}) {
  if (state.busy) {
    setStatus(t("anotherOperationInProgress"));
    return;
  }
  state.busy = true;
  state.userOperationSerial += 1;
  clearVisibleError();
  clearVisibleStatus();
  $("busyOverlay").hidden = false;
  $("busyTitle").textContent = title;
  resetBusyProgress();
  setBusyIndeterminate(options.initialBusyLabel || t("starting"));
  updateControls();
  try {
    const result = await task();
    renderProgressImmediately({ phase: "complete", completed: 1, total: 1 }, true);
    return result;
  } catch (error) {
    await syncPendingTransactionsAfterError(error);
    const cancelled = CheckPoFrontendState.isCancellationKind(errorKind(error));
    if (cancelled) {
      const display = displayError(error);
      clearVisibleError();
      setStatus(display.message);
      setResult({ cancelled: true, message: display.message });
      options.onCancelled?.(display);
    } else if (!options.suppressError) {
      showError(error);
    }
    if (options.rethrow && !cancelled) {
      throw error;
    }
  } finally {
    state.busy = false;
    $("busyOverlay").hidden = true;
    updateControls();
  }
}

function setStatus(text) {
  appendLog(text);
  if ($("statusBannerText")) $("statusBannerText").textContent = text;
  if ($("statusBanner")) $("statusBanner").hidden = false;
  const dialogTarget = [
    ["rollbackOverlay", "rollbackStatus"],
    ["projectRegistrationOverlay", "projectRegistrationStatus"],
    ["projectSelectionOverlay", "projectSelectionStatus"],
    ["recoveryConflictOverlay", "recoveryConflictStatus"],
    ["advancedOverlay", "advancedStatus"],
    ["settingsOverlay", "settingsStatus"],
  ].find(([overlayId]) => !$(overlayId)?.hidden);
  if (dialogTarget) {
    const target = $(dialogTarget[1]);
    if (target) {
      target.textContent = text;
      target.hidden = false;
    }
  }
}

function clearVisibleStatus() {
  if ($("statusBanner")) $("statusBanner").hidden = true;
  if ($("statusBannerText")) $("statusBannerText").textContent = "";
  for (const id of [
    "settingsStatus",
    "advancedStatus",
    "projectRegistrationStatus",
    "projectSelectionStatus",
    "rollbackStatus",
    "recoveryConflictStatus",
  ]) {
    const target = $(id);
    if (!target) continue;
    target.hidden = true;
    target.textContent = "";
  }
}

function clearDialogStatus(id) {
  const target = $(id);
  if (!target) return;
  target.hidden = true;
  target.textContent = "";
}

function setResult(value) {
  let text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  if (text.length > RESULT_OUTPUT_MAX_CHARS) {
    const omitted = text.length - RESULT_OUTPUT_MAX_CHARS;
    text = `${text.slice(0, RESULT_OUTPUT_MAX_CHARS)}\n... (${omitted} 文字を省略)`;
  }
  $("resultOutput").textContent = text;
}

function errorText(error) {
  if (typeof error === "string") return error;
  if (error?.message) return error.message;
  try {
    return JSON.stringify(error);
  } catch (_) {
    return String(error);
  }
}

function errorKind(error) {
  return typeof error === "object" && error ? error.kind || "generic" : "generic";
}

function displayError(error) {
  const raw = errorText(error);
  return CheckPoFrontendState.localizedErrorDisplay(
    errorKind(error),
    raw,
    typeof error === "object" && error ? error.detail : null,
  );
}

function showError(error) {
  const display = displayError(error);
  appendLog(display.message);
  setResult(display.detail ? { error: display.message, detail: display.detail } : { error: display.message });
  showVisibleError(display.message);
  if (!$('rollbackOverlay')?.hidden) {
    $("rollbackError").textContent = display.message;
    $("rollbackError").hidden = false;
  }
  if (["workingTreeChanged", "pendingTransaction"].includes(display.kind)) {
    state.rollbackPlan = null;
    state.rollbackPlanContext = null;
    state.transactionCleanupPlan = null;
    state.gcPlan = null;
    state.tempCleanupPlan = null;
    state.rollbackRequestSerial += 1;
    if ($("rollbackConfirm")) $("rollbackConfirm").checked = false;
    if ($("cleanupSummary")) $("cleanupSummary").textContent = t("cleanupEmpty");
    if ($("gcSummary")) $("gcSummary").textContent = t("gcEmpty");
    if ($("tempCleanupSummary")) $("tempCleanupSummary").textContent = "未確認";
    if ($("gcResult")) $("gcResult").textContent = "-";
    if ($("tempCleanupResult")) $("tempCleanupResult").textContent = "-";
    updateControls();
  }
}

function showVisibleError(message) {
  $("errorBannerText").textContent = message;
  $("errorBanner").hidden = false;
  const modalNeedsForegroundError = [
    "settingsOverlay",
    "advancedOverlay",
    "projectRegistrationOverlay",
    "projectSelectionOverlay",
    "recoveryConflictOverlay",
  ].some((id) => !$(id)?.hidden);
  if (modalNeedsForegroundError) {
    $("errorDialogText").textContent = message;
    $("errorOverlay").hidden = false;
  }
}

function clearVisibleError() {
  if ($("errorBanner")) $("errorBanner").hidden = true;
  if ($("errorBannerText")) $("errorBannerText").textContent = "";
  if ($("errorOverlay")) $("errorOverlay").hidden = true;
  if ($("errorDialogText")) $("errorDialogText").textContent = "";
  if ($("rollbackError")) {
    $("rollbackError").hidden = true;
    $("rollbackError").textContent = "";
  }
}

function renderWarningBanner() {
  const currentDiffIsLatest = state.currentDiff?.checkpointId
    && state.currentDiff.checkpointId === state.latestDiffCheckpointId;
  const staleWarning = state.diffRefreshFailure ? [t("diffRefreshFailedStale")] : [];
  const latestStaleWarning = state.latestDiffRefreshFailure && !currentDiffIsLatest
    ? ["最新チェックポイントとの差分を更新できませんでした。未保存件数は未確定です。"]
    : [];
  const latestDiffWarnings = (currentDiffIsLatest ? [] : state.latestDiffWarnings || [])
    .map((warning) => `最新チェックポイントとの差分: ${warning}`);
  const diffWarnings = Array.isArray(state.currentDiff?.warnings) ? state.currentDiff.warnings : [];
  const presentation = CheckPoFrontendState.warningBannerText([
    state.snapshotWarnings,
    state.operationWarnings,
    diffWarnings,
    staleWarning,
    latestStaleWarning,
    latestDiffWarnings,
  ]);
  $("warningBanner").hidden = presentation.count === 0;
  $("warningBannerText").textContent = presentation.text;
}

function setOperationWarnings(warnings) {
  state.operationWarnings = Array.isArray(warnings) ? warnings.map(String) : [];
  renderWarningBanner();
}

function shouldStartProjectAfterLoadError(error) {
  const text = errorText(error);
  return text.includes("CheckPo marker was not found")
    || text.includes("Storage registry entry was not found");
}

function resetBusyProgress() {
  if (state.progressFrame !== null) {
    cancelAnimationFrame(state.progressFrame);
    state.progressFrame = null;
  }
  state.pendingProgress = null;
  $("busyCommand").textContent = "";
  const progress = $("busyProgress");
  progress.max = 100;
  progress.removeAttribute("value");
  $("busyProgressText").textContent = "";
  if ($("busyCloseNotice")) {
    $("busyCloseNotice").hidden = true;
    $("busyCloseNotice").textContent = "";
  }
  state.activeCommand = null;
  state.cancelRequested = false;
  state.currentOperationCancellable = false;
  $("cancelOperationButton").disabled = true;
}

function setBusyIndeterminate(label, detail = "") {
  $("busyCommand").textContent = label;
  $("busyProgress").removeAttribute("value");
  $("busyProgressText").textContent = detail;
}

function renderPendingClose(payload) {
  const presentation = CheckPoFrontendState.pendingClosePresentation(payload, state.busy);
  if (presentation.inline && $("busyCloseNotice")) {
    $("busyCloseNotice").textContent = presentation.message;
    $("busyCloseNotice").hidden = false;
    state.cancelRequested = presentation.cancellationRequested;
    updateControls();
  } else {
    showVisibleError(presentation.message);
    setStatus(presentation.message);
  }
}

function appendLog(text) {
  const item = document.createElement("li");
  item.textContent = `[${new Date().toLocaleTimeString()}] ${text}`;
  $("logList")?.prepend(item);
}

function renderUpdateBanner() {
  const update = state.availableUpdate;
  $("updateBanner").hidden = !update;
  if ($("updateSettingsStatus")) {
    $("updateSettingsStatus").textContent = update
      ? tf("updateVersionText", {
        version: update.version || "-",
        currentVersion: update.currentVersion || "-",
      })
      : t("updateStatusIdle");
  }
  if (!update) return;
  $("updateVersionText").textContent = tf("updateVersionText", {
    version: update.version || "-",
    currentVersion: update.currentVersion || "-",
  });
}

function updateCheckFailedText(error) {
  return tf("updateCheckFailed", { error: errorText(error) || "unknown" });
}

function updateAutoRefreshStatus() {
  const indicator = $("autoRefreshStatus");
  if (!indicator) return;
  indicator.hidden = !state.autoRefreshInFlight;
}

function showUpdateCheckError(error) {
  const message = updateCheckFailedText(error);
  if ($("updateSettingsStatus")) {
    $("updateSettingsStatus").textContent = message;
  }
  setStatus(message);
  setResult({ error: message });
}

async function checkForUpdate(options = {}) {
  if (!tauriInvoke) {
    const error = new Error("Tauri invoke API is not available.");
    if (!options.silent) showUpdateCheckError(error);
    if (options.rethrow) throw error;
    return null;
  }
  try {
    if ($("updateSettingsStatus") && !options.silent) {
      $("updateSettingsStatus").textContent = t("updateChecking");
    }
    const update = await tauriInvoke("check_for_update");
    state.availableUpdate = update || null;
    renderUpdateBanner();
    updateControls();
    if (!update && !options.silent) {
      $("updateSettingsStatus").textContent = t("updateNotAvailable");
      setStatus(t("updateNotAvailable"));
    }
    return update;
  } catch (error) {
    state.availableUpdate = null;
    renderUpdateBanner();
    updateControls();
    if (!options.silent) showUpdateCheckError(error);
    if (options.rethrow) throw error;
    return null;
  }
}

async function installAvailableUpdate() {
  await run("更新中", async () => {
    let update;
    try {
      update = await checkForUpdate({ rethrow: true });
    } catch (_) {
      return;
    }
    if (!update) {
      setStatus(t("updateNotAvailable"));
      return;
    }
    setBusyIndeterminate(t("updateInstalling"));
    try {
      await invokeCommand("install_update");
    } catch (error) {
      state.availableUpdate = null;
      renderUpdateBanner();
      updateControls();
      throw error;
    }
    setStatus(t("updateInstalled"));
  });
}

function getProjectPath() {
  const path = state.projectPath || $("projectPath")?.value.trim();
  if (!path) throw new Error("Unity プロジェクトを選択してください。");
  return path;
}

function selectedCheckpoint() {
  return state.checkpoints.find((item) => item.checkpointId === state.selectedCheckpointId);
}

function latestCheckpointId() {
  return sortCheckpoints(state.checkpoints)[0]?.checkpointId || null;
}

function diffBaselineCheckpointId() {
  return state.selectedCheckpointId || latestCheckpointId();
}

function getCheckpointId() {
  if (!state.selectedCheckpointId) throw new Error("チェックポイントを選択してください。");
  return state.selectedCheckpointId;
}

async function refreshProject(options = {}) {
  if (state.busy) setBusyIndeterminate(t("refreshingCheckpointList"));
  const snapshot = await invokeCommand(
    "refresh_project",
    { projectPath: getProjectPath() },
    { fromAutoRefresh: options.fromAutoRefresh },
  );
  if (options.render !== false) renderSnapshot(snapshot);
  return snapshot;
}

async function syncPendingTransactionsAfterError(error, options = {}) {
  if (errorKind(error) !== "pendingTransaction" || !state.projectPath) return;
  try {
    await refreshProject({ fromAutoRefresh: options.fromAutoRefresh });
  } catch (refreshError) {
    console.warn("中断された作業の状態を再読み込みできませんでした", refreshError);
  }
}

async function loadDiffForDisplay(projectPath, checkpointId, metadataOnly) {
  const args = { projectPath, checkpointId };
  const diff = await invokeCommand(
    CheckPoFrontendState.diffCommandName(metadataOnly),
    args,
    { fromAutoRefresh: true },
  );
  return CheckPoFrontendState.diffLoadResult(diff, metadataOnly);
}

async function refreshLatestCheckpointChangeCount({
  projectPath,
  selectedCheckpointId,
  generation,
  backgroundRefresh,
  startedUserOperationSerial,
}) {
  const latestId = latestCheckpointId();
  if (!latestId || latestId === selectedCheckpointId) return;
  let loaded;
  try {
    loaded = await loadDiffForDisplay(projectPath, latestId, true);
  } catch (error) {
    if (generation !== state.autoRefreshGeneration
      || projectPath !== state.projectPath
      || latestId !== latestCheckpointId()
      || CheckPoFrontendState.isCancellationKind(errorKind(error))) return;
    state.latestDiffCheckpointId = latestId;
    state.latestDiffChangeCount = null;
    state.latestDiffExact = false;
    state.latestDiffRefreshFailure = errorText(error);
    state.latestDiffWarnings = [];
    renderPendingFileCount();
    updateWorkingCheckpointRow();
    renderWarningBanner();
    return;
  }
  if (generation !== state.autoRefreshGeneration
    || (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial)
    || projectPath !== state.projectPath
    || latestId !== latestCheckpointId()) return;
  state.latestDiffCheckpointId = latestId;
  const latest = CheckPoFrontendState.latestDiffState(loaded.diff, loaded.exact);
  state.latestDiffWarnings = latest.warnings;
  state.latestDiffExact = latest.exact;
  state.latestDiffRefreshFailure = null;
  state.latestDiffChangeCount = latest.changeCount;
  renderPendingFileCount();
  updateWorkingCheckpointRow();
  renderWarningBanner();
}

async function refreshLatestDiff(options = {}) {
  if ((state.busy && !options.allowBusy) || !state.projectPath) return;
  if (state.autoRefreshInFlight) {
    state.queuedDiffRefreshOptions = CheckPoFrontendState.mergeDiffRefreshOptions(
      state.queuedDiffRefreshOptions,
      options,
    );
    return;
  }
  const backgroundRefresh = !options.allowBusy;
  const startedUserOperationSerial = state.userOperationSerial;
  const generation = state.autoRefreshGeneration;
  let requestedCheckpointId = null;
  state.autoRefreshInFlight = true;
  updateAutoRefreshStatus();
  try {
    if (options.refreshProject) {
      const snapshot = await refreshProject({ fromAutoRefresh: true, render: false });
      if (generation !== state.autoRefreshGeneration
        || (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial)) return;
      renderSnapshot(snapshot);
    }
    if (generation !== state.autoRefreshGeneration) return;
    const requestSerial = ++state.diffRequestSerial;
    const projectPath = getProjectPath();
    const checkpointId = diffBaselineCheckpointId();
    requestedCheckpointId = checkpointId;
    if (!checkpointId) {
      if (generation !== state.autoRefreshGeneration
        || (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial)) return;
      state.currentDiff = null;
      state.latestDiffCheckpointId = null;
      state.latestDiffChangeCount = 0;
      state.latestDiffExact = true;
      state.latestDiffRefreshFailure = null;
      state.latestDiffWarnings = [];
      $("diffSummary").textContent = t("diffEmpty");
      resetVirtualDiffTree();
      renderPendingFileCount();
      updateWorkingCheckpointRow();
      updateFilterChips(0, 0, 0);
      return;
    }
    if (!backgroundRefresh) {
      $("diffSummary").textContent = t("diffLoading");
    }
    if (options.allowBusy && state.busy) setBusyIndeterminate(t("refreshingDiffView"));
    const loaded = await loadDiffForDisplay(projectPath, checkpointId, options.metadataOnly);
    const { diff } = loaded;
    if (generation !== state.autoRefreshGeneration
      || (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial)) {
      return;
    }
    if (requestSerial !== state.diffRequestSerial
      || projectPath !== state.projectPath
      || checkpointId !== diffBaselineCheckpointId()) return;
    renderDiff(diff, checkpointId, { exact: loaded.exact });
    await refreshLatestCheckpointChangeCount({
      projectPath,
      selectedCheckpointId: checkpointId,
      generation,
      backgroundRefresh,
      startedUserOperationSerial,
    });
    if (Array.isArray(diff?.warnings) && diff.warnings.length) {
      setStatus(`高速確認で一部確認できませんでした:\n${diff.warnings.join("\n")}`);
    }
  } catch (error) {
    if (generation !== state.autoRefreshGeneration || errorKind(error) === "cancelled") return;
    await syncPendingTransactionsAfterError(error, { fromAutoRefresh: true });
    if (options.silent) {
      const failure = errorText(error);
      state.diffRefreshFailure = failure;
      const latestId = latestCheckpointId();
      if (latestId && (!requestedCheckpointId || requestedCheckpointId === latestId)) {
        state.latestDiffCheckpointId = latestId;
        state.latestDiffChangeCount = null;
        state.latestDiffExact = false;
        state.latestDiffRefreshFailure = failure;
        state.latestDiffWarnings = [];
        renderPendingFileCount();
        updateWorkingCheckpointRow();
      }
      renderWarningBanner();
      updateControls();
    } else {
      clearCurrentDiff();
      showError(error);
    }
  } finally {
    state.autoRefreshInFlight = false;
    updateAutoRefreshStatus();
    const queued = state.queuedDiffRefreshOptions;
    state.queuedDiffRefreshOptions = null;
    if (queued && (!state.busy || queued.allowBusy) && state.projectPath) {
      window.setTimeout(() => refreshLatestDiff(queued), 0);
    }
  }
}

function clearCurrentDiff() {
  state.diffRequestSerial += 1;
  state.currentDiff = null;
  state.diffRefreshFailure = null;
  state.rollbackPlan = null;
  state.currentDiffSelectedPaths.clear();
  state.lastSelectedChangePath = null;
  $("diffSummary").textContent = t("diffEmpty");
  resetVirtualDiffTree();
  renderPendingFileCount();
  updateFilterChips(0, 0, 0);
  updateSelectedDiffButton();
  renderWarningBanner();
  updateControls();
}

function scheduleFocusRefresh() {
  if (document.hidden || !state.projectPath || state.busy || state.confirming) return;
  const now = Date.now();
  if (now - state.lastAutoRefreshAt < 750) return;
  state.lastAutoRefreshAt = now;
  refreshLatestDiff({ silent: true, metadataOnly: true });
}

function queueFocusRefresh() {
  window.setTimeout(scheduleFocusRefresh, 100);
}

function renderSnapshot(snapshot) {
  const nextProjectPath = snapshot.projectPath || snapshot.project?.projectRootPath || state.projectPath;
  const nextProject = snapshot.project
    || (nextProjectPath === state.projectPath ? state.project : null);
  const changedProject = CheckPoFrontendState.projectChanged(
    state.projectPath,
    state.project,
    nextProjectPath,
    nextProject,
  );
  const previousStoredSize = changedProject ? null : state.storage?.storedSizeBytes;
  if (changedProject) {
    Object.assign(state, CheckPoFrontendState.projectScopedStateReset());
    state.storageSizeLoadedProjectPath = null;
    state.storageSizeLoadingProjectPath = null;
    clearCurrentDiff();
    resetProjectScopedDom();
  }
  state.projectPath = nextProjectPath;
  state.project = nextProject;
  state.projectLocationStatus = state.project?.locationStatus || "current";
  state.checkpointIndex = snapshot.checkpointIndex
    || { state: "current", rebuildable: false, detail: null };
  state.checkpoints = sortCheckpoints(snapshot.checkpoints || []);
  const nextLatestCheckpointId = latestCheckpointId();
  if (state.latestDiffCheckpointId !== nextLatestCheckpointId) {
    state.latestDiffCheckpointId = null;
    state.latestDiffChangeCount = nextLatestCheckpointId ? null : 0;
    state.latestDiffExact = !nextLatestCheckpointId;
    state.latestDiffRefreshFailure = null;
    state.latestDiffWarnings = [];
  }
  state.storage = CheckPoFrontendState.storageSummaryWithRetainedSize(
    snapshot.storage,
    previousStoredSize,
  );
  state.gcPlan = null;
  state.tempCleanupPlan = null;
  state.transactionCleanupPlan = null;
  state.snapshotWarnings = Array.isArray(snapshot.warnings) ? snapshot.warnings.map(String) : [];
  if (!state.checkpoints.some((checkpoint) => checkpoint.checkpointId === state.selectedCheckpointId)) {
    state.selectedCheckpointId = state.checkpoints[0]?.checkpointId || null;
  }

  rememberProject(snapshot);
  renderProjectLabels();
  renderProjectHistory();
  renderCheckpointIndexNotice();
  renderCheckpoints();
  renderStorage();
  renderPending(snapshot.pendingTransactions || []);
  renderUnresolvedQuarantines(snapshot.unresolvedQuarantines || []);
  renderProjectWarnings(snapshot.project?.warnings || []);
  if (snapshot.warnings?.length) {
    setStatus(snapshot.warnings.join("\n"));
  }
  renderWarningBanner();
  updateControls();
  scheduleStorageSizeRefresh();
}

function sortCheckpoints(checkpoints) {
  return [...checkpoints].sort((a, b) => {
    const byTime = String(b.createdAtUtc || "").localeCompare(String(a.createdAtUtc || ""));
    if (byTime !== 0) return byTime;
    return String(b.checkpointId || "").localeCompare(String(a.checkpointId || ""));
  });
}

function rememberProject(snapshot) {
  if (!state.projectPath) return;
  if (state.hiddenProjectPaths.has(state.projectPath)) return;
  const entry = {
    path: state.projectPath,
    name: snapshot.project?.projectName || basename(state.projectPath),
    lastOpenedAt: new Date().toISOString(),
  };
  state.projectHistory = [entry, ...state.projectHistory.filter((item) => item.path !== entry.path)].slice(0, 12);
  writeProjectHistory();
  writeLocalSetting("lastProjectPath", state.projectPath);
}

function renderCheckpointIndexNotice() {
  const presentation = CheckPoFrontendState.checkpointIndexPresentation(state.checkpointIndex);
  const notice = $("checkpointIndexNotice");
  if (!notice) return;
  notice.hidden = presentation.available;
  $("checkpointIndexMessage").textContent = presentation.message;
  $("checkpointIndexMessage").title = presentation.detail || "";
  $("rebuildIndexButton").hidden = !presentation.rebuildable;
}

async function restoreLastProject() {
  const projectPath = CheckPoFrontendState.restorableLastProjectPath(
    state.projectHistory,
    readLocalSetting("lastProjectPath"),
  );
  if (!tauriInvoke || !projectPath) return;
  const restored = await run("前回のプロジェクトを読み込み中", async () => {
    const snapshot = await invokeCommand("load_project", { projectPath });
    renderSnapshot(snapshot);
    if (!(snapshot.pendingTransactions?.length || snapshot.unresolvedQuarantines?.length)) {
      await refreshLatestDiff({ allowBusy: true, metadataOnly: true });
    }
    return snapshot;
  });
  if (!restored) removeLocalSetting("lastProjectPath");
}

function renderProjectLabels() {
  const label = state.project?.projectName || basename(state.projectPath) || t("projectSelectPlaceholder");
  const projectPath = CheckPoFrontendState.userFacingPath(state.projectPath);
  const storageRootPath = CheckPoFrontendState.userFacingPath(state.project?.storageRootPath);
  $("projectMenuLabel").textContent = label;
  $("projectMenuButton").classList.toggle("has-selection", Boolean(state.projectPath));
  if ($("projectRegistrationOverlay").hidden) {
    $("projectPath").value = projectPath;
  }
  $("settingsStorageRootPath").value = storageRootPath || "-";
  $("settingsNewStorageRootPath").value = "";
  renderDefaultStorageRootPath();
  const active = selectedCheckpoint();
  if ($("selectedCheckpointLabel")) $("selectedCheckpointLabel").textContent = active ? active.name : t("noSelection");
  $("activeCheckpointTitle").textContent = active
    ? `${active.name || active.checkpointId} → ${t("workingFolder")}`
    : t("noSelection");
  $("projectStatusPath").textContent = projectPath || "-";
  $("projectStatusPath").title = projectPath;
  updateProjectEmptyState();
}

function updateProjectEmptyState() {
  const hasProject = Boolean(state.projectPath);
  $("projectEmptyState").hidden = hasProject;
  $("diffGroups").hidden = !hasProject;
}

function renderStorage() {
  $("checkpointCount").textContent = String(state.storage?.checkpointCount ?? state.checkpoints.length);
  $("logicalSize").textContent = formatBytes(state.storage?.logicalSizeBytes ?? 0);
  $("storedSize").textContent = state.storage?.storedSizeBytes == null
    ? "-"
    : formatBytes(state.storage.storedSizeBytes);
  $("uniqueBlobs").textContent = String(state.storage?.uniqueBlobCount ?? "-");
  $("packCount").textContent = "-";
  if ($("recoveryRescueSize")) {
    const rescueBytes = state.storage?.recoveryRescueBytes;
    const rescueFiles = state.storage?.recoveryRescueFileCount;
    $("recoveryRescueSize").textContent = rescueBytes == null
      ? "-"
      : `${formatBytes(rescueBytes)} / ${rescueFiles ?? 0} files`;
  }
}

function invalidateStoredSize() {
  state.storageSizeLoadedProjectPath = null;
  if (state.storage) {
    state.storage.storedSizeBytes = null;
    state.storage.recoveryRescueBytes = null;
    state.storage.recoveryRescueFileCount = null;
  }
  renderStorage();
}

function scheduleStorageSizeRefresh(options = {}) {
  const projectPath = state.projectPath;
  if (!tauriInvoke || !projectPath) return;
  if (!options.force && state.storageSizeLoadedProjectPath === projectPath) return;
  if (state.storageSizeLoadingProjectPath === projectPath) return;
  state.storageSizeLoadingProjectPath = projectPath;
  window.setTimeout(async () => {
    let retryWhenIdle = false;
    try {
      const summary = await invokeCommand(
        "calculate_storage_summary",
        { projectPath },
        { fromAutoRefresh: true, silentResult: true },
      );
      if (state.projectPath !== projectPath) return;
      state.storage = { ...(state.storage || {}), ...summary };
      state.storageSizeLoadedProjectPath = projectPath;
      renderStorage();
    } catch (error) {
      retryWhenIdle = errorKind(error) === "operationBusy"
        && state.projectPath === projectPath;
      if (!retryWhenIdle) {
        console.warn("保存容量のバックグラウンド集計に失敗しました", error);
      }
    } finally {
      if (state.storageSizeLoadingProjectPath === projectPath) {
        state.storageSizeLoadingProjectPath = null;
      }
    }
    if (retryWhenIdle) {
      window.setTimeout(() => scheduleStorageSizeRefresh(options), 500);
    }
  }, 0);
}

function renderPending(items) {
  state.pendingTransactions = Array.isArray(items) ? items : [];
  state.failedTransactions = CheckPoFrontendState.retainPendingTransactionFailures(
    state.pendingTransactions,
    state.failedTransactions,
  );
  $("pendingTransactionBanner").hidden = items.length === 0;
  if (items.length === 0) {
    $("pendingTransactionText").textContent = "";
    renderTransactionQuarantineAction();
    updateControls();
    return;
  }
  const states = items
    .slice(0, 3)
    .map((item) => `${shortId(item.transactionId)}:${item.state || t("unknownState")}`)
    .join(" / ");
  const omitted = items.length > 3 ? tf("otherItems", { count: items.length - 3 }) : "";
  const details = states ? ` (${states}${omitted})` : "";
  $("pendingTransactionText").textContent = state.failedTransactions.length > 0
    ? `${items.length} 件の中断された書き戻しで、自動復旧を安全に続けられませんでした。${details}`
    : tf("pendingTransactionsMessage", { count: items.length, details });
  renderTransactionQuarantineAction();
  updateControls();
}

function resetProjectScopedSettingsResults() {
  $("gcSummary").textContent = t("gcEmpty");
  $("gcResult").textContent = "-";
  $("cleanupSummary").textContent = t("cleanupEmpty");
  $("tempCleanupSummary").textContent = "未確認";
  $("tempCleanupResult").textContent = "-";
  $("rollbackOverlay").hidden = true;
  $("recoveryConflictOverlay").hidden = true;
  $("recoveryConflictList").replaceChildren();
  $("recoveryExportRoot").value = "";
}

function resetProjectScopedDom() {
  resetProjectScopedSettingsResults();
  $("checkpointSearch").value = "";
  $("checkpointName").value = "";
  $("verificationSummary").textContent = t("notRun");
  $("cleanupResult").textContent = "-";
  $("logList").replaceChildren();
  setResult({});
}

function renderProjectWarnings(warnings) {
  const items = Array.isArray(warnings) ? warnings : [];
  state.projectWarnings = items;
  const locationWarnings = items.filter((warning) =>
    ["projectMoved", "copiedProjectSuspected"].includes(warning?.kind)
  );
  const copiedWarnings = items.filter((warning) =>
    warning?.kind === "copiedProjectSuspected" || warning?.locationStatus === "copiedSuspected"
  );
  $("projectWarningBanner").hidden = items.length === 0;
  $("projectWarningText").textContent = items.map(projectWarningText).join(" / ");
  $("confirmProjectLocationButton").hidden = locationWarnings.length === 0;
  $("startSeparateProjectButton").hidden = copiedWarnings.length === 0;
}

function projectWarningText(warning) {
  if (["projectMoved", "copiedProjectSuspected"].includes(warning?.kind)) {
    const previous = CheckPoFrontendState.userFacingPath(warning.previousProjectRootPath) || "-";
    const current = CheckPoFrontendState.userFacingPath(
      warning.currentProjectRootPath || state.projectPath,
    ) || "-";
    if (warning.previousMarkerHasSameProjectId || warning.locationStatus === "copiedSuspected") {
      return `同じ project_id が別の場所にも登録されています。以前の場所: ${previous} / 現在の場所: ${current}。コピーした Unity プロジェクトの可能性があります。`;
    }
    return `以前の登録場所から移動されています。以前の場所: ${previous} / 現在の場所: ${current}。`;
  }
  return warning?.message || "プロジェクトの状態に警告があります。";
}

let checkpointSearchTimer = null;
let checkpointScrollFrame = null;
const checkpointRowCache = new Map();
const CHECKPOINT_SEARCH_DEBOUNCE_MS = 80;
const CHECKPOINT_ROW_HEIGHT_PX = 32;
const CHECKPOINT_VIRTUAL_OVERSCAN = 8;
let filteredCheckpoints = [];

async function runExactDiff() {
  await run(t("runDiff"), async () => {
    clearCurrentDiff();
    $("diffSummary").textContent = t("diffLoading");
    const requestSerial = ++state.diffRequestSerial;
    const projectPath = getProjectPath();
    const checkpointId = getCheckpointId();
    const diff = await invokeCommand("diff_checkpoint_full", {
      projectPath,
      checkpointId,
    });
    if (requestSerial !== state.diffRequestSerial
      || projectPath !== state.projectPath
      || checkpointId !== diffBaselineCheckpointId()) return;
    renderDiff(diff, checkpointId, { exact: true });
    setStatus(t("diffUpdated"));
  });
}

function renderRollbackPlan(plan, context) {
  if (!CheckPoFrontendState.restorePlanCanApply(
    plan,
    state.unresolvedQuarantines.length,
  ) && !(plan?.warnings?.length)) {
    state.rollbackPlan = null;
    state.rollbackPlanContext = null;
    $("rollbackOverlay").hidden = true;
    const checkpoint = state.checkpoints.find((item) => item.checkpointId === context?.checkpointId);
    if (checkpoint) {
      renderDiff({
        added: [],
        modified: [],
        deleted: [],
        unchangedCount: checkpoint.fileCount ?? 0,
        warnings: [],
      }, context.checkpointId, { exact: true });
    }
    setStatus("戻す変更はありません。");
    updateControls();
    return;
  }
  state.rollbackPlan = plan;
  state.rollbackPlanContext = context;
  clearVisibleError();
  const operations = plan.operations || [];
  $("rollbackSummary").textContent =
    `復元 ${plan.restoreCount ?? 0} / 置換 ${plan.replaceCount ?? 0} / 日時 ${plan.metadataCount ?? 0} / 削除 ${plan.deleteCount ?? 0} / 一時容量 約${formatBytes(plan.estimatedTemporaryBytes ?? 0)} / 対象 ${operations.length} 件`;
  $("rollbackWarnings").replaceChildren();
  for (const warning of plan.warnings || []) {
    const item = document.createElement("p");
    item.className = "empty-list";
    item.textContent = `警告: ${warning}`;
    $("rollbackWarnings").append(item);
  }
  const list = $("rollbackOperations");
  list.replaceChildren();
  if (!operations.length) {
    const empty = document.createElement("p");
    empty.className = "empty-list";
    empty.textContent = "変更はありません。";
    list.append(empty);
  }
  for (const operation of operations.slice(0, ROLLBACK_OPERATION_RENDER_LIMIT)) {
    const row = document.createElement("div");
    row.className = "operation-row";
    row.innerHTML = `
      <span class="operation-type"></span>
      <span class="operation-path"></span>
    `;
    row.querySelector(".operation-type").textContent = ({
      restore: "復元",
      replace: "置換",
      delete: "削除",
      setMetadata: "日時",
    })[operation.operationType] || operation.operationType;
    row.querySelector(".operation-path").textContent = operation.path;
    list.append(row);
  }
  if (operations.length > ROLLBACK_OPERATION_RENDER_LIMIT) {
    const omitted = document.createElement("p");
    omitted.className = "empty-list";
    omitted.textContent = `他 ${operations.length - ROLLBACK_OPERATION_RENDER_LIMIT} 件は省略しています。`;
    list.append(omitted);
  }
  $("rollbackConfirm").checked = false;
  clearDialogStatus("rollbackStatus");
  $("rollbackOverlay").hidden = false;
  updateControls();
}

function updateControls() {
  const hasProject = Boolean(state.projectPath);
  const hasCheckpoint = Boolean(state.selectedCheckpointId);
  const hasCheckpointName = Boolean($("checkpointName").value.trim());
  const hasRegistrationProjectPath = Boolean($("projectPath").value.trim());
  const newStorageRootPath = $("settingsNewStorageRootPath").value.trim();
  const storageRootUnchanged = CheckPoFrontendState.samePathInput(
    newStorageRootPath,
    $("settingsStorageRootPath").value,
  );
  const locationMutationBlocked = state.projectLocationStatus === "copiedSuspected";
  const pendingMutationBlocked = state.pendingTransactions.length > 0;
  const quarantineMutationBlocked = state.unresolvedQuarantines.length > 0;
  const checkpointIndex = CheckPoFrontendState.checkpointIndexPresentation(state.checkpointIndex);
  const indexUnavailable = !checkpointIndex.available;
  const destructiveBlocked = locationMutationBlocked || pendingMutationBlocked || quarantineMutationBlocked;
  const controlsBlocked = state.busy || state.confirming;
  document.querySelectorAll("button").forEach((button) => {
    if (["confirmOkButton", "confirmCancelButton"].includes(button.id)) {
      button.disabled = state.busy && $("confirmOverlay").hidden;
      return;
    }
    if (button.id === "cancelOperationButton") {
      button.disabled = !state.busy || !state.currentOperationCancellable || state.cancelRequested;
      return;
    }
    if (button.id === "installUpdateButton") {
      button.disabled = controlsBlocked || !state.availableUpdate;
      return;
    }
    if (button.id === "checkUpdateButton") {
      button.disabled = controlsBlocked;
      return;
    }
    if ([
      "projectMenuButton",
      "addProjectFromSelectionButton",
      "closeProjectSelectionButton",
      "closeProjectRegistrationButton",
      "pickProjectButton",
      "pickRegistrationStorageRootButton",
      "settingsButton",
      "closeSettingsButton",
      "pickDefaultStorageRootButton",
    ].includes(button.id)) {
      button.disabled = controlsBlocked;
      return;
    }
    if (button.id === "openProjectButton") {
      button.disabled = controlsBlocked || !hasRegistrationProjectPath;
      return;
    }
    if (button.id === "applyRollbackButton") {
      button.disabled = controlsBlocked
        || locationMutationBlocked
        || pendingMutationBlocked
        || !state.rollbackPlan
        || !state.rollbackPlanContext
        || Boolean(state.rollbackPlan.warnings?.length)
        || !$("rollbackConfirm").checked
        || !CheckPoFrontendState.restorePlanCanApply(
          state.rollbackPlan,
          state.unresolvedQuarantines.length,
        );
      return;
    }
    if (button.id === "rebuildIndexButton") {
      button.disabled = controlsBlocked
        || locationMutationBlocked
        || pendingMutationBlocked
        || !hasProject
        || !checkpointIndex.rebuildable;
      return;
    }
    if (button.id === "applyGcButton") {
      const gc = CheckPoFrontendState.gcPlanPresentation(state.gcPlan);
      button.disabled = controlsBlocked
        || destructiveBlocked
        || !hasProject
        || !state.gcPlan
        || state.gcPlan.hasIntegrityProblems
        || gc.totalCount === 0;
      return;
    }
    if (["previewRollbackButton", "diffButton", "verifyButton"].includes(button.id)) {
      const pendingBlocked = button.id === "previewRollbackButton" && pendingMutationBlocked;
      const exactNoChanges = button.id === "previewRollbackButton"
        && CheckPoFrontendState.restorePreviewIsRedundant(
          checkpointHasKnownExactNoChanges(state.selectedCheckpointId),
          state.unresolvedQuarantines.length,
        );
      button.disabled = controlsBlocked
        || indexUnavailable
        || pendingBlocked
        || exactNoChanges
        || !hasProject
        || !hasCheckpoint;
      return;
    }
    if (button.id === "deleteCheckpointButton") {
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject || !hasCheckpoint;
      return;
    }
    if (button.id === "verifyProjectButton") {
      button.disabled = controlsBlocked || !hasProject;
      return;
    }
    if (button.id === "createCheckpointButton") {
      button.disabled = controlsBlocked || indexUnavailable || destructiveBlocked || !hasProject || !hasCheckpointName;
      return;
    }
    if (button.id === "setStorageRootButton") {
      button.disabled = controlsBlocked
        || destructiveBlocked
        || !hasProject
        || !newStorageRootPath
        || storageRootUnchanged;
      return;
    }
    if (button.id === "applyCleanupButton") {
      button.disabled = controlsBlocked
        || locationMutationBlocked
        || !hasProject
        || !CheckPoFrontendState.transactionCleanupPlanHasCandidates(
          state.transactionCleanupPlan,
        );
      return;
    }
    if (button.id === "applyRecoveryConflictButton") {
      button.disabled = controlsBlocked
        || locationMutationBlocked
        || !state.recoveryConflictPlan
        || selectedRecoveryConflictPaths().length === 0
        || !$("recoveryExportRoot").value.trim();
      return;
    }
    if (button.id === "applyRecoveryConflictWithoutExportButton") {
      button.disabled = controlsBlocked
        || locationMutationBlocked
        || !state.recoveryConflictPlan;
      return;
    }
    if ([
      "recoverTransactionsButton",
      "quarantineTransactionButton",
      "selectRecoveryFilesButton",
    ].includes(button.id)) {
      button.disabled = controlsBlocked || locationMutationBlocked || !hasProject;
      return;
    }
    if (button.id === "applyTempCleanupButton") {
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject || !state.tempCleanupPlan || !state.tempCleanupPlan.fileCount;
      return;
    }
    if (button.dataset.destructive === "true") {
      const diffBlocked = (button.id === "discardSelectedDiffButton" || button.classList.contains("tree-op"))
        && !CheckPoFrontendState.diffResultIsComplete(state.currentDiff, state.diffRefreshFailure);
      button.disabled = controlsBlocked || destructiveBlocked || diffBlocked || !hasProject;
      return;
    }
    button.disabled = controlsBlocked || (!hasProject && button.dataset.command);
  });
  $("activeCheckpointCard")?.classList.toggle("is-disabled", !hasCheckpoint);
  updateSelectedDiffButton();
}

async function pickFolder(title) {
  const result = await invokeCommand("pick_folder", { title });
  return result?.path || "";
}

function isCopiedProjectError(error) {
  return errorKind(error) === "copiedProjectSuspected";
}

async function isCopiedProjectAtPath(projectPath) {
  if (!tauriInvoke || !projectPath) return false;
  try {
    const snapshot = await tauriInvoke("load_project", { projectPath });
    return snapshot?.project?.locationStatus === "copiedSuspected";
  } catch {
    return false;
  }
}

function copiedProjectRegistrationChoiceMessage(projectPath) {
  const displayedProjectPath = CheckPoFrontendState.userFacingPath(projectPath);
  return [
    "この Unity プロジェクトは、すでに CheckPo に登録済みのプロジェクトをコピーした可能性があります。",
    "",
    `現在の場所: ${displayedProjectPath || "現在の場所"}`,
    "",
    "既存のチェックポイント履歴をこの場所に紐づけ直す場合は「この場所を使う」を選んでください。",
    "コピー元とは別の履歴として始める場合は「別プロジェクトとして開始」を選んでください。",
  ].join("\n");
}

async function handleCopiedProjectRegistrationChoice(projectPath, storageRootPath = "") {
  state.confirming = true;
  updateControls();
  let decision = null;
  try {
    decision = await chooseCopiedProjectAction(copiedProjectRegistrationChoiceMessage(projectPath));
  } finally {
    state.confirming = false;
    updateControls();
  }
  if (!decision) return;

  if (decision.action === "useLocation") {
    await run("場所を記録中", async () => {
      renderSnapshot(await invokeCommand("confirm_project_location", { projectPath }));
      await refreshLatestDiff({ allowBusy: true });
      $("projectRegistrationOverlay").hidden = true;
      setStatus("この場所をプロジェクトの現在の場所として記録しました。");
    });
    return;
  }

  if (decision.action === "startSeparate") {
    await run(
      decision.createInitialCheckpoint ? "初回チェックポイントを作成中" : "別プロジェクトとして開始中",
      async () => {
        const snapshot = await invokeCommand("start_as_separate_project", {
          projectPath,
          storageRootPath,
          confirmed: true,
          createInitialCheckpoint: decision.createInitialCheckpoint,
        });
        renderStartedProject(snapshot, "この場所を別プロジェクトとして開始しました。");
        await refreshLatestDiff({ allowBusy: true });
        $("projectRegistrationOverlay").hidden = true;
      },
    );
  }
}

async function reconnectProjectStorageAfterLoadFailure(projectPath, storageRootPath) {
  state.confirming = true;
  updateControls();
  let reconnect = false;
  try {
    reconnect = await confirmAction(
      `登録済みの保存先を読み込めません。${storageRootPath} に手動移動済みの保存データがある場合、このプロジェクトを再接続します。`,
      "保存データへ再接続",
    );
  } finally {
    state.confirming = false;
    updateControls();
  }
  if (!reconnect) {
    setStatus("保存データへの再接続を中止しました。");
    return;
  }
  await run("保存データへ再接続中", async () => {
    const snapshot = await invokeCommand("set_storage_root", {
      projectPath,
      storageRootPath,
      confirmed: true,
    });
    invalidateStoredSize();
    renderSnapshot(snapshot);
    await refreshLatestDiff({ allowBusy: true, metadataOnly: true });
    $("projectRegistrationOverlay").hidden = true;
    setStatus("手動移動済みの保存データへ再接続しました。");
  });
}

function copiedLocationConfirmMessage() {
  const warning = (state.projectWarnings || []).find((item) =>
    item?.kind === "copiedProjectSuspected" || item?.locationStatus === "copiedSuspected"
  );
  const previous = CheckPoFrontendState.userFacingPath(warning?.previousProjectRootPath)
    || "以前の場所";
  const current = CheckPoFrontendState.userFacingPath(
    warning?.currentProjectRootPath || state.projectPath,
  ) || "現在の場所";
  return [
    "この場所を同じプロジェクトとして使います。",
    "",
    `現在の場所: ${current}`,
    `以前の場所: ${previous}`,
    "",
    "続行すると、既存のチェックポイント履歴は現在の場所に紐づきます。以前の場所を開いた場合はコピー疑いとして止まり、ファイルのディスカードや復元などの変更操作はできなくなります。",
    "",
    "逆に、以前の場所を使いたい場合はここでは続行せず、以前の場所を開いて「この場所を使う」を選んでください。",
  ].join("\n");
}

function startSeparateProjectConfirmMessage() {
  const warning = (state.projectWarnings || []).find((item) =>
    item?.kind === "copiedProjectSuspected" || item?.locationStatus === "copiedSuspected"
  );
  const previous = CheckPoFrontendState.userFacingPath(warning?.previousProjectRootPath)
    || "コピー元の場所";
  const current = CheckPoFrontendState.userFacingPath(
    warning?.currentProjectRootPath || state.projectPath,
  ) || "現在の場所";
  return [
    "この場所を別プロジェクトとして開始します。",
    "",
    `現在の場所: ${current}`,
    `コピー元と思われる場所: ${previous}`,
    "",
    "コピー元のチェックポイント履歴は変更せず、この場所では空の履歴から開始します。",
    "",
    "既存のチェックポイント履歴をこの場所で使いたい場合は、ここでは続行せず「この場所を使う」を選んでください。",
  ].join("\n");
}

async function discardPaths(paths) {
  if (state.pendingTransactions.length > 0) {
    showError({ kind: "pendingTransaction", message: "A transaction must be recovered first" });
    return;
  }
  if (!CheckPoFrontendState.diffResultIsComplete(state.currentDiff, state.diffRefreshFailure)) {
    showError({
      kind: "workingTreeChanged",
      message: "差分を完全に確認できていません。警告を解消して再読み込みしてください。",
    });
    return;
  }
  const projectPath = getProjectPath();
  const checkpointId = state.currentDiff?.checkpointId;
  const diffRequestSerial = state.diffRequestSerial;
  if (!checkpointId || checkpointId !== diffBaselineCheckpointId()) {
    showError({ kind: "workingTreeChanged", message: "差分の基準が変わりました。再読み込みして選び直してください。" });
    return;
  }
  const plan = await run("取り消し確認", () => invokeCommand("preview_discard_files", {
    projectPath,
    paths,
    checkpointId,
  }));
  if (!plan) return;
  const contextIsCurrent = () => projectPath === state.projectPath
    && checkpointId === diffBaselineCheckpointId()
    && diffRequestSerial === state.diffRequestSerial;
  if (!contextIsCurrent()) {
    showError({ kind: "workingTreeChanged", message: "確認中に差分の基準が変わりました。再読み込みしてください。" });
    return;
  }
  if (!plan.hasChanges) {
    setStatus("取り消す変更はありません。");
    return;
  }
  state.confirming = true;
  updateControls();
  let confirmed = false;
  try {
    const effectivePaths = plan.selectedPaths || paths;
    const preview = CheckPoFrontendState.pathConfirmationPreview(
      effectivePaths,
      CONFIRM_PATH_RENDER_LIMIT,
    );
    confirmed = await confirmAction(
      `${preview.total} 件の変更を戻します。\n\n${preview.text}\n\nUnityを起動したまま実行できます。CheckPoは確認した対象だけを書き戻します。書き戻し完了後にUnityが対象パスへ保存した内容は、新しい変更として扱われます。\n\n続行しますか？`,
      "戻す",
    );
  } finally {
    state.confirming = false;
    updateControls();
  }
  if (!confirmed) return;
  if (!contextIsCurrent()) {
    showError({ kind: "workingTreeChanged", message: "確認中に差分の基準が変わりました。もう一度確認してください。" });
    return;
  }

  await run("戻し中", async () => {
    const result = await invokeCommand("apply_discard_files", {
      projectPath,
      paths,
      checkpointId,
      expectedPlan: plan,
      confirmed: true,
    });
    setBusyIndeterminate("再読み込み中");
    setStatus("変更を取り消しました。");
    invalidateStoredSize();
    await refreshProject();
    state.currentDiffSelectedPaths.clear();
    state.lastSelectedChangePath = null;
    await refreshLatestDiff({ allowBusy: true });
    if (result.warnings?.length) {
      setOperationWarnings(result.warnings);
      setStatus("変更は取り消しましたが、後処理に警告があります。");
    }
  });
}

function bindEvents() {
  $("dismissErrorButton").addEventListener("click", clearVisibleError);
  $("dismissErrorDialogButton").addEventListener("click", clearVisibleError);
  $("projectMenuButton").addEventListener("click", () => {
    renderProjectHistory();
    clearDialogStatus("projectSelectionStatus");
    $("projectSelectionOverlay").hidden = false;
  });
  $("projectMenuButton").addEventListener("contextmenu", (event) => {
    event.preventDefault();
    showProjectContextMenu(event.clientX, event.clientY);
  });
  document.addEventListener("contextmenu", (event) => {
    if (event.defaultPrevented) return;
    event.preventDefault();
    hideContextMenu();
  });
  document.addEventListener("click", (event) => {
    if (!$("contextMenu")?.contains(event.target)) hideContextMenu();
  });
  document.addEventListener("keydown", handleContextMenuKeyDown);
  window.addEventListener("resize", hideContextMenu);
  window.addEventListener("scroll", hideContextMenu, true);
  $("addProjectFromEmptyButton").addEventListener("click", () => {
    openProjectRegistration();
  });
  $("addProjectFromSelectionButton").addEventListener("click", () => {
    $("projectSelectionOverlay").hidden = true;
    openProjectRegistration();
  });
  $("closeProjectSelectionButton").addEventListener("click", () => $("projectSelectionOverlay").hidden = true);
  $("closeProjectRegistrationButton").addEventListener("click", () => $("projectRegistrationOverlay").hidden = true);
  $("settingsButton").addEventListener("click", () => {
    clearDialogStatus("settingsStatus");
    $("settingsOverlay").hidden = false;
    updateControls();
  });
  $("closeSettingsButton").addEventListener("click", () => $("settingsOverlay").hidden = true);
  $("advancedButton").addEventListener("click", () => {
    clearDialogStatus("advancedStatus");
    $("advancedOverlay").hidden = false;
  });
  $("closeAdvancedButton").addEventListener("click", () => $("advancedOverlay").hidden = true);
  $("closeRollbackDialogButton").addEventListener("click", () => $("rollbackOverlay").hidden = true);
  $("closeRecoveryConflictButton").addEventListener("click", closeRecoveryConflictDialog);
  $("cancelRecoveryConflictButton").addEventListener("click", closeRecoveryConflictDialog);
  $("recoverySelectAll").addEventListener("change", (event) => {
    $("recoveryConflictList").querySelectorAll("input[type='checkbox']").forEach((input) => {
      input.checked = event.currentTarget.checked;
    });
    updateRecoveryConflictControls();
  });
  $("pickRecoveryExportRootButton").addEventListener("click", async () => {
    const path = await pickFolder(t("recoveryConflictPickExportRoot"));
    if (!path) return;
    $("recoveryExportRoot").value = path;
    updateRecoveryConflictControls();
  });
  $("applyRecoveryConflictButton").addEventListener("click", applyRecoveryConflictSelection);
  $("applyRecoveryConflictWithoutExportButton").addEventListener("click", () => {
    applyRecoveryConflictSelection({ withoutExport: true });
  });
  $("dismissStatusButton").addEventListener("click", () => {
    $("statusBanner").hidden = true;
    $("statusBannerText").textContent = "";
  });
  $("clearLogButton").addEventListener("click", () => $("logList").replaceChildren());
  $("openDiagnosticLogsButton").addEventListener("click", async () => {
    await run("診断ログを開いています", async () => {
      await invokeCommand("open_diagnostic_logs");
      setStatus("診断ログフォルダを開きました。");
    });
  });
  $("clearResultButton").addEventListener("click", () => setResult({}));
  $("checkpointSearch").addEventListener("input", scheduleCheckpointSearchFilter);
  for (const [eventName, handler] of CheckPoFrontendState.checkpointListEventBindings({
    click: handleCheckpointListClick,
    contextmenu: handleCheckpointListContextMenu,
    keydown: handleCheckpointListKeyDown,
  })) {
    $("checkpointList").addEventListener(eventName, handler);
  }
  $("checkpointList").addEventListener("scroll", scheduleCheckpointVirtualWindowRender, { passive: true });
  $("checkpointName").addEventListener("input", updateControls);
  $("projectPath").addEventListener("input", updateControls);
  $("settingsNewStorageRootPath").addEventListener("input", updateControls);
  document.querySelectorAll("[data-diff-filter]").forEach((button) => {
    button.addEventListener("click", () => {
      state.currentDiffFilter = button.dataset.diffFilter || "all";
      if (state.currentDiff) renderCurrentDiff(state.currentDiff, { resetScroll: true });
      else updateFilterChips(0, 0, 0);
    });
  });
  $("expandDiffTreeButton").addEventListener("click", () => {
    state.diffTreeTouched = true;
    if (state.currentDiff) {
      state.diffTreeOpenPaths = new Set(collectFolderPaths(buildChangeTree(currentFilteredChanges())));
      renderCurrentDiff(state.currentDiff);
    }
  });
  $("collapseDiffTreeButton").addEventListener("click", () => {
    state.diffTreeTouched = true;
    state.diffTreeOpenPaths.clear();
    if (state.currentDiff) renderCurrentDiff(state.currentDiff);
  });
  $("discardSelectedDiffButton").addEventListener("click", () => {
    const paths = [...state.currentDiffSelectedPaths];
    if (paths.length > 0) discardPaths(paths);
  });
  $("rollbackConfirm").addEventListener("change", updateControls);
  document.querySelectorAll("[data-theme-option]").forEach((button) => {
    button.addEventListener("click", () => setTheme(button.dataset.themeOption || "system"));
  });

  $("pickProjectButton").addEventListener("click", async () => {
    const path = await pickFolder("Unity project");
    if (path) $("projectPath").value = path;
    updateControls();
  });
  $("pickRegistrationStorageRootButton").addEventListener("click", async () => {
    const path = await pickFolder("Checkpoint storage");
    if (path) $("registrationStorageRootPath").value = path;
  });
  $("settingsDefaultStorageRootPath").addEventListener("input", (event) => {
    setDefaultStorageRootPath(event.target.value);
  });
  $("pickDefaultStorageRootButton").addEventListener("click", async () => {
    const path = await pickFolder("Checkpoint storage");
    if (path) {
      setDefaultStorageRootPath(path);
      setStatus("新規プロジェクトの既定保存先を変更しました。");
    }
  });
  $("pickStorageRootButton").addEventListener("click", async () => {
    const path = await pickFolder("Checkpoint storage");
    if (path) $("settingsNewStorageRootPath").value = path;
    updateControls();
  });
  $("setStorageRootButton").addEventListener("click", async () => {
    const storageRootPath = $("settingsNewStorageRootPath").value.trim();
    if (!storageRootPath) {
      setStatus("手動移動済みの保存データがある場所を選んでください。");
      return;
    }
    const current = $("settingsStorageRootPath").value;
    if (CheckPoFrontendState.samePathInput(storageRootPath, current)) {
      setStatus("現在参照している場所とは異なる場所を選んでください。");
      updateControls();
      return;
    }
    const confirmed = await confirmAction(
      `CheckPoはファイルを移動しません。${current} から選択した場所へ、このプロジェクトの保存データを手動で移動済みの場合だけ再接続できます。続行しますか？`,
      "保存データへ再接続",
    );
    if (!confirmed) return;
    await run("保存データへ再接続中", async () => {
      const snapshot = await invokeCommand("set_storage_root", {
        projectPath: getProjectPath(),
        storageRootPath,
        confirmed: true,
      });
      invalidateStoredSize();
      renderSnapshot(snapshot);
      await refreshLatestDiff({ allowBusy: true });
      setStatus("手動移動済みの保存データへ再接続しました。");
    });
  });
  $("analyzeGcButton").addEventListener("click", async () => {
    await run("不要なバックアップデータを確認中", async () => {
      const plan = await invokeCommand("analyze_gc", { projectPath: getProjectPath() });
      state.gcPlan = plan;
      const gc = CheckPoFrontendState.gcPlanPresentation(plan);
      $("gcSummary").textContent =
        `不要なバックアップデータ ${gc.totalCount} 件 / ${formatBytes(gc.totalBytes)}` +
        (gc.detailsTruncated
          ? `。一覧では ${gc.displayedCount} 件のみ表示していますが、残り ${gc.omittedCount} 件も確認済みの削除対象です。`
          : "");
      $("gcResult").textContent = plan.hasIntegrityProblems
        ? "破損または読み取れないチェックポイントがあるため削除できません。"
        : "削除前の確認が完了しました。";
      updateControls();
    });
  });
  $("applyGcButton").addEventListener("click", async () => {
    if (!state.gcPlan) {
      setStatus("先に不要なバックアップデータを確認してください。");
      return;
    }
    if (state.gcPlan.hasIntegrityProblems) {
      setStatus("破損または読み取れないチェックポイントがあるため削除できません。");
      return;
    }
    const gc = CheckPoFrontendState.gcPlanPresentation(state.gcPlan);
    if (gc.totalCount === 0) {
      setStatus("削除できる不要なバックアップデータはありません。");
      updateControls();
      return;
    }
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      confirmed = await confirmAction(
        `不要なバックアップデータ ${gc.totalCount} 件（${formatBytes(gc.totalBytes)}）を削除します。` +
        (gc.detailsTruncated
          ? `一覧に表示していない ${gc.omittedCount} 件も削除対象です。`
          : "") +
        "続行しますか？",
        "削除",
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run("不要なバックアップデータを削除中", async () => {
      const result = await invokeCommand("apply_gc", {
        ...CheckPoFrontendState.maintenanceApplyArgs(getProjectPath(), state.gcPlan),
      });
      invalidateStoredSize();
      state.gcPlan = null;
      const deletedCount = (result.deletedBlobCount ?? 0)
        + (result.deletedManifestChunkCount ?? 0)
        + (result.deletedInventoryNodeCount ?? 0);
      $("gcSummary").textContent = `削除 ${deletedCount} 件 / ${formatBytes(result.deletedBytes ?? 0)}`;
      $("gcResult").textContent = result.completed === false
        ? `一部を削除した後で停止しました。未処理 ${result.remainingCandidateCount ?? 0} 件です。詳しい内容は作業記録を確認してください。`
        : "不要なバックアップデータを削除しました。";
      await refreshProject();
    });
  });
  $("openProjectButton").addEventListener("click", async () => {
    const projectPath = $("projectPath").value.trim();
    if (!projectPath) {
      setStatus("Unity プロジェクトを選択してください。");
      updateControls();
      return;
    }
    state.hiddenProjectPaths.delete(projectPath);
    const storageRootPath = $("registrationStorageRootPath").value.trim();
    const createInitialCheckpoint = wantsInitialCheckpoint("registrationInitialCheckpoint");
    try {
      await run("プロジェクトを確認中", async () => {
        try {
          renderSnapshot(await invokeCommand("load_project", { projectPath }));
          setStatus("プロジェクトを開きました。");
        } catch (error) {
          if (error?.kind === "storageRootUnavailable" && storageRootPath) throw error;
          if (!shouldStartProjectAfterLoadError(error)) throw error;
          const snapshot = await invokeCommand("init_project", {
            projectPath,
            storageRootPath,
            createInitialCheckpoint,
          });
          renderStartedProject(snapshot, "プロジェクトを開始しました。");
        }
        await refreshLatestDiff({ allowBusy: true, metadataOnly: true });
        $("projectRegistrationOverlay").hidden = true;
      }, { rethrow: true, suppressError: true });
    } catch (error) {
      if (error?.kind === "storageRootUnavailable" && storageRootPath) {
        await reconnectProjectStorageAfterLoadFailure(projectPath, storageRootPath);
      } else if (isCopiedProjectError(error) || await isCopiedProjectAtPath(projectPath)) {
        await handleCopiedProjectRegistrationChoice(projectPath, storageRootPath);
      } else {
        showError(error);
      }
    }
  });
  $("refreshButton").addEventListener("click", () => run("再読み込み中", async () => {
    await refreshProject();
    await refreshLatestDiff({ allowBusy: true });
  }));
  $("rebuildIndexButton").addEventListener("click", async () => {
    const projectPath = getProjectPath();
    await run("一覧の索引を再構築中", async () => {
      const result = await invokeCommand("rebuild_index", { projectPath });
      const snapshot = await invokeCommand("refresh_project", { projectPath });
      if (state.projectPath !== projectPath) return;
      renderSnapshot(snapshot);
      const index = CheckPoFrontendState.checkpointIndexPresentation(state.checkpointIndex);
      if (!index.available) {
        setStatus("一覧の索引を再構築できませんでした。詳細を確認してください。");
        return;
      }
      await refreshLatestDiff({ allowBusy: true, metadataOnly: true });
      if ((result.unavailableReferencedObjectCount ?? 0) > 0 || result.errors?.length) {
        setStatus(
          `一覧の索引は再構築しましたが、参照データ ${result.unavailableReferencedObjectCount ?? 0} 件を利用できません。フル検証を実行してください。`,
        );
      } else {
        setStatus("一覧の索引を再構築しました。");
      }
    });
  });
  $("confirmProjectLocationButton").addEventListener("click", async () => {
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      const isCopied = state.projectLocationStatus === "copiedSuspected";
      confirmed = await confirmAction(
        isCopied
          ? copiedLocationConfirmMessage()
          : "この場所をプロジェクトの現在の場所として記録します。続行しますか？",
        "この場所を使う"
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run("場所を記録中", async () => {
      renderSnapshot(await invokeCommand("confirm_project_location", { projectPath: getProjectPath() }));
      await refreshLatestDiff({ allowBusy: true });
      setStatus("この場所をプロジェクトの現在の場所として記録しました。");
    });
  });
  $("startSeparateProjectButton").addEventListener("click", async () => {
    state.confirming = true;
    updateControls();
    let confirmed = false;
    let createInitialCheckpoint = true;
    try {
      const decision = await confirmAction(
        startSeparateProjectConfirmMessage(),
        "別プロジェクトとして開始",
        { initialCheckpointChoice: true }
      );
      confirmed = decision.confirmed;
      createInitialCheckpoint = decision.createInitialCheckpoint;
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run(createInitialCheckpoint ? "初回チェックポイントを作成中" : "別プロジェクトとして開始中", async () => {
      const snapshot = await invokeCommand("start_as_separate_project", {
        projectPath: getProjectPath(),
        storageRootPath: state.defaultStorageRootPath || "",
        confirmed: true,
        createInitialCheckpoint,
      });
      renderStartedProject(snapshot, "この場所を別プロジェクトとして開始しました。");
      await refreshLatestDiff({ allowBusy: true });
    });
  });
  $("createCheckpointButton").addEventListener("click", async () => {
    await run("作成中", async () => {
      const name = $("checkpointName").value.trim();
      if (!name) return;
      const created = await invokeCommand("create_checkpoint", {
        projectPath: getProjectPath(),
        name,
        initIfNeeded: false,
      });
      invalidateStoredSize();
      state.selectedCheckpointId = created.checkpointId || created.checkpoint_id || null;
      $("checkpointName").value = "";
      updateControls();
      await refreshProject();
      await refreshLatestDiff({ allowBusy: true });
      const warningText = checkpointWarningsText(created.warnings);
      if (warningText) {
        setOperationWarnings(created.warnings);
        setStatus(`チェックポイントを作成しましたが、警告があります。${warningText}`);
        setResult({
          warning: "チェックポイントを作成しましたが、警告があります。",
          details: created.warnings,
        });
      } else {
        setOperationWarnings([]);
        setStatus("チェックポイントを作成しました。");
      }
    });
  });
  $("deleteCheckpointButton")?.addEventListener("click", async () => {
    await deleteCheckpointById(getCheckpointId());
  });
  $("diffButton").addEventListener("click", runExactDiff);
  $("previewRollbackButton").addEventListener("click", async () => {
    await previewRestoreCheckpoint(getCheckpointId());
  });
  $("applyRollbackButton").addEventListener("click", async () => {
    const context = state.rollbackPlanContext;
    if (!state.rollbackPlan || !context
      || context.projectPath !== state.projectPath
      || context.checkpointId !== state.selectedCheckpointId) {
      state.rollbackPlan = null;
      state.rollbackPlanContext = null;
      updateControls();
      setStatus("先に preview を実行してください。");
      return;
    }
    await run("戻し中", async () => {
      const result = await invokeCommand("apply_restore", {
        projectPath: context.projectPath,
        checkpointId: context.checkpointId,
        expectedPlan: state.rollbackPlan,
        confirmed: true,
      });
      setBusyIndeterminate("再読み込み中");
      state.rollbackPlan = null;
      state.rollbackPlanContext = null;
      $("rollbackOverlay").hidden = true;
      invalidateStoredSize();
      await refreshProject();
      await refreshLatestDiff({ allowBusy: true });
      if (result.warnings?.length) {
        setOperationWarnings(result.warnings);
        setStatus("復元しましたが、後処理に警告があります。");
      } else {
        setOperationWarnings([]);
        setStatus("復元しました。");
      }
    });
  });
  $("verifyButton").addEventListener("click", async () => {
    await run("検証中", async () => {
      const result = await invokeCommand("verify_project", {
        projectPath: getProjectPath(),
        checkpointId: state.selectedCheckpointId,
        full: true,
      });
      $("verificationSummary").textContent = result.isValid ? "問題は見つかりませんでした。" : `問題があります: ${result.errors?.length ?? 0}`;
    }, {
      onCancelled: () => {
        $("verificationSummary").textContent = "検証を中止しました。";
      },
    });
  });
  $("verifyProjectButton").addEventListener("click", async () => {
    await run("検証中", async () => {
      const result = await invokeCommand("verify_project", {
        projectPath: getProjectPath(),
        checkpointId: null,
        full: true,
      });
      $("verificationSummary").textContent = result.isValid ? "問題は見つかりませんでした。" : `問題があります: ${result.errors?.length ?? 0}`;
    }, {
      onCancelled: () => {
        $("verificationSummary").textContent = "検証を中止しました。";
      },
    });
  });
  $("previewCleanupButton").addEventListener("click", async () => {
    await run("確認中", async () => {
      const plan = await invokeCommand("analyze_transaction_cleanup", {
        projectPath: getProjectPath(),
      });
      state.transactionCleanupPlan = plan;
      $("cleanupSummary").textContent =
        `復旧用データ ${plan.directoryCount ?? 0} 件 / `
        + `${plan.fileCount ?? 0} ファイル / ${formatBytes(plan.totalBytes ?? 0)}`;
      $("cleanupResult").textContent = plan.directoryCount
        ? "削除できる復旧用データを確認しました。"
        : "削除できる復旧用データはありません。";
      updateControls();
    });
  });
  $("applyCleanupButton").addEventListener("click", async () => {
    const plan = state.transactionCleanupPlan;
    if (!CheckPoFrontendState.transactionCleanupPlanHasCandidates(plan)) {
      setStatus("先に復旧用データの片付け内容を確認してください。");
      updateControls();
      return;
    }
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      confirmed = await confirmAction(
        `確認済みの復旧用データ ${plan.directoryCount} 件（${formatBytes(plan.totalBytes ?? 0)}）を削除します。`
        + "削除後は元に戻せません。続行しますか？",
        "削除",
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run("復旧用データを片付け中", async () => {
      const result = await invokeCommand("cleanup_journals", {
        projectPath: getProjectPath(),
        expectedPlan: plan,
        confirmed: true,
      });
      state.transactionCleanupPlan = null;
      $("cleanupSummary").textContent = t("cleanupEmpty");
      $("cleanupResult").textContent =
        `削除 ${result.deletedDirectoryCount ?? 0} 件 / ${formatBytes(result.deletedBytes ?? 0)}`;
      invalidateStoredSize();
      scheduleStorageSizeRefresh({ force: true });
      updateControls();
    });
  });
  $("previewTempCleanupButton").addEventListener("click", async () => {
    await run("一時ファイルを確認中", async () => {
      const plan = await invokeCommand("analyze_orphan_temp_files", { projectPath: getProjectPath() });
      state.tempCleanupPlan = plan;
      const warnings = plan.warnings?.length ? ` / 警告 ${plan.warnings.length} 件` : "";
      $("tempCleanupSummary").textContent =
        `一時ファイル ${plan.fileCount ?? 0} 件 / ${formatBytes(plan.totalBytes ?? 0)}${warnings}`;
      $("tempCleanupResult").textContent = "削除前の確認が完了しました。";
      updateControls();
    });
  });
  $("applyTempCleanupButton").addEventListener("click", async () => {
    if (!state.tempCleanupPlan) {
      setStatus("先に一時ファイルを確認してください。");
      return;
    }
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      confirmed = await confirmAction(
        `${state.tempCleanupPlan.fileCount ?? 0} 件の CheckPo 一時ファイルを削除します。続行しますか？`,
        "削除",
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run("一時ファイルを削除中", async () => {
      const result = await invokeCommand("cleanup_orphan_temp_files", {
        ...CheckPoFrontendState.maintenanceApplyArgs(getProjectPath(), state.tempCleanupPlan),
      });
      state.tempCleanupPlan = null;
      const warningCount = (result.plan?.warnings?.length ?? 0) + (result.warnings?.length ?? 0);
      $("tempCleanupSummary").textContent =
        `削除 ${result.deletedFileCount ?? 0} 件 / ${formatBytes(result.deletedBytes ?? 0)}`;
      $("tempCleanupResult").textContent = warningCount ? `警告 ${warningCount} 件` : "一時ファイルを削除しました。";
      await refreshLatestDiff({ allowBusy: true });
      updateControls();
    });
  });
  $("recoverTransactionsButton").addEventListener("click", async () => {
    await run("復旧中", async () => {
      const result = await invokeCommand("recover_transactions", { projectPath: getProjectPath() });
      state.failedTransactions = result.failedTransactions || [];
      setBusyIndeterminate("再読み込み中");
      await refreshProject();
      ensureRecoverySucceeded(result);
      await refreshLatestDiff({ allowBusy: true });
      setStatus(recoverySummary(result));
    });
  });
  $("cancelOperationButton").addEventListener("click", async () => {
    state.cancelRequested = true;
    $("cancelOperationButton").disabled = true;
    setBusyIndeterminate("中止中...");
    const result = await invokeCommand("cancel_current_operation");
    if (!result?.cancelled) {
      setStatus(t("operationNotCancellable"));
    }
  });
  $("installUpdateButton").addEventListener("click", installAvailableUpdate);
  $("checkUpdateButton").addEventListener("click", () => run(t("updateChecking"), async () => {
    await checkForUpdate();
  }));
  window.addEventListener("focus", queueFocusRefresh);
  document.addEventListener("visibilitychange", queueFocusRefresh);
}

window.addEventListener("DOMContentLoaded", async () => {
  applyTheme();
  applyI18n();
  renderUpdateBanner();
  bindEvents();
  renderProjectHistory();
  renderDefaultStorageRootPath();
  renderCheckpoints();
  updateProjectEmptyState();
  updateControls();
  setResult({});
  if (tauriListen) {
    tauriListen("operation-progress", (event) => renderProgress(event.payload));
    tauriListen("operation-close-pending", (event) => renderPendingClose(event.payload));
  }
  await restoreLastProject();
  checkForUpdate({ silent: true });
});
