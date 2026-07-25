function renderProgress(progress) {
  if (!state.busy || !state.activeCommand) return;
  if (!immediatelyCancellableCommands.has(state.activeCommand)
    && !progressCancellableStartCommands.has(state.activeCommand)) return;
  if (!operationCanCancelAtProgress(progress) && state.currentOperationCancellable) {
    state.currentOperationCancellable = false;
    updateControls();
  }
  state.pendingProgress = progress;
  if (state.progressFrame !== null) return;
  state.progressFrame = requestAnimationFrame(() => {
    state.progressFrame = null;
    const latest = state.pendingProgress;
    state.pendingProgress = null;
    if (latest) renderProgressImmediately(latest);
  });
}

function renderProgressImmediately(progress, uiOperationComplete = false) {
  if (!state.busy) return;
  const visiblePhase = CheckPoFrontendState.visibleProgressPhase(
    progress?.phase,
    uiOperationComplete,
  );
  if (["backendComplete", "uiComplete"].includes(visiblePhase)) {
    if (state.progressFrame !== null) cancelAnimationFrame(state.progressFrame);
    state.progressFrame = null;
    state.pendingProgress = null;
  }
  const total = Number(progress?.total || 0);
  const completed = Number(progress?.completed || 0);
  const percent = CheckPoFrontendState.operationProgressPercent(
    state.activeCommand,
    progress,
    uiOperationComplete,
  );
  const progressBar = $("busyProgress");
  $("busyCommand").textContent = progressPhaseLabel(visiblePhase);
  progressBar.max = 100;
  progressBar.removeAttribute("value");
  if (percent !== undefined) progressBar.value = percent;
  $("busyProgressText").textContent = total > 0
    ? `${completed}/${total}${progress?.currentItem ? ` ${compactProgressItem(progress.currentItem)}` : ""}`
    : compactProgressItem(progress?.currentItem || "");
  state.currentOperationCancellable = operationCanCancelAtProgress({ ...progress, phase: visiblePhase });
  updateControls();
}

function operationCanCancelAtProgress(progress) {
  if (state.cancelRequested) return false;
  const cancellableAtStartOnly = progressCancellableStartCommands.has(state.activeCommand);
  if (!cancellableAtStartOnly && !immediatelyCancellableCommands.has(state.activeCommand)) return false;
  return CheckPoFrontendState.progressPhaseCanCancel(progress?.phase, cancellableAtStartOnly);
}

function progressPhaseLabel(phase) {
  return ({
    scan: "ファイル確認中",
    storeCheckpoint: "保存中",
    writeCheckpointMetadata: "チェックポイント情報を書き込み中",
    syncCheckpoint: "保存内容をディスクへ確定中",
    readbackCheckpoint: "保存内容を検証中",
    commitCheckpoint: "チェックポイントを公開中",
    planning: "戻す内容を確認中",
    staging: "復元準備中",
    backingUp: "変更を適用中",
    removingDirectories: "ディレクトリ削除中",
    creatingDirectories: "ディレクトリ作成中",
    applying: "書き戻し中",
    finalizing: "完了処理中",
    verifySnapshots: "チェックポイント確認中",
    verifyObjects: "保存データ確認中",
    rebuildIndex: "一覧を再構築中",
    readingSnapshots: "チェックポイント一覧を集計中",
    aggregatingReferences: "保存データの参照を集計中",
    checkingObjects: "保存データの存在を確認中",
    gcReadingSnapshots: "チェックポイントを確認中",
    gcCheckingReferences: "使用中のバックアップデータを確認中",
    gcEnumeratingObjects: "不要なバックアップデータを確認中",
    gcEnumeratingManifestChunks: "不要なバックアップデータを確認中",
    gcDeletingObjects: "不要なバックアップデータを削除中",
    gcDeletingManifestChunks: "不要なバックアップデータを削除中",
    gcDeletingInventoryNodes: "不要なバックアップデータを削除中",
    committingIndex: "一覧の更新を確定中",
    backendComplete: t("backendCommandComplete"),
    uiComplete: t("uiOperationComplete"),
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
