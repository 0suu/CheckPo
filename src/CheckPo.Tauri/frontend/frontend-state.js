(function exposeFrontendState(root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.CheckPoFrontendState = api;
})(typeof globalThis === "object" ? globalThis : this, () => {
  const localizedErrorMessages = Object.freeze({
    user: "操作を完了できませんでした。詳しい内容を確認してください。",
    invalidProject: "Unity プロジェクトとして開けませんでした。選んだフォルダを確認してください。",
    invalidTrackedPath: "CheckPo が扱えないファイルパスが含まれています。",
    invalidId: "指定されたIDが正しくありません。",
    outsideTrackedScope: "Assets、Packages、ProjectSettings 以外のファイルは操作できません。",
    snapshotNotFound: "チェックポイントが見つかりません。再読み込みして選び直してください。",
    objectMissing: "チェックポイントの保存データが不足しています。破損チェックを実行してください。",
    objectHashMismatch: "チェックポイントの保存データが一致しません。破損チェックを実行してください。",
    workingTreeChanged: "処理中にUnity側のファイルが変わりました。保存が落ち着いてから、もう一度実行してください。",
    repositoryLocked: "別のCheckPo処理が実行中です。完了してから、もう一度実行してください。",
    storageRootConflict: "指定した場所と登録済みの保存先が一致しません。手動移動済みの保存データへ再接続してください。",
    storageRootUnavailable: "登録済みの保存データを読み込めません。手動移動済みの保存データがある場所を選んで再接続してください。",
    operationBusy: "別の処理が実行中です。完了してから、もう一度実行してください。",
    pendingTransaction: "中断された作業があります。先に復旧してください。",
    unresolvedTransactionQuarantine: "プロジェクトの状態を安全と確認できません。既知のチェックポイントへ全体復元してください。",
    transactionRecoveryFailed: "自動復旧できない作業があります。内容を安全な場所へ退避できます。",
    indexUnavailable: "一覧用データを読み込めませんでした。インデックスの再構築を実行してください。",
    unsupportedFormat: "この保存データは、より新しいCheckPoで作られています。データを変更せず、CheckPoを更新してください。",
    copiedProjectSuspected: "同じプロジェクトのコピーが見つかりました。使用する場所を確認してください。",
    unsafeFolderMetaOperation: "フォルダーの .meta だけを戻すことはできません。Unity上でフォルダーの状態を確認してください。",
    cancelled: "処理を中止しました。",
    io: "ファイルを読み書きできませんでした。空き容量、権限、他のアプリの使用状況を確認してください。",
    json: "CheckPoの保存データを読み込めませんでした。破損チェックを実行してください。",
    database: "一覧用データを読み込めませんでした。インデックスの再構築を実行してください。",
    corruption: "CheckPoの保存データに問題があります。破損チェックを実行してください。",
    confirmationRequired: "この操作には確認が必要です。",
    unexpected: "予期しない問題が発生しました。詳しい内容を確認してください。",
  });

  function projectIdentity(projectPath, project) {
    const id = String(project?.projectId || "");
    const path = String(projectPath || project?.projectRootPath || "");
    return `${id}\0${path}`;
  }

  function projectChanged(previousPath, previousProject, nextPath, nextProject) {
    return projectIdentity(previousPath, previousProject) !== projectIdentity(nextPath, nextProject);
  }

  function projectScopedStateReset() {
    return {
      checkpointIndex: { state: "current", rebuildable: false, detail: null },
      selectedCheckpointId: null,
      renamingCheckpointId: null,
      gcPlan: null,
      tempCleanupPlan: null,
      transactionCleanupPlan: null,
      rollbackPlan: null,
      rollbackPlanContext: null,
      rollbackRequestSerial: 0,
      pendingTransactions: [],
      unresolvedQuarantines: [],
      failedTransactions: [],
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
      queuedDiffRefreshOptions: null,
      currentDiffFilter: "all",
      diffTreeOpenPaths: new Set(),
      diffTreeTouched: false,
      currentDiffSelectedPaths: new Set(),
      lastSelectedChangePath: null,
    };
  }

  function diffResultIsComplete(currentDiff, diffRefreshFailure) {
    return Boolean(currentDiff)
      && !diffRefreshFailure
      && currentDiff.complete !== false
      && !(Array.isArray(currentDiff.unknown) && currentDiff.unknown.length > 0)
      && !(Array.isArray(currentDiff.warnings) && currentDiff.warnings.length > 0);
  }

  function diffResultIsProvisionalZero(diff) {
    return Boolean(diff)
      && diffChangeCount(diff) === 0
      && (!diff.exact
        || diff.complete === false
        || (Array.isArray(diff.unknown) && diff.unknown.length > 0)
        || (Array.isArray(diff.warnings) && diff.warnings.length > 0));
  }

  function warningBannerText(warningGroups, limit = 5) {
    const warnings = [];
    const seen = new Set();
    for (const group of Array.isArray(warningGroups) ? warningGroups : []) {
      for (const value of Array.isArray(group) ? group : []) {
        const warning = String(value || "").trim();
        if (!warning || seen.has(warning)) continue;
        seen.add(warning);
        warnings.push(warning);
      }
    }
    const safeLimit = Math.max(1, Number(limit) || 1);
    const shown = warnings.slice(0, safeLimit);
    const omitted = warnings.length - shown.length;
    return {
      count: warnings.length,
      text: `${shown.join(" / ")}${omitted > 0 ? ` / 他 ${omitted} 件` : ""}`,
    };
  }

  function gcPlanPresentation(plan) {
    const value = plan && typeof plan === "object" ? plan : {};
    const number = (input) => {
      const parsed = Number(input);
      return Number.isFinite(parsed) && parsed > 0 ? parsed : 0;
    };
    const objectCount = number(value.unreferencedBlobCount);
    const objectBytes = number(value.unreferencedLogicalBytes);
    const manifestChunkCount = number(value.unreferencedManifestChunkCount);
    const manifestChunkBytes = number(value.unreferencedManifestChunkBytes);
    const inventoryNodeCount = number(value.unreferencedInventoryNodeCount);
    const inventoryNodeBytes = number(value.unreferencedInventoryNodeBytes);
    const displayedCount = (Array.isArray(value.unreferencedBlobs) ? value.unreferencedBlobs.length : 0)
      + (Array.isArray(value.unreferencedManifestChunks) ? value.unreferencedManifestChunks.length : 0)
      + (Array.isArray(value.unreferencedInventoryNodes) ? value.unreferencedInventoryNodes.length : 0);
    const totalCount = objectCount + manifestChunkCount + inventoryNodeCount;
    return {
      objectCount,
      objectBytes,
      manifestChunkCount,
      manifestChunkBytes,
      inventoryNodeCount,
      inventoryNodeBytes,
      totalCount,
      totalBytes: objectBytes + manifestChunkBytes + inventoryNodeBytes,
      detailsTruncated: Boolean(value.detailsTruncated),
      displayedCount,
      omittedCount: Math.max(0, totalCount - displayedCount),
    };
  }

  function diffChangeCount(diff) {
    return (diff?.added?.length ?? 0)
      + (diff?.modified?.length ?? 0)
      + (diff?.deleted?.length ?? 0);
  }

  function latestDiffState(diff, exact) {
    const warnings = Array.isArray(diff?.warnings) ? diff.warnings.map(String) : [];
    const complete = diff?.complete !== false
      && !(Array.isArray(diff?.unknown) && diff.unknown.length > 0);
    const changeCount = diffChangeCount(diff);
    return {
      warnings,
      exact: Boolean(exact) && complete && warnings.length === 0,
      changeCount: !complete || warnings.length || (!exact && changeCount === 0)
        ? null
        : changeCount,
    };
  }

  function latestDiffCountText(changeCount, exact) {
    if (!Number.isFinite(changeCount) || (!exact && changeCount === 0)) return "…";
    return exact ? String(changeCount) : `${changeCount}+`;
  }

  function detectedDiffCountText(changeCount, marker = "") {
    return `${changeCount}${marker}`;
  }

  function isCancellationKind(kind) {
    return String(kind || "") === "cancelled";
  }

  function normalizedPathInput(value) {
    const trimmed = String(value || "").trim();
    if (!trimmed) return "";
    return trimmed.replace(/[\\/]+$/, "") || trimmed;
  }

  function samePathInput(left, right) {
    const normalizedLeft = normalizedPathInput(left);
    return Boolean(normalizedLeft) && normalizedLeft === normalizedPathInput(right);
  }

  function checkpointSearchText(checkpoint) {
    const value = checkpoint && typeof checkpoint === "object" ? checkpoint : {};
    return `${value.name || ""} ${value.checkpointId || ""} ${value.createdAtUtc || ""}`.toLowerCase();
  }

  function filterCheckpoints(checkpoints, query) {
    const values = Array.isArray(checkpoints) ? checkpoints : [];
    const normalizedQuery = String(query || "").trim().toLowerCase();
    if (!normalizedQuery) return values;
    return values.filter((checkpoint) => checkpointSearchText(checkpoint).includes(normalizedQuery));
  }

  function checkpointNavigationIndex(length, currentIndex, key, pageSize = 10) {
    const count = Math.max(0, Number(length) || 0);
    if (count === 0) return -1;
    const current = Number.isInteger(currentIndex) ? currentIndex : -1;
    const page = Math.max(1, Number(pageSize) || 1);
    if (key === "Home") return 0;
    if (key === "End") return count - 1;
    if (key === "ArrowDown") return current < 0 ? 0 : Math.min(count - 1, current + 1);
    if (key === "ArrowUp") return current < 0 ? count - 1 : Math.max(0, current - 1);
    if (key === "PageDown") return current < 0 ? 0 : Math.min(count - 1, current + page);
    if (key === "PageUp") return current < 0 ? count - 1 : Math.max(0, current - page);
    return current;
  }

  function transactionCleanupPlanHasCandidates(plan) {
    return Number(plan?.directoryCount || 0) > 0
      && Array.isArray(plan?.candidates)
      && plan.candidates.length > 0;
  }

  function restorePlanHasChanges(plan) {
    const operationCount = Number(plan?.restoreCount || 0)
      + Number(plan?.replaceCount || 0)
      + Number(plan?.deleteCount || 0)
      + Number(plan?.metadataCount || 0)
      + (plan?.directoriesToRemove?.length ?? 0)
      + (plan?.directoriesToCreate?.length ?? 0);
    return Boolean(plan?.hasChanges) && operationCount > 0;
  }

  function restorePlanCanApply(plan, unresolvedQuarantineCount = 0) {
    return restorePlanHasChanges(plan) || Number(unresolvedQuarantineCount || 0) > 0;
  }

  function restorePreviewIsRedundant(exactNoChanges, unresolvedQuarantineCount = 0) {
    return Boolean(exactNoChanges) && Number(unresolvedQuarantineCount || 0) <= 0;
  }

  function pathConfirmationPreview(paths, limit = 30) {
    const normalized = (Array.isArray(paths) ? paths : []).map((path) => String(path));
    const safeLimit = Math.max(1, Number(limit) || 1);
    const shown = normalized.slice(0, safeLimit);
    const omitted = normalized.length - shown.length;
    return {
      total: normalized.length,
      omitted,
      text: `${shown.join("\n")}${omitted > 0 ? `\n... 他 ${omitted} 件` : ""}`,
    };
  }

  function visibleProgressPhase(phase, uiOperationComplete = false) {
    if (uiOperationComplete) return "uiComplete";
    return phase === "complete" ? "backendComplete" : phase;
  }

  function operationProgressPercent(command, progress, uiOperationComplete = false) {
    const phase = visibleProgressPhase(progress?.phase, uiOperationComplete);
    if (phase === "uiComplete") return 100;
    if (phase === "backendComplete") return 99;
    const total = Number(progress?.total || 0);
    const completed = Number(progress?.completed || 0);
    if ([
      "create_checkpoint",
      "init_project",
      "start_as_separate_project",
    ].includes(command)) {
      const checkpointRanges = {
        scan: [0, 25],
        storeCheckpoint: [25, 70],
        writeCheckpointMetadata: [70, 75],
        syncCheckpoint: [75, 85],
        readbackCheckpoint: [85, 95],
        commitCheckpoint: [95, 99],
      };
      const range = checkpointRanges[phase];
      if (range) {
        const ratio = total > 0 ? Math.max(0, Math.min(1, completed / total)) : 1;
        return Math.min(99, Math.floor(range[0] + ((range[1] - range[0]) * ratio)));
      }
    }
    if (!(total > 0)) return undefined;
    const ratio = Math.max(0, Math.min(1, completed / total));
    return Math.min(99, Math.floor(ratio * 100));
  }

  function progressPhaseCanCancel(phase, cancellableAtStartOnly = false) {
    if ([
      "backingUp",
      "removingDirectories",
      "creatingDirectories",
      "applying",
      "finalizing",
      "committingIndex",
      "commitCheckpoint",
      "backendComplete",
      "uiComplete",
    ].includes(phase)) return false;
    if (!cancellableAtStartOnly) return true;
    return [
      "scan",
      "storeCheckpoint",
      "writeCheckpointMetadata",
      "syncCheckpoint",
      "readbackCheckpoint",
    ].includes(phase);
  }

  function checkpointIndexPresentation(status) {
    const normalized = status && typeof status === "object"
      ? status
      : { state: "current", rebuildable: false, detail: null };
    const state = String(normalized.state || "current");
    const messages = {
      missing: "チェックポイント一覧の索引がありません。再構築すると一覧を読み込めます。",
      stale: "チェックポイントの保存内容が変わったため、一覧の索引を更新する必要があります。",
      incompatible: "このバージョンで使える一覧索引へ再構築する必要があります。チェックポイント本体は削除されません。",
      corrupt: "チェックポイント一覧の索引を読み込めません。再構築して復旧してください。",
    };
    return {
      state,
      available: state === "current",
      rebuildable: state !== "current" && normalized.rebuildable !== false,
      message: messages[state] || (state === "current" ? "" : messages.corrupt),
      detail: normalized.detail || null,
    };
  }

  function storageSummaryWithRetainedSize(storage, previousStoredSize) {
    if (!storage) return null;
    if (storage.storedSizeBytes != null || previousStoredSize == null) return storage;
    return { ...storage, storedSizeBytes: previousStoredSize };
  }

  function localizedErrorDisplay(kind, raw, providedDetail) {
    const normalizedKind = String(kind || "generic");
    const rawText = String(raw || "不明なエラーです。");
    const message = localizedErrorMessages[normalizedKind] || rawText;
    const detail = providedDetail !== undefined && providedDetail !== null
      ? providedDetail
      : message !== rawText
        ? rawText
        : null;
    return detail === null
      ? { kind: normalizedKind, message }
      : { kind: normalizedKind, message, detail };
  }

  function retainPendingTransactionFailures(pendingTransactions, failedTransactions) {
    const pendingIds = new Set(
      (Array.isArray(pendingTransactions) ? pendingTransactions : [])
        .map((item) => item?.transactionId)
        .filter(Boolean),
    );
    return (Array.isArray(failedTransactions) ? failedTransactions : [])
      .filter((item) => pendingIds.has(item?.transactionId));
  }

  function selectChangePaths({
    selectedPaths,
    anchorPath,
    targetPath,
    visiblePaths,
    shiftKey = false,
    toggleKey = false,
  }) {
    const selected = new Set(selectedPaths);
    const anchorIndex = visiblePaths.indexOf(anchorPath);
    const targetIndex = visiblePaths.indexOf(targetPath);

    if (shiftKey && anchorPath && anchorIndex >= 0 && targetIndex >= 0) {
      const start = Math.min(anchorIndex, targetIndex);
      const end = Math.max(anchorIndex, targetIndex);
      for (const path of visiblePaths.slice(start, end + 1)) selected.add(path);
      return { selectedPaths: selected, anchorPath };
    }

    if (shiftKey || !toggleKey) {
      selected.clear();
      selected.add(targetPath);
    } else if (selected.has(targetPath)) {
      selected.delete(targetPath);
    } else {
      selected.add(targetPath);
    }

    return { selectedPaths: selected, anchorPath: targetPath };
  }

  function retainVisibleChangeSelection(selectedPaths, anchorPath, visiblePaths) {
    const visible = new Set(visiblePaths);
    return {
      selectedPaths: new Set([...selectedPaths].filter((path) => visible.has(path))),
      anchorPath: visible.has(anchorPath) ? anchorPath : null,
    };
  }

  function removeProjectFromHistory(projectHistory, projectPath) {
    return (Array.isArray(projectHistory) ? projectHistory : [])
      .filter((item) => item?.path !== projectPath);
  }

  function restorableLastProjectPath(projectHistory, lastProjectPath) {
    const path = String(lastProjectPath || "");
    return (Array.isArray(projectHistory) ? projectHistory : [])
      .some((item) => item?.path === path)
      ? path
      : null;
  }

  function mergeDiffRefreshOptions(previous, next) {
    if (!previous) return { ...next };
    return {
      ...previous,
      ...next,
      refreshProject: Boolean(previous.refreshProject || next.refreshProject),
      allowBusy: Boolean(previous.allowBusy || next.allowBusy),
      metadataOnly: previous.metadataOnly === true && next.metadataOnly === true,
      silent: previous.silent === true && next.silent === true,
    };
  }

  async function cancelAndWaitForIdle({
    isActive,
    cancel,
    sleep,
    timeoutMs,
    intervalMs,
    now = Date.now,
  }) {
    if (!isActive()) return true;
    await cancel();
    const deadline = now() + timeoutMs;
    while (isActive()) {
      const remaining = deadline - now();
      if (remaining <= 0) return false;
      await sleep(Math.min(intervalMs, remaining));
    }
    return true;
  }

  function buildChangeTreeModel(changes) {
    const root = createChangeTreeNode("", "");
    for (const change of changes) {
      const parts = String(change.path).split(/[\\/]/).filter(Boolean);
      const fileName = parts.pop() || change.path;
      let node = root;
      for (const part of parts) {
        const path = node.path ? `${node.path}/${part}` : part;
        if (!node.children.has(part)) {
          node.children.set(part, createChangeTreeNode(part, path));
        }
        node = node.children.get(part);
        node.counts[change.type] += 1;
      }
      node.files.push({ ...change, name: fileName });
    }
    return root;
  }

  function createChangeTreeNode(name, path) {
    return {
      name,
      path,
      children: new Map(),
      files: [],
      counts: { added: 0, modified: 0, deleted: 0 },
    };
  }

  function compressedChangeFolder(node) {
    const names = [node.name];
    let current = node;
    while (current.files.length === 0 && current.children.size === 1) {
      const next = [...current.children.values()][0];
      names.push(next.name);
      current = next;
    }
    return { node: current, names };
  }

  function flattenChangeTreeRows(root, openPaths) {
    const rows = [];
    const appendChildren = (parent, depth, parentKey) => {
      const folders = [...parent.children.values()]
        .sort(compareChangeTreeNode)
        .map(compressedChangeFolder);
      const files = [...parent.files].sort(compareChangeTreeFile);
      const setSize = folders.length + files.length;
      let position = 1;
      for (const folder of folders) {
        const key = `folder:${folder.node.path}`;
        const isOpen = openPaths.has(folder.node.path);
        rows.push({
          kind: "folder",
          key,
          parentKey,
          depth,
          posInSet: position,
          setSize,
          isOpen,
          node: folder.node,
          names: folder.names,
        });
        position += 1;
        if (isOpen) appendChildren(folder.node, depth + 1, key);
      }
      for (const file of files) {
        rows.push({
          kind: "file",
          key: `file:${file.path}`,
          parentKey,
          depth,
          posInSet: position,
          setSize,
          change: file,
        });
        position += 1;
      }
    };
    appendChildren(root, 0, null);
    return rows;
  }

  function collectChangeTreeFolderPaths(root, paths = []) {
    for (const child of root.children.values()) {
      const compressed = compressedChangeFolder(child);
      paths.push(compressed.node.path);
      collectChangeTreeFolderPaths(compressed.node, paths);
    }
    return paths;
  }

  function collectChangeTreeFilePaths(node, paths = []) {
    for (const file of node.files) paths.push(file.path);
    for (const child of node.children.values()) collectChangeTreeFilePaths(child, paths);
    return paths;
  }

  function virtualTreeWindowRange(rowCount, scrollTop, viewportHeight, rowHeight, overscan) {
    if (rowCount <= 0) return { start: 0, end: 0 };
    const safeRowHeight = Math.max(1, rowHeight);
    const firstVisible = Math.min(
      rowCount - 1,
      Math.max(0, Math.floor(Math.max(0, scrollTop) / safeRowHeight)),
    );
    const visibleEnd = Math.max(
      firstVisible + 1,
      Math.ceil((Math.max(0, scrollTop) + Math.max(0, viewportHeight)) / safeRowHeight),
    );
    const start = Math.max(0, firstVisible - overscan);
    const end = Math.min(rowCount, visibleEnd + overscan);
    return { start, end };
  }

  function compareChangeTreeNode(a, b) {
    return a.name.localeCompare(b.name, "ja");
  }

  function compareChangeTreeFile(a, b) {
    if (a.type !== b.type) return changeTypeOrder(a.type) - changeTypeOrder(b.type);
    return a.name.localeCompare(b.name, "ja");
  }

  function changeTypeOrder(type) {
    if (type === "added") return 0;
    if (type === "modified") return 1;
    return 2;
  }

  return {
    buildChangeTreeModel,
    cancelAndWaitForIdle,
    checkpointIndexPresentation,
    checkpointNavigationIndex,
    checkpointSearchText,
    collectChangeTreeFilePaths,
    collectChangeTreeFolderPaths,
    diffResultIsComplete,
    diffResultIsProvisionalZero,
    diffChangeCount,
    latestDiffState,
    latestDiffCountText,
    detectedDiffCountText,
    flattenChangeTreeRows,
    filterCheckpoints,
    gcPlanPresentation,
    isCancellationKind,
    projectChanged,
    projectIdentity,
    projectScopedStateReset,
    mergeDiffRefreshOptions,
    normalizedPathInput,
    operationProgressPercent,
    pathConfirmationPreview,
    progressPhaseCanCancel,
    removeProjectFromHistory,
    restorableLastProjectPath,
    localizedErrorDisplay,
    retainPendingTransactionFailures,
    retainVisibleChangeSelection,
    selectChangePaths,
    samePathInput,
    storageSummaryWithRetainedSize,
    transactionCleanupPlanHasCandidates,
    restorePlanHasChanges,
    restorePlanCanApply,
    restorePreviewIsRedundant,
    virtualTreeWindowRange,
    visibleProgressPhase,
    warningBannerText,
  };
});
