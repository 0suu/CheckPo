function renderTransactionQuarantineAction() {
  const actions = $("pendingTransactionActions");
  const guidanceElement = $("recoveryGuidance");
  const reasonElement = $("recoveryFailureReason");
  const recoverButton = $("recoverTransactionsButton");
  const failed = state.failedTransactions?.[0];
  let quarantineButton = $("quarantineTransactionButton");
  let selectFilesButton = $("selectRecoveryFilesButton");
  if (!failed) {
    quarantineButton?.remove();
    selectFilesButton?.remove();
    guidanceElement.hidden = true;
    reasonElement.textContent = "";
    recoverButton.hidden = false;
    recoverButton.textContent = t("recoverTransactions");
    return;
  }

  const guidance = CheckPoFrontendState.transactionRecoveryGuidance(failed);
  reasonElement.textContent = guidance.message;
  guidanceElement.hidden = false;
  recoverButton.hidden = !guidance.retryable;
  recoverButton.textContent = guidance.retryable
    ? "もう一度復旧"
    : t("recoverTransactions");

  if (guidance.canSelectFiles) {
    quarantineButton?.remove();
    if (!selectFilesButton) {
      selectFilesButton = document.createElement("button");
      selectFilesButton.id = "selectRecoveryFilesButton";
      selectFilesButton.type = "button";
      selectFilesButton.className = "button primary";
      selectFilesButton.addEventListener("click", openRecoveryConflictDialog);
      actions.append(selectFilesButton);
    }
    selectFilesButton.dataset.transactionId = failed.transactionId;
    selectFilesButton.textContent = "保存するファイルを選ぶ";
    return;
  }

  selectFilesButton?.remove();
  if (!guidance.canQuarantine) {
    quarantineButton?.remove();
    return;
  }
  if (!quarantineButton) {
    quarantineButton = document.createElement("button");
    quarantineButton.id = "quarantineTransactionButton";
    quarantineButton.type = "button";
    quarantineButton.className = "button danger-secondary";
    quarantineButton.addEventListener("click", quarantineFailedTransaction);
    actions.append(quarantineButton);
  }
  quarantineButton.dataset.transactionId = failed.transactionId;
  const remaining = state.failedTransactions.length;
  quarantineButton.textContent = remaining > 1
    ? `復旧データを退避（残り ${remaining} 件）`
    : "復旧データを安全な場所へ退避";
}

async function openRecoveryConflictDialog(event) {
  const transactionId = event?.currentTarget?.dataset.transactionId
    || state.failedTransactions?.[0]?.transactionId;
  if (!transactionId || state.busy) return;
  const plan = await run("変更されたファイルを確認中", async () => invokeCommand(
    "analyze_transaction_recovery_conflicts",
    { projectPath: getProjectPath(), transactionId },
  ));
  if (!plan) return;
  if (!Array.isArray(plan.conflicts) || plan.conflicts.length === 0) {
    setStatus(t("recoveryConflictNoChanges"));
    return;
  }
  state.recoveryConflictPlan = plan;
  $("recoveryExportRoot").value = "";
  clearDialogStatus("recoveryConflictStatus");
  renderRecoveryConflictPlan(plan);
  $("recoveryConflictOverlay").hidden = false;
  updateRecoveryConflictControls();
}

function renderRecoveryConflictPlan(plan) {
  const list = $("recoveryConflictList");
  const groups = CheckPoFrontendState.groupRecoveryConflicts(plan?.conflicts);
  list.replaceChildren(...groups.map((group) => {
    const label = document.createElement("label");
    label.className = "recovery-conflict-item";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = true;
    checkbox.dataset.recoveryPaths = JSON.stringify(group.paths);
    checkbox.addEventListener("change", updateRecoveryConflictControls);
    const copy = document.createElement("span");
    copy.className = "recovery-conflict-item-copy";
    const name = document.createElement("strong");
    name.textContent = group.label;
    const detail = document.createElement("small");
    detail.textContent = group.detail;
    copy.append(name, detail);
    const size = document.createElement("span");
    size.className = "recovery-conflict-size";
    size.textContent = formatBytes(group.sizeBytes);
    label.append(checkbox, copy, size);
    return label;
  }));
  $("recoverySelectAll").checked = true;
  $("recoverySelectAll").indeterminate = false;
}

function selectedRecoveryConflictPaths() {
  return Array.from($("recoveryConflictList").querySelectorAll("input[type='checkbox']:checked"))
    .flatMap((input) => {
      try {
        const paths = JSON.parse(input.dataset.recoveryPaths || "[]");
        return Array.isArray(paths) ? paths.map(String) : [];
      } catch (_) {
        return [];
      }
    });
}

function updateRecoveryConflictControls() {
  const checkboxes = Array.from(
    $("recoveryConflictList").querySelectorAll("input[type='checkbox']"),
  );
  const selectedGroups = checkboxes.filter((input) => input.checked).length;
  const selectedPaths = selectedRecoveryConflictPaths();
  const totalPaths = state.recoveryConflictPlan?.conflicts?.length ?? 0;
  $("recoverySelectAll").checked = checkboxes.length > 0 && selectedGroups === checkboxes.length;
  $("recoverySelectAll").indeterminate = selectedGroups > 0 && selectedGroups < checkboxes.length;
  $("recoverySelectionSummary").textContent = tf("recoveryConflictSelectionSummary", {
    selected: selectedGroups,
    total: checkboxes.length,
  });
  const notSaved = Math.max(0, totalPaths - selectedPaths.length);
  $("recoverySelectionWarning").hidden = notSaved === 0;
  $("recoverySelectionWarning").textContent = notSaved > 0
    ? tf("recoveryConflictNotSavedWarning", { count: notSaved })
    : "";
  updateControls();
}

function closeRecoveryConflictDialog() {
  $("recoveryConflictOverlay").hidden = true;
  state.recoveryConflictPlan = null;
  $("recoveryConflictList").replaceChildren();
  $("recoveryExportRoot").value = "";
  updateControls();
}

async function applyRecoveryConflictSelection({ withoutExport = false } = {}) {
  const plan = state.recoveryConflictPlan;
  const selectedPaths = withoutExport ? [] : selectedRecoveryConflictPaths();
  const exportRoot = withoutExport ? "" : $("recoveryExportRoot").value.trim();
  if (
    !plan
    || (!withoutExport && (selectedPaths.length === 0 || !exportRoot))
    || state.busy
    || state.confirming
  ) return;

  const conflictCount = plan.conflicts?.length ?? 0;
  const notSaved = withoutExport
    ? conflictCount
    : Math.max(0, conflictCount - selectedPaths.length);
  if (notSaved > 0) {
    state.confirming = true;
    updateControls();
    let confirmed = false;
    try {
      const message = withoutExport
        ? tf("recoveryConflictConfirmWithoutExport", { count: notSaved })
        : tf("recoveryConflictConfirmPartialExport", { count: notSaved });
      confirmed = await confirmAction(
        message,
        t(withoutExport
          ? "recoveryConflictApplyWithoutExport"
          : "recoveryConflictApplyWithExport"),
      );
    } finally {
      state.confirming = false;
      updateControls();
    }
    if (!confirmed) return;
  }

  const activity = withoutExport
    ? t("recoveryConflictApplyingWithoutExport")
    : t("recoveryConflictApplyingWithExport");
  const result = await run(activity, async () => {
    const recovered = await invokeCommand("recover_transaction_with_conflict_export", {
      projectPath: getProjectPath(),
      transactionId: plan.transactionId,
      expectedPlanId: plan.planId,
      selectedPaths,
      exportRoot,
      confirmed: true,
    });
    state.failedTransactions = state.failedTransactions
      .filter((item) => item.transactionId !== plan.transactionId);
    state.recoveryConflictPlan = null;
    $("recoveryConflictOverlay").hidden = true;
    setBusyIndeterminate(t("recoveryConflictRefreshing"));
    await refreshProject();
    if (state.pendingTransactions.length === 0) {
      await refreshLatestDiff({ allowBusy: true });
    }
    if (withoutExport) {
      setStatus(
        tf("recoveryConflictRecoveredWithoutExport", {
          count: recovered.restoredWithoutExportCount ?? conflictCount,
        }),
      );
    } else {
      setStatus(
        tf("recoveryConflictRecoveredWithExport", {
          count: selectedPaths.length,
          path: recovered.exportDirectory,
        }),
      );
    }
    setResult(recovered);
    return recovered;
  });
  if (result) {
    $("recoveryConflictList").replaceChildren();
    $("recoveryExportRoot").value = "";
  }
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
    const failures = Array.isArray(result?.failedTransactions) ? result.failedTransactions : [];
    const canSelectFiles = failures.length > 0
      && failures.every((failure) => Number(failure?.recoveryConflictCount || 0) > 0);
    const detail = canSelectFiles
      ? t("recoveryConflictSelectionGuidance")
      : "。上の案内を確認してください";
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
        + "\n\n復旧用データは削除せず保存し、Unityプロジェクト内のファイルもこの操作では変更しません。"
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
