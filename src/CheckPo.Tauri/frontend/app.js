const tauriInvoke = window.__TAURI__?.core?.invoke;
const tauriListen = window.__TAURI__?.event?.listen;
const RESULT_OUTPUT_MAX_CHARS = 20000;
const ROLLBACK_OPERATION_RENDER_LIMIT = 500;
const OPERATION_BUSY_RETRY_DELAYS_MS = [150, 300, 600, 1000, 1500];
const AUTO_REFRESH_WAIT_INTERVAL_MS = 100;

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
  checkpoints: [],
  selectedCheckpointId: null,
  renamingCheckpointId: null,
  storage: null,
  gcPlan: null,
  tempCleanupPlan: null,
  rollbackPlan: null,
  rollbackPlanContext: null,
  rollbackRequestSerial: 0,
  pendingTransactions: [],
  unresolvedQuarantines: [],
  failedTransactions: [],
  confirming: false,
  currentDiff: null,
  diffRequestSerial: 0,
  currentDiffFilter: "all",
  diffTreeOpenPaths: new Set(),
  diffTreeTouched: false,
  currentDiffSelectedPaths: new Set(),
  lastSelectedChangePath: null,
  busy: false,
  autoRefreshInFlight: false,
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
  if (trackOperation) {
    if (state.autoRefreshInFlight && state.busy) {
      setBusyIndeterminate(t("waitingForAutoRefresh"));
    }
    await waitForAutoRefreshToFinish();
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
        if (trackOperation && state.activeCommand === command) {
          renderProgressImmediately({ phase: "complete", completed: 1, total: 1 });
        }
        setResult(result);
        return result;
      } catch (error) {
        const canRetryBusy = command !== "cancel_current_operation"
          && errorKind(error) === "operationBusy";
        if (!canRetryBusy) throw error;
        if (state.autoRefreshInFlight && !options.fromAutoRefresh) {
          setStatus(t("waitingForAutoRefresh"));
          await waitForAutoRefreshToFinish();
          continue;
        }
        if (busyRetryIndex >= OPERATION_BUSY_RETRY_DELAYS_MS.length) throw error;
        if (busyRetryIndex === 0) setStatus("別の処理の完了を待っています。");
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

async function waitForAutoRefreshToFinish() {
  while (state.autoRefreshInFlight) {
    await sleep(AUTO_REFRESH_WAIT_INTERVAL_MS);
  }
}

async function run(title, task, options = {}) {
  if (state.busy) {
    setStatus(t("anotherOperationInProgress"));
    return;
  }
  state.busy = true;
  state.userOperationSerial += 1;
  clearVisibleError();
  $("busyOverlay").hidden = false;
  $("busyTitle").textContent = title;
  resetBusyProgress();
  setBusyIndeterminate(options.initialBusyLabel || t("starting"));
  updateControls();
  try {
    return await task();
  } catch (error) {
    if (!options.suppressError) {
      showError(error);
    }
    if (options.rethrow) {
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
  setStatus(display.message);
  setResult(display.detail ? { error: display.message, detail: display.detail } : { error: display.message });
  showVisibleError(display.message);
  if (!$('rollbackOverlay')?.hidden) {
    $("rollbackError").textContent = display.message;
    $("rollbackError").hidden = false;
  }
  if (["workingTreeChanged", "pendingTransaction"].includes(display.kind)) {
    state.rollbackPlan = null;
    state.rollbackPlanContext = null;
    state.rollbackRequestSerial += 1;
    if ($("rollbackConfirm")) $("rollbackConfirm").checked = false;
    updateControls();
  }
}

function showVisibleError(message) {
  $("errorBannerText").textContent = message;
  $("errorBanner").hidden = false;
}

function clearVisibleError() {
  if ($("errorBanner")) $("errorBanner").hidden = true;
  if ($("errorBannerText")) $("errorBannerText").textContent = "";
  if ($("rollbackError")) {
    $("rollbackError").hidden = true;
    $("rollbackError").textContent = "";
  }
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
  const snapshot = await invokeCommand(
    "refresh_project",
    { projectPath: getProjectPath() },
    { fromAutoRefresh: options.fromAutoRefresh },
  );
  renderSnapshot(snapshot);
  return snapshot;
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
  state.autoRefreshInFlight = true;
  updateAutoRefreshStatus();
  try {
    if (options.refreshProject) {
      await refreshProject({ fromAutoRefresh: true });
      if (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial) return;
    }
    const requestSerial = ++state.diffRequestSerial;
    const projectPath = getProjectPath();
    const checkpointId = diffBaselineCheckpointId();
    if (!checkpointId) {
      if (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial) return;
      state.currentDiff = null;
      $("diffSummary").textContent = t("diffEmpty");
      resetVirtualDiffTree();
      $("pendingFileCount").textContent = `0${t("fileUnit")}`;
      updateFilterChips(0, 0, 0);
      return;
    }
    if (!backgroundRefresh) {
      $("diffSummary").textContent = t("diffLoading");
    }
    const diffCommand = options.metadataOnly ? "diff_checkpoint_metadata" : "diff_checkpoint";
    const diff = await invokeCommand(
      diffCommand,
      {
        projectPath: getProjectPath(),
        checkpointId,
      },
      { fromAutoRefresh: true },
    );
    if (backgroundRefresh && startedUserOperationSerial !== state.userOperationSerial) {
      return;
    }
    if (requestSerial !== state.diffRequestSerial
      || projectPath !== state.projectPath
      || checkpointId !== diffBaselineCheckpointId()) return;
    renderDiff(diff, checkpointId);
    if (Array.isArray(diff?.warnings) && diff.warnings.length) {
      setStatus(`高速確認で一部確認できませんでした:\n${diff.warnings.join("\n")}`);
    }
  } catch (error) {
    clearCurrentDiff();
    if (!options.silent) showError(error);
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
  state.rollbackPlan = null;
  state.currentDiffSelectedPaths.clear();
  state.lastSelectedChangePath = null;
  $("diffSummary").textContent = t("diffEmpty");
  resetVirtualDiffTree();
  $("pendingFileCount").textContent = `0${t("fileUnit")}`;
  updateFilterChips(0, 0, 0);
  updateSelectedDiffButton();
  updateControls();
}

function scheduleFocusRefresh() {
  if (document.hidden || !state.projectPath || state.busy) return;
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
  if (CheckPoFrontendState.projectChanged(
    state.projectPath,
    state.project,
    nextProjectPath,
    nextProject,
  )) {
    Object.assign(state, CheckPoFrontendState.projectScopedStateReset());
    clearCurrentDiff();
  }
  state.projectPath = nextProjectPath;
  state.project = nextProject;
  state.projectLocationStatus = state.project?.locationStatus || "current";
  state.checkpoints = sortCheckpoints(snapshot.checkpoints || []);
  state.storage = snapshot.storage || null;
  state.gcPlan = null;
  state.tempCleanupPlan = null;
  if (!state.checkpoints.some((checkpoint) => checkpoint.checkpointId === state.selectedCheckpointId)) {
    state.selectedCheckpointId = state.checkpoints[0]?.checkpointId || null;
  }

  rememberProject(snapshot);
  renderProjectLabels();
  renderProjectHistory();
  renderCheckpoints();
  renderStorage();
  renderPending(snapshot.pendingTransactions || []);
  renderUnresolvedQuarantines(snapshot.unresolvedQuarantines || []);
  renderProjectWarnings(snapshot.project?.warnings || []);
  if (snapshot.warnings?.length) {
    setStatus(snapshot.warnings.join("\n"));
  }
  updateControls();
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
}

function renderProjectLabels() {
  const label = state.project?.projectName || basename(state.projectPath) || t("projectSelectPlaceholder");
  $("projectMenuLabel").textContent = label;
  $("projectMenuButton").classList.toggle("has-selection", Boolean(state.projectPath));
  if ($("projectRegistrationOverlay").hidden) {
    $("projectPath").value = state.projectPath || "";
  }
  $("settingsStorageRootPath").value = state.project?.storageRootPath ?? "-";
  $("settingsNewStorageRootPath").value = "";
  renderDefaultStorageRootPath();
  const active = selectedCheckpoint();
  if ($("selectedCheckpointLabel")) $("selectedCheckpointLabel").textContent = active ? active.name : t("noSelection");
  $("activeCheckpointTitle").textContent = active
    ? `${active.name || active.checkpointId} → ${t("workingFolder")}`
    : t("noSelection");
  $("projectStatusPath").textContent = state.projectPath || "-";
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
  $("storedSize").textContent = formatBytes(state.storage?.storedSizeBytes ?? 0);
  $("uniqueBlobs").textContent = String(state.storage?.uniqueBlobCount ?? "-");
  $("packCount").textContent = "-";
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
  $("pendingTransactionText").textContent =
    tf("pendingTransactionsMessage", {
      count: items.length,
      details: states ? ` (${states}${omitted})` : "",
    });
  renderTransactionQuarantineAction();
  updateControls();
}

function renderTransactionQuarantineAction() {
  const banner = $("pendingTransactionBanner");
  const failed = state.failedTransactions?.[0];
  let button = $("quarantineTransactionButton");
  if (!failed) {
    button?.remove();
    return;
  }
  if (!button) {
    button = document.createElement("button");
    button.id = "quarantineTransactionButton";
    button.type = "button";
    button.className = "button danger-secondary";
    button.addEventListener("click", quarantineFailedTransaction);
    banner.append(button);
  }
  button.dataset.transactionId = failed.transactionId;
  const remaining = state.failedTransactions.length;
  button.textContent = remaining > 1
    ? `復旧できない作業を退避（残り ${remaining} 件）`
    : "復旧できない作業を安全な場所へ退避";
}

function renderUnresolvedQuarantines(items) {
  state.unresolvedQuarantines = Array.isArray(items) ? items : [];
  const banner = $("unresolvedQuarantineBanner");
  banner.hidden = state.unresolvedQuarantines.length === 0;
  if (state.unresolvedQuarantines.length === 0) {
    $("unresolvedQuarantineText").textContent = "";
    updateControls();
    return;
  }
  const ids = state.unresolvedQuarantines
    .slice(0, 3)
    .map((item) => shortId(item.transactionId))
    .join(" / ");
  const omitted = state.unresolvedQuarantines.length > 3
    ? ` / 他 ${state.unresolvedQuarantines.length - 3} 件`
    : "";
  $("unresolvedQuarantineText").textContent =
    `状態を確認できない退避済み作業が ${state.unresolvedQuarantines.length} 件あります（${ids}${omitted}）。`
    + " 新規チェックポイント作成や削除、選択ファイルだけを戻す操作は停止しています。"
    + " 既知のチェックポイントを選び「このチェックポイントに戻す」で全体復元してください。";
  updateControls();
}

function shortId(value) {
  return String(value ?? "").slice(0, 8) || t("unknownTransaction");
}

function recoverySummary(result) {
  const recovered = result?.recoveredTransactionCount ?? 0;
  const failed = result?.failedTransactionCount ?? 0;
  if (failed > 0) {
    const detail = "。復旧できない作業は、安全な場所へ退避できます";
    return tf("recoveryFailed", { recovered, failed, detail });
  }
  return tf("recoverySucceeded", { count: recovered });
}

function ensureRecoverySucceeded(result) {
  if ((result?.failedTransactionCount ?? 0) > 0) {
    const error = new Error(recoverySummary(result));
    error.kind = "transactionRecoveryFailed";
    error.detail = result.failedTransactions || [];
    throw error;
  }
}

async function quarantineFailedTransaction(event) {
  const transactionId = event?.currentTarget?.dataset.transactionId
    || state.failedTransactions?.[0]?.transactionId;
  if (!transactionId || state.busy || state.confirming) return;

  state.confirming = true;
  updateControls();
  let confirmed = false;
  try {
    confirmed = await confirmAction(
      `自動復旧に失敗した作業 ${transactionId} を、CheckPoの自動復旧対象から外して安全な場所へ退避します。`
        + "\n\njournal・backup・stagedは削除せず保存し、Unityプロジェクト内のファイルもこの操作では変更しません。"
        + "ただし、現在のUnityプロジェクトが処理前の状態へ完全に戻っていない可能性があります。"
        + "退避後は正常なチェックポイントへ戻して状態を確認してください。続行しますか？",
      "安全な場所へ退避",
    );
  } finally {
    state.confirming = false;
    updateControls();
  }
  if (!confirmed) return;

  await run("復旧できない作業を退避中", async () => {
    const result = await invokeCommand("quarantine_transaction", {
      projectPath: getProjectPath(),
      transactionId,
      confirmed: true,
    });
    state.failedTransactions = state.failedTransactions
      .filter((item) => item.transactionId !== transactionId);
    setBusyIndeterminate("再読み込み中");
    await refreshProject();
    if (state.pendingTransactions.length === 0) {
      await refreshLatestDiff({ allowBusy: true });
    }
    const warning = result.warnings?.length
      ? " Unityプロジェクトが完全に戻っていない可能性があります。正常なチェックポイントへ戻して確認してください。"
      : "";
    setStatus(`復旧できない作業を安全な場所へ退避しました。${warning}`);
    setResult(result);
  });
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
    const previous = warning.previousProjectRootPath || "-";
    const current = warning.currentProjectRootPath || state.projectPath || "-";
    if (warning.previousMarkerHasSameProjectId || warning.locationStatus === "copiedSuspected") {
      return `同じ project_id が別の場所にも登録されています。以前の場所: ${previous} / 現在の場所: ${current}。コピーした Unity プロジェクトの可能性があります。`;
    }
    return `以前の登録場所から移動されています。以前の場所: ${previous} / 現在の場所: ${current}。`;
  }
  return warning?.message || "プロジェクトの状態に警告があります。";
}

function renderCheckpoints() {
  const query = $("checkpointSearch").value.trim().toLowerCase();
  const list = $("checkpointList");
  list.replaceChildren();
  const changeCount = currentChangeCount();
  if (!query && changeCount > 0) {
    const workingSection = document.createElement("section");
    workingSection.className = "checkpoint-section working-section";
    const heading = document.createElement("div");
    heading.className = "checkpoint-section-label";
    heading.textContent = t("workingFolder");
    const working = document.createElement("button");
    working.type = "button";
    working.className = "checkpoint-row working-row";
    working.innerHTML = `
      <span class="checkpoint-id mono">now</span>
      <strong class="checkpoint-title">未保存の変更</strong>
      <span class="checkpoint-meta">${changeCount}${t("fileUnit")}</span>
    `;
    working.addEventListener("click", () => refreshLatestDiff({ refreshProject: true }));
    workingSection.append(heading, working);
    list.append(workingSection);
  }
  const checkpoints = state.checkpoints.filter((checkpoint) => {
    const haystack = `${checkpoint.name} ${checkpoint.checkpointId} ${checkpoint.createdAtUtc}`.toLowerCase();
    return !query || haystack.includes(query);
  });
  const checkpointSection = document.createElement("section");
  checkpointSection.className = "checkpoint-section saved-section";
  if (!query && changeCount > 0) {
    const heading = document.createElement("div");
    heading.className = "checkpoint-section-label";
    heading.textContent = "チェックポイント";
    checkpointSection.append(heading);
  }
  if (checkpoints.length === 0) {
    const empty = document.createElement("p");
    empty.className = "empty-list";
    empty.textContent = "チェックポイントはありません。";
    checkpointSection.append(empty);
    list.append(checkpointSection);
    return;
  }
  for (const checkpoint of checkpoints) {
    const isRenaming = checkpoint.checkpointId === state.renamingCheckpointId;
    const row = document.createElement(isRenaming ? "div" : "button");
    if (!isRenaming) row.type = "button";
    row.className = `checkpoint-row${checkpoint.checkpointId === state.selectedCheckpointId ? " is-selected" : ""}${isRenaming ? " is-renaming" : ""}`;
    row.dataset.checkpointId = checkpoint.checkpointId;
    if (isRenaming) {
      row.setAttribute("role", "option");
      row.tabIndex = 0;
    }
    row.innerHTML = `
      <span class="checkpoint-id mono"></span>
      <strong class="checkpoint-title"></strong>
      <span class="checkpoint-meta"></span>
    `;
    row.querySelector(".checkpoint-id").textContent = String(checkpoint.checkpointId || "").slice(0, 4) || "----";
    row.querySelector(".checkpoint-meta").textContent =
      `${formatCompactDate(checkpoint.createdAtUtc)} · ${checkpoint.fileCount ?? 0} files`;
    if (isRenaming) {
      const title = row.querySelector(".checkpoint-title");
      const input = document.createElement("input");
      input.className = "checkpoint-rename-input";
      input.type = "text";
      input.value = checkpoint.name || checkpoint.checkpointId;
      input.setAttribute("aria-label", "チェックポイント名");
      title.replaceChildren(input);
      setupCheckpointRenameInput(input, checkpoint);
      requestAnimationFrame(() => {
        input.focus();
        input.select();
      });
    } else {
      row.querySelector(".checkpoint-title").textContent = checkpoint.name || checkpoint.checkpointId;
      row.addEventListener("click", async () => {
        selectCheckpoint(checkpoint.checkpointId);
        await refreshLatestDiff({ metadataOnly: true });
      });
    }
    row.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      selectCheckpoint(checkpoint.checkpointId, { render: !isRenaming });
      showCheckpointContextMenu(event.clientX, event.clientY, checkpoint);
    });
    checkpointSection.append(row);
  }
  list.append(checkpointSection);
}

function selectCheckpoint(checkpointId, options = {}) {
  const changed = state.selectedCheckpointId !== checkpointId;
  state.selectedCheckpointId = checkpointId;
  state.rollbackPlan = null;
  state.rollbackPlanContext = null;
  if (changed) state.rollbackRequestSerial += 1;
  if (changed) clearCurrentDiff();
  if (options.render !== false) renderCheckpoints();
  renderProjectLabels();
  updateControls();
}

function beginRenameCheckpoint(checkpointId) {
  if (state.busy || state.confirming) return;
  if (state.pendingTransactions.length > 0) {
    showError({ kind: "pendingTransaction", message: "A transaction must be recovered first" });
    return;
  }
  if (state.unresolvedQuarantines.length > 0) {
    setStatus("安全を確認できるまでチェックポイント名は変更できません。既知のチェックポイントへ全体復元してください。");
    return;
  }
  state.renamingCheckpointId = checkpointId;
  renderCheckpoints();
}

function setupCheckpointRenameInput(input, checkpoint) {
  let committing = false;
  const previousName = String(checkpoint.name || checkpoint.checkpointId || "").trim();
  const cancel = () => {
    if (committing) return;
    state.renamingCheckpointId = null;
    renderCheckpoints();
  };
  const commit = async () => {
    if (committing) return;
    const name = input.value.trim();
    if (!name) {
      setStatus("チェックポイント名を入力してください。");
      input.focus();
      return;
    }
    if (name === previousName) {
      cancel();
      return;
    }
    committing = true;
    await run("名前を変更中", async () => {
      const updated = await invokeCommand("rename_checkpoint", {
        projectPath: getProjectPath(),
        checkpointId: checkpoint.checkpointId,
        name,
      });
      const updatedId = updated.checkpointId || updated.checkpoint_id || checkpoint.checkpointId;
      state.checkpoints = state.checkpoints.map((item) => (
        item.checkpointId === updatedId ? { ...item, name: updated.name || name } : item
      ));
      if (state.currentDiff?.checkpoint?.checkpointId === updatedId) {
        state.currentDiff.checkpoint.name = updated.name || name;
      }
      state.renamingCheckpointId = null;
      renderCheckpoints();
      renderProjectLabels();
      setStatus("チェックポイント名を変更しました。");
    });
    if (state.renamingCheckpointId === checkpoint.checkpointId) committing = false;
  };
  input.addEventListener("click", (event) => event.stopPropagation());
  input.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    event.stopPropagation();
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commit();
    } else if (event.key === "Escape") {
      event.preventDefault();
      cancel();
    }
  });
  input.addEventListener("blur", () => {
    if (!input.value.trim()) {
      cancel();
      return;
    }
    commit();
  });
}

function showCheckpointContextMenu(x, y, checkpoint) {
  const locationBlocked = state.projectLocationStatus === "copiedSuspected";
  const pendingBlocked = state.pendingTransactions.length > 0;
  const quarantineBlocked = state.unresolvedQuarantines.length > 0;
  showContextMenu(x, y, [
    {
      label: "名前を変更",
      disabled: locationBlocked || pendingBlocked || quarantineBlocked,
      action: () => beginRenameCheckpoint(checkpoint.checkpointId),
    },
    {
      label: "この状態に戻す",
      disabled: locationBlocked || pendingBlocked,
      action: () => previewRestoreCheckpoint(checkpoint.checkpointId),
    },
    { separator: true },
    { label: "IDをコピー", action: () => copyCheckpointId(checkpoint.checkpointId) },
    { separator: true },
    {
      label: "削除",
      danger: true,
      disabled: locationBlocked || pendingBlocked || quarantineBlocked,
      action: () => deleteCheckpointById(checkpoint.checkpointId),
    },
  ]);
}

function showProjectContextMenu(x, y) {
  showContextMenu(x, y, [
    {
      label: "エクスプローラーで開く",
      disabled: !state.projectPath,
      action: openProjectInFileManager,
    },
  ]);
}

function showContextMenu(x, y, items) {
  const menu = $("contextMenu");
  if (!menu) return;
  menu.replaceChildren();
  for (const item of items) {
    if (item.separator) {
      const separator = document.createElement("div");
      separator.className = "context-menu-separator";
      separator.setAttribute("role", "separator");
      menu.append(separator);
      continue;
    }
    const button = document.createElement("button");
    button.type = "button";
    button.className = `context-menu-item${item.danger ? " danger" : ""}`;
    button.textContent = item.label;
    button.disabled = state.busy || state.confirming || Boolean(item.disabled);
    button.setAttribute("role", "menuitem");
    button.addEventListener("click", () => {
      hideContextMenu();
      if (!button.disabled) item.action();
    });
    menu.append(button);
  }
  menu.hidden = false;
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  requestAnimationFrame(() => {
    const rect = menu.getBoundingClientRect();
    const left = Math.min(x, window.innerWidth - rect.width - 8);
    const top = Math.min(y, window.innerHeight - rect.height - 8);
    menu.style.left = `${Math.max(8, left)}px`;
    menu.style.top = `${Math.max(8, top)}px`;
    menu.querySelector("button:not(:disabled)")?.focus();
  });
}

function hideContextMenu() {
  const menu = $("contextMenu");
  if (menu) menu.hidden = true;
}

async function copyCheckpointId(checkpointId) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(checkpointId);
    } else {
      const text = document.createElement("textarea");
      text.value = checkpointId;
      text.style.position = "fixed";
      text.style.opacity = "0";
      document.body.append(text);
      text.select();
      document.execCommand("copy");
      text.remove();
    }
    setStatus("チェックポイントIDをコピーしました。");
  } catch (error) {
    showError(error);
  }
}

async function previewRestoreCheckpoint(checkpointId) {
  if (state.pendingTransactions.length > 0) {
    showError({ kind: "pendingTransaction", message: "A transaction must be recovered first" });
    return;
  }
  selectCheckpoint(checkpointId);
  const requestSerial = ++state.rollbackRequestSerial;
  const projectPath = getProjectPath();
  await run("戻す内容を確認中", async () => {
    const plan = await invokeCommand("preview_restore", {
      projectPath,
      checkpointId,
    });
    if (requestSerial !== state.rollbackRequestSerial
      || projectPath !== state.projectPath
      || checkpointId !== state.selectedCheckpointId) return;
    renderRollbackPlan(plan, { projectPath, checkpointId });
  });
}

async function deleteCheckpointById(checkpointId) {
  if (state.pendingTransactions.length > 0) {
    showError({ kind: "pendingTransaction", message: "A transaction must be recovered first" });
    return;
  }
  if (state.unresolvedQuarantines.length > 0) {
    showError({
      kind: "unresolvedTransactionQuarantine",
      message: "A known checkpoint must be restored before deleting checkpoints",
    });
    return;
  }
  const checkpoint = state.checkpoints.find((item) => item.checkpointId === checkpointId);
  selectCheckpoint(checkpointId);
  state.confirming = true;
  updateControls();
  let confirmed = false;
  try {
    const name = checkpoint?.name || checkpointId;
    confirmed = await confirmAction(`「${name}」を削除します。続行しますか？`, "削除");
  } finally {
    state.confirming = false;
    updateControls();
  }
  if (!confirmed) return;
  await run("削除中", async () => {
    await invokeCommand("delete_checkpoint", { projectPath: getProjectPath(), checkpointId, confirmed: true });
    state.selectedCheckpointId = null;
    state.renamingCheckpointId = null;
    await refreshProject();
    await refreshLatestDiff({ allowBusy: true });
  });
}

async function openProjectInFileManager() {
  if (!state.projectPath) return;
  await run("エクスプローラーを開いています", async () => {
    await invokeCommand("open_project_in_file_manager", { projectPath: getProjectPath() });
    setStatus("Unityプロジェクトの場所を開きました。");
  });
}

function renderProjectHistory() {
  const list = $("projectSelectionList");
  if (!list) return;
  list.replaceChildren();
  if (state.projectHistory.length === 0) {
    const empty = document.createElement("p");
    empty.className = "empty-list";
    empty.textContent = "登録済みプロジェクトはありません。";
    list.append(empty);
    return;
  }
  for (const project of state.projectHistory) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `project-dialog-item${project.path === state.projectPath ? " is-active" : ""}`;
    button.innerHTML = "<strong></strong><span></span>";
    button.querySelector("strong").textContent = project.name || basename(project.path);
    button.querySelector("span").textContent = project.path;
    button.addEventListener("click", async () => {
      $("projectSelectionOverlay").hidden = true;
      await run("読み込み中", async () => {
        renderSnapshot(await invokeCommand("load_project", { projectPath: project.path }));
        await refreshLatestDiff({ allowBusy: true, metadataOnly: true });
      });
    });
    button.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      event.stopPropagation();
      showContextMenu(event.clientX, event.clientY, [{
        label: "一覧から消す（データは削除しません）",
        danger: true,
        action: () => forgetProjectFromHistory(project.path),
      }]);
    });
    list.append(button);
  }
}

function openProjectRegistration() {
  $("projectPath").value = "";
  $("registrationStorageRootPath").value = state.defaultStorageRootPath || "";
  resetInitialCheckpointChoice("registrationInitialCheckpoint");
  $("projectRegistrationOverlay").hidden = false;
}

function resetInitialCheckpointChoice(name) {
  const recommended = document.querySelector(`input[name="${name}"][value="yes"]`);
  if (recommended) recommended.checked = true;
}

function wantsInitialCheckpoint(name) {
  return document.querySelector(`input[name="${name}"]:checked`)?.value !== "no";
}

function checkpointWarningsText(warnings) {
  if (!warnings?.length) return "";
  return `警告 ${warnings.length} 件: ${warnings.join(" / ")}`;
}

function renderStartedProject(snapshot, successMessage) {
  renderSnapshot(snapshot);
  const initialCheckpointId =
    snapshot.initialCheckpoint?.checkpointId || snapshot.initialCheckpoint?.checkpoint_id || null;
  if (initialCheckpointId) {
    state.selectedCheckpointId = initialCheckpointId;
    renderCheckpoints();
    renderProjectLabels();
    updateControls();
    const warningText = checkpointWarningsText(snapshot.initialCheckpoint?.warnings);
    if (warningText) {
      setStatus(`初回チェックポイントを作成しましたが、警告があります。${warningText}`);
      setResult({
        warning: "初回チェックポイントを作成しましたが、警告があります。",
        details: snapshot.initialCheckpoint.warnings,
      });
    } else {
      setStatus("初回チェックポイントを作成しました。");
    }
  } else if (snapshot.initialCheckpointError) {
    setStatus(`プロジェクトは開始しましたが、初回チェックポイント作成に失敗しました: ${errorText(snapshot.initialCheckpointError)}`);
  } else if (snapshot.initialCheckpointCancelled) {
    setStatus("初回チェックポイントの作成を中止しました。");
  } else if (successMessage) {
    setStatus(successMessage);
  }
}

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
    renderDiff(diff, checkpointId);
    setStatus(t("diffUpdated"));
  });
}

function renderRollbackPlan(plan, context) {
  state.rollbackPlan = plan;
  state.rollbackPlanContext = context;
  clearVisibleError();
  const operations = plan.operations || [];
  $("rollbackSummary").textContent =
    `復元 ${plan.restoreCount ?? 0} / 置換 ${plan.replaceCount ?? 0} / 削除 ${plan.deleteCount ?? 0} / 一時容量 約${formatBytes(plan.estimatedTemporaryBytes ?? 0)} / 対象 ${operations.length} 件`;
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
    row.querySelector(".operation-type").textContent = operation.operationType;
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
  $("rollbackOverlay").hidden = false;
  updateControls();
}

function updateControls() {
  const hasProject = Boolean(state.projectPath);
  const hasCheckpoint = Boolean(state.selectedCheckpointId);
  const hasCheckpointName = Boolean($("checkpointName").value.trim());
  const locationMutationBlocked = state.projectLocationStatus === "copiedSuspected";
  const pendingMutationBlocked = state.pendingTransactions.length > 0;
  const quarantineMutationBlocked = state.unresolvedQuarantines.length > 0;
  const destructiveBlocked = locationMutationBlocked || pendingMutationBlocked || quarantineMutationBlocked;
  const controlsBlocked = state.busy || state.confirming;
  document.querySelectorAll("button").forEach((button) => {
    if (["confirmOkButton", "confirmCancelButton"].includes(button.id)) {
      button.disabled = state.busy;
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
      "openProjectButton",
      "settingsButton",
      "closeSettingsButton",
      "pickDefaultStorageRootButton",
    ].includes(button.id)) {
      button.disabled = controlsBlocked;
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
        || !state.rollbackPlan.hasChanges;
      return;
    }
    if (button.id === "applyGcButton") {
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject || !state.gcPlan || state.gcPlan.hasIntegrityProblems;
      return;
    }
    if (["previewRollbackButton", "diffButton", "verifyButton"].includes(button.id)) {
      const pendingBlocked = button.id === "previewRollbackButton" && pendingMutationBlocked;
      button.disabled = controlsBlocked || pendingBlocked || !hasProject || !hasCheckpoint;
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
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject || !hasCheckpointName;
      return;
    }
    if (button.id === "setStorageRootButton") {
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject;
      return;
    }
    if (["recoverTransactionsButton", "quarantineTransactionButton", "applyCleanupButton"].includes(button.id)) {
      button.disabled = controlsBlocked || locationMutationBlocked || !hasProject;
      return;
    }
    if (button.id === "applyTempCleanupButton") {
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject || !state.tempCleanupPlan || !state.tempCleanupPlan.fileCount;
      return;
    }
    if (button.dataset.destructive === "true") {
      button.disabled = controlsBlocked || destructiveBlocked || !hasProject;
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
  return [
    "この Unity プロジェクトは、すでに CheckPo に登録済みのプロジェクトをコピーした可能性があります。",
    "",
    `現在の場所: ${projectPath || "現在の場所"}`,
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

function copiedLocationConfirmMessage() {
  const warning = (state.projectWarnings || []).find((item) =>
    item?.kind === "copiedProjectSuspected" || item?.locationStatus === "copiedSuspected"
  );
  const previous = warning?.previousProjectRootPath || "以前の場所";
  const current = warning?.currentProjectRootPath || state.projectPath || "現在の場所";
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
  const previous = warning?.previousProjectRootPath || "コピー元の場所";
  const current = warning?.currentProjectRootPath || state.projectPath || "現在の場所";
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
    confirmed = await confirmAction(
      `${effectivePaths.length} 件の変更を戻します。\n\n${effectivePaths.join("\n")}\n\n続行しますか？`,
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
    await invokeCommand("apply_discard_files", {
      projectPath,
      paths,
      checkpointId,
      expectedPlan: plan,
      confirmed: true,
    });
    setBusyIndeterminate("再読み込み中");
    setStatus("変更を取り消しました。");
    await refreshProject();
    state.currentDiffSelectedPaths.clear();
    state.lastSelectedChangePath = null;
    await refreshLatestDiff({ allowBusy: true });
  });
}

function bindEvents() {
  $("dismissErrorButton").addEventListener("click", clearVisibleError);
  $("projectMenuButton").addEventListener("click", () => {
    renderProjectHistory();
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
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") hideContextMenu();
  });
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
  $("settingsButton").addEventListener("click", () => $("settingsOverlay").hidden = false);
  $("closeSettingsButton").addEventListener("click", () => $("settingsOverlay").hidden = true);
  $("advancedButton").addEventListener("click", () => $("advancedOverlay").hidden = false);
  $("closeAdvancedButton").addEventListener("click", () => $("advancedOverlay").hidden = true);
  $("closeRollbackDialogButton").addEventListener("click", () => $("rollbackOverlay").hidden = true);
  $("clearLogButton").addEventListener("click", () => $("logList").replaceChildren());
  $("openDiagnosticLogsButton").addEventListener("click", async () => {
    await run("診断ログを開いています", async () => {
      await invokeCommand("open_diagnostic_logs");
      setStatus("診断ログフォルダを開きました。");
    });
  });
  $("clearResultButton").addEventListener("click", () => setResult({}));
  $("checkpointSearch").addEventListener("input", renderCheckpoints);
  $("checkpointName").addEventListener("input", updateControls);
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
  });
  $("setStorageRootButton").addEventListener("click", async () => {
    const storageRootPath = $("settingsNewStorageRootPath").value.trim();
    if (!storageRootPath) {
      setStatus("新しいチェックポイント保存先を選んでください。");
      return;
    }
    const current = $("settingsStorageRootPath").value;
    const confirmed = await confirmAction(
      `保存先情報だけを変更します。ファイル移動は行いません。先に ${current} の repos フォルダ内にあるこのプロジェクトの保存データを、新しい保存先へ手動で移動してください。移動済みなら続行します。`,
      "保存先を変更",
    );
    if (!confirmed) return;
    await run("保存先を変更中", async () => {
      renderSnapshot(await invokeCommand("set_storage_root", {
        projectPath: getProjectPath(),
        storageRootPath,
        confirmed: true,
      }));
      await refreshLatestDiff({ allowBusy: true });
      setStatus("チェックポイント保存先を変更しました。");
    });
  });
  $("analyzeGcButton").addEventListener("click", async () => {
    await run("不要データを確認中", async () => {
      const plan = await invokeCommand("analyze_gc", { projectPath: getProjectPath() });
      state.gcPlan = plan;
      $("gcSummary").textContent =
        `不要 object ${plan.unreferencedBlobCount ?? 0} 件 / ${formatBytes(plan.unreferencedLogicalBytes ?? 0)}`;
      $("gcResult").textContent = plan.hasIntegrityProblems
        ? "破損または読み取れない checkpoint があるため削除できません。"
        : "削除前の確認が完了しました。";
      updateControls();
    });
  });
  $("applyGcButton").addEventListener("click", async () => {
    if (!state.gcPlan) {
      setStatus("先に不要データを調べてください。");
      return;
    }
    if (state.gcPlan.hasIntegrityProblems) {
      setStatus("破損または読み取れない checkpoint があるため削除できません。");
      return;
    }
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      confirmed = await confirmAction(
        `${state.gcPlan.unreferencedBlobCount ?? 0} 件の不要 object を削除します。続行しますか？`,
        "削除",
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run("不要データを削除中", async () => {
      const result = await invokeCommand("apply_gc", { projectPath: getProjectPath(), confirmed: true });
      state.gcPlan = null;
      $("gcSummary").textContent = `削除 ${result.deletedBlobCount ?? 0} 件 / ${formatBytes(result.deletedBytes ?? 0)}`;
      $("gcResult").textContent = "不要データを削除しました。";
      await refreshProject();
    });
  });
  $("openProjectButton").addEventListener("click", async () => {
    const projectPath = $("projectPath").value.trim();
    state.hiddenProjectPaths.delete(projectPath);
    const storageRootPath = $("registrationStorageRootPath").value.trim();
    const createInitialCheckpoint = wantsInitialCheckpoint("registrationInitialCheckpoint");
    try {
      await run("プロジェクトを確認中", async () => {
        try {
          renderSnapshot(await invokeCommand("load_project", { projectPath }));
          setStatus("プロジェクトを開きました。");
        } catch (error) {
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
      if (isCopiedProjectError(error) || await isCopiedProjectAtPath(projectPath)) {
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
      state.selectedCheckpointId = created.checkpointId || created.checkpoint_id || null;
      $("checkpointName").value = "";
      updateControls();
      await refreshProject();
      await refreshLatestDiff({ allowBusy: true });
      const warningText = checkpointWarningsText(created.warnings);
      if (warningText) {
        setStatus(`チェックポイントを作成しましたが、警告があります。${warningText}`);
        setResult({
          warning: "チェックポイントを作成しましたが、警告があります。",
          details: created.warnings,
        });
      } else {
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
      await invokeCommand("apply_restore", {
        projectPath: context.projectPath,
        checkpointId: context.checkpointId,
        expectedPlan: state.rollbackPlan,
        confirmed: true,
      });
      setBusyIndeterminate("再読み込み中");
      state.rollbackPlan = null;
      state.rollbackPlanContext = null;
      $("rollbackOverlay").hidden = true;
      await refreshProject();
      await refreshLatestDiff({ allowBusy: true });
      setStatus("復元しました。");
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
    });
  });
  $("previewCleanupButton").addEventListener("click", async () => {
    await run("確認中", async () => {
      const result = await invokeCommand("list_transactions", { projectPath: getProjectPath() });
      $("cleanupSummary").textContent = `${result.length ?? 0} 件の未完了 transaction`;
    });
  });
  $("applyCleanupButton").addEventListener("click", async () => {
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      confirmed = await confirmAction(
        "完了・復旧済み transaction の journal と backup を削除します。削除後は参照できません。続行しますか？",
        "削除",
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
    await run("片付け中", async () => {
      const result = await invokeCommand("cleanup_journals", {
        projectPath: getProjectPath(),
        confirmed: true,
      });
      $("cleanupResult").textContent = `削除 ${result.deletedDirectoryCount ?? 0} 件`;
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
        projectPath: getProjectPath(),
        confirmed: true,
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

function renderProgress(progress) {
  if (!state.busy || !state.activeCommand) return;
  if (!immediatelyCancellableCommands.has(state.activeCommand)
    && !progressCancellableStartCommands.has(state.activeCommand)) return;
  state.pendingProgress = progress;
  if (state.progressFrame !== null) return;
  state.progressFrame = requestAnimationFrame(() => {
    state.progressFrame = null;
    const latest = state.pendingProgress;
    state.pendingProgress = null;
    if (latest) renderProgressImmediately(latest);
  });
}

function renderProgressImmediately(progress) {
  if (!state.busy) return;
  if (progress?.phase === "complete") {
    if (state.progressFrame !== null) cancelAnimationFrame(state.progressFrame);
    state.progressFrame = null;
    state.pendingProgress = null;
  }
  const total = Number(progress?.total || 0);
  const completed = Number(progress?.completed || 0);
  const percent = total > 0 ? Math.max(0, Math.min(100, Math.floor((completed * 100) / total))) : undefined;
  const progressBar = $("busyProgress");
  $("busyCommand").textContent = progressPhaseLabel(progress?.phase);
  progressBar.max = 100;
  progressBar.removeAttribute("value");
  if (percent !== undefined) progressBar.value = percent;
  $("busyProgressText").textContent = total > 0
    ? `${completed}/${total}${progress?.currentItem ? ` ${compactProgressItem(progress.currentItem)}` : ""}`
    : compactProgressItem(progress?.currentItem || "");
  state.currentOperationCancellable = operationCanCancelAtProgress(progress);
  updateControls();
}

function operationCanCancelAtProgress(progress) {
  if (state.cancelRequested) return false;
  if (["applying", "finalizing", "complete"].includes(progress?.phase)) return false;
  if (progressCancellableStartCommands.has(state.activeCommand)) {
    return progress?.phase === "scan" || progress?.phase === "storeCheckpoint";
  }
  return immediatelyCancellableCommands.has(state.activeCommand);
}

function progressPhaseLabel(phase) {
  return ({
    scan: "ファイル確認中",
    storeCheckpoint: "保存中",
    planning: "戻す内容を確認中",
    staging: "復元準備中",
    applying: "書き戻し中",
    finalizing: "完了処理中",
    verifySnapshots: "チェックポイント確認中",
    verifyObjects: "保存データ確認中",
    rebuildIndex: "一覧を再構築中",
    complete: "完了",
  })[phase] || phase || "";
}

function compactProgressItem(item) {
  const text = String(item || "");
  if (text.length <= 72) return text;
  const parts = text.split("/");
  if (parts.length >= 3) {
    const compact = `${parts[0]}/.../${parts.slice(-2).join("/")}`;
    if (compact.length <= 72) return compact;
  }
  return `${text.slice(0, 34)}...${text.slice(-35)}`;
}

window.addEventListener("DOMContentLoaded", () => {
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
  }
  checkForUpdate({ silent: true });
});
