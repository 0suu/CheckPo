const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const {
  buildChangeTreeModel,
  cancelAndWaitForIdle,
  checkpointIndexPresentation,
  checkpointNavigationIndex,
  checkpointSearchText,
  collectChangeTreeFolderPaths,
  diffChangeCount,
  diffResultIsComplete,
  diffResultIsProvisionalZero,
  detectedDiffCountText,
  flattenChangeTreeRows,
  filterCheckpoints,
  gcPlanPresentation,
  isCancellationKind,
  latestDiffState,
  latestDiffCountText,
  localizedErrorDisplay,
  mergeDiffRefreshOptions,
  normalizedPathInput,
  operationProgressPercent,
  projectChanged,
  projectScopedStateReset,
  pathConfirmationPreview,
  progressPhaseCanCancel,
  removeProjectFromHistory,
  restorableLastProjectPath,
  retainPendingTransactionFailures,
  retainVisibleChangeSelection,
  restorePlanHasChanges,
  restorePlanCanApply,
  restorePreviewIsRedundant,
  samePathInput,
  selectChangePaths,
  storageSummaryWithRetainedSize,
  transactionCleanupPlanHasCandidates,
  virtualTreeWindowRange,
  visibleProgressPhase,
  warningBannerText,
} = require("./frontend-state.js");

test("zero-change restore remains available to resolve an unresolved quarantine", () => {
  const zeroPlan = { hasChanges: false };
  assert.equal(restorePlanCanApply(zeroPlan, 0), false);
  assert.equal(restorePlanCanApply(zeroPlan, 1), true);
  assert.equal(restorePreviewIsRedundant(true, 0), true);
  assert.equal(restorePreviewIsRedundant(true, 1), false);
  assert.equal(restorePreviewIsRedundant(false, 0), false);
});

test("project identity changes when either id or path changes", () => {
  const project = { projectId: "project-a", projectRootPath: "C:/avatar" };
  assert.equal(projectChanged("C:/avatar", project, "C:/avatar", { ...project }), false);
  assert.equal(projectChanged("C:/avatar", project, "D:/avatar", { ...project, projectRootPath: "D:/avatar" }), true);
  assert.equal(projectChanged("C:/avatar", project, "C:/avatar", { ...project, projectId: "project-b" }), true);
});

test("project-scoped reset does not share mutable selection state", () => {
  const first = projectScopedStateReset();
  const second = projectScopedStateReset();
  first.currentDiffSelectedPaths.add("Assets/A.prefab");
  first.diffTreeOpenPaths.add("Assets");
  assert.deepEqual([...second.currentDiffSelectedPaths], []);
  assert.deepEqual([...second.diffTreeOpenPaths], []);
  assert.equal(first.currentDiff, null);
  assert.equal(first.latestDiffCheckpointId, null);
  assert.equal(first.latestDiffChangeCount, null);
  assert.equal(first.latestDiffRefreshFailure, null);
  assert.deepEqual(first.latestDiffWarnings, []);
  assert.equal(first.rollbackPlan, null);
  assert.equal(first.gcPlan, null);
  assert.equal(first.tempCleanupPlan, null);
  assert.equal(first.transactionCleanupPlan, null);
  assert.deepEqual(first.pendingTransactions, []);
  assert.deepEqual(first.unresolvedQuarantines, []);
  assert.deepEqual(first.failedTransactions, []);
  assert.equal(first.currentDiffFilter, "all");
  assert.equal(first.selectedCheckpointId, null);
  assert.equal(first.checkpointIndex.state, "current");
  assert.deepEqual(first.snapshotWarnings, []);
  assert.deepEqual(first.operationWarnings, []);
  assert.equal(first.diffRefreshFailure, null);
});

test("stale or warned diffs cannot drive destructive file operations", () => {
  const complete = {
    added: ["Assets/A.asset"],
    complete: true,
    unknown: [],
    warnings: [],
  };
  assert.equal(diffResultIsComplete(complete, null), true);
  assert.equal(diffResultIsComplete({ ...complete, warnings: ["scan failed"] }, null), false);
  assert.equal(diffResultIsComplete({ ...complete, complete: false }, null), false);
  assert.equal(diffResultIsComplete({ ...complete, unknown: ["Assets/Unknown.asset"] }, null), false);
  assert.equal(diffResultIsComplete(complete, "refresh failed"), false);
  assert.equal(diffResultIsComplete(null, null), false);
});

test("warning banner output is deduplicated and bounded", () => {
  const result = warningBannerText([
    ["one", "two", "one"],
    ["three", "four"],
  ], 2);
  assert.equal(result.count, 4);
  assert.equal(result.text, "one / two / 他 2 件");
});

test("GC presentation includes manifest-only reclaimable data", () => {
  const result = gcPlanPresentation({
    unreferencedBlobCount: 0,
    unreferencedLogicalBytes: 0,
    unreferencedManifestChunkCount: 1,
    unreferencedManifestChunkBytes: 4096,
    unreferencedInventoryNodeCount: 0,
    unreferencedInventoryNodeBytes: 0,
    unreferencedBlobs: [],
    unreferencedManifestChunks: [{ chunkPath: "manifests/v2/leaves/aa/id" }],
    unreferencedInventoryNodes: [],
    detailsTruncated: false,
  });
  assert.deepEqual(result, {
    objectCount: 0,
    objectBytes: 0,
    manifestChunkCount: 1,
    manifestChunkBytes: 4096,
    inventoryNodeCount: 0,
    inventoryNodeBytes: 0,
    totalCount: 1,
    totalBytes: 4096,
    detailsTruncated: false,
    displayedCount: 1,
    omittedCount: 0,
  });
});

test("GC presentation reports candidates omitted from truncated details", () => {
  const result = gcPlanPresentation({
    unreferencedBlobCount: 1005,
    unreferencedLogicalBytes: 1005,
    unreferencedManifestChunkCount: 2,
    unreferencedManifestChunkBytes: 2,
    unreferencedInventoryNodeCount: 0,
    unreferencedInventoryNodeBytes: 0,
    unreferencedBlobs: Array.from({ length: 1000 }, (_, index) => ({ objectId: String(index) })),
    unreferencedManifestChunks: [{ chunkPath: "one" }, { chunkPath: "two" }],
    unreferencedInventoryNodes: [],
    detailsTruncated: true,
  });

  assert.equal(result.totalCount, 1007);
  assert.equal(result.displayedCount, 1002);
  assert.equal(result.omittedCount, 5);
  assert.equal(result.detailsTruncated, true);
});

test("diff counts and cancellation outcomes are classified explicitly", () => {
  assert.equal(diffChangeCount({
    added: ["A"],
    modified: ["B", "C"],
    deleted: ["D"],
  }), 4);
  assert.equal(diffChangeCount(null), 0);
  assert.equal(isCancellationKind("cancelled"), true);
  assert.equal(isCancellationKind("io"), false);
});

test("storage input comparison trims whitespace and trailing separators only", () => {
  assert.equal(normalizedPathInput(" /Volumes/Data/// "), "/Volumes/Data");
  assert.equal(samePathInput("/Volumes/Data/", "/Volumes/Data"), true);
  assert.equal(samePathInput("/Volumes/Data", "/volumes/data"), false);
  assert.equal(samePathInput("", ""), false);
});

test("checkpoint search is case-insensitive and preserves matching object identity", () => {
  const checkpoints = Array.from({ length: 10_000 }, (_, index) => ({
    checkpointId: `checkpoint-${index}`,
    name: index === 9_999 ? "Milestone Final" : `checkpoint ${index}`,
    createdAtUtc: "2026-07-14T00:00:00Z",
  }));
  const result = filterCheckpoints(checkpoints, "  MILESTONE final ");
  assert.deepEqual(result, [checkpoints[9_999]]);
  assert.equal(result[0], checkpoints[9_999]);
  assert.equal(filterCheckpoints(checkpoints, "").length, 10_000);
  assert.match(checkpointSearchText(checkpoints[9_999]), /milestone final/);
});

test("cleanup and restore plans require real targets", () => {
  assert.equal(transactionCleanupPlanHasCandidates({
    directoryCount: 1,
    candidates: [{ transactionId: "tx" }],
  }), true);
  assert.equal(transactionCleanupPlanHasCandidates({ directoryCount: 0, candidates: [] }), false);
  assert.equal(restorePlanHasChanges({
    hasChanges: true,
    restoreCount: 0,
    replaceCount: 0,
    deleteCount: 0,
    metadataCount: 0,
  }), false);
  assert.equal(restorePlanHasChanges({ hasChanges: true, restoreCount: 1 }), true);
  assert.equal(restorePlanHasChanges({ hasChanges: true, metadataCount: 1 }), true);
  assert.equal(restorePlanHasChanges({
    hasChanges: true,
    directoriesToRemove: ["Assets/EmptyFolder"],
  }), true);
});

test("large path confirmations do not include every path", () => {
  const paths = Array.from({ length: 5000 }, (_, index) => `Assets/Folder/File${index}.asset`);
  const preview = pathConfirmationPreview(paths, 20);
  assert.equal(preview.total, 5000);
  assert.equal(preview.omitted, 4980);
  assert.match(preview.text, /他 4980 件/);
  assert.ok(preview.text.length < 1000);
  assert.doesNotMatch(preview.text, /File4999/);
});

test("backend completion stays distinct from whole UI completion", () => {
  assert.equal(visibleProgressPhase("complete"), "backendComplete");
  assert.equal(visibleProgressPhase("complete", true), "uiComplete");
  assert.equal(visibleProgressPhase("scan"), "scan");
});

test("checkpoint progress is monotonic and reaches 100 only after UI completion", () => {
  const phases = [
    ["scan", 25],
    ["storeCheckpoint", 70],
    ["writeCheckpointMetadata", 75],
    ["syncCheckpoint", 85],
    ["readbackCheckpoint", 95],
    ["commitCheckpoint", 99],
  ];
  let previous = -1;
  for (const [phase, expectedEnd] of phases) {
    const start = operationProgressPercent("create_checkpoint", { phase, completed: 0, total: 10 });
    const end = operationProgressPercent("create_checkpoint", { phase, completed: 10, total: 10 });
    assert.ok(start >= previous, `${phase} must not move progress backwards`);
    assert.equal(end, expectedEnd);
    previous = end;
  }
  assert.equal(operationProgressPercent("create_checkpoint", {
    phase: "complete",
    completed: 1,
    total: 1,
  }), 99);
  assert.equal(operationProgressPercent("create_checkpoint", {
    phase: "commitCheckpoint",
    completed: 1,
    total: 1,
  }, true), 100);
  assert.equal(operationProgressPercent("verify_project", {
    phase: "verifyObjects",
    completed: 10,
    total: 10,
  }), 99);
  assert.equal(operationProgressPercent("create_checkpoint", {
    phase: "readbackCheckpoint",
    completed: 0,
    total: 0,
  }), 95);
});

test("initial project checkpoint commands use the monotonic checkpoint phase ranges", () => {
  for (const command of ["init_project", "start_as_separate_project"]) {
    assert.equal(operationProgressPercent(command, {
      phase: "scan",
      completed: 10,
      total: 10,
    }), 25);
    assert.equal(operationProgressPercent(command, {
      phase: "storeCheckpoint",
      completed: 0,
      total: 10,
    }), 25);
    assert.equal(operationProgressPercent(command, {
      phase: "commitCheckpoint",
      completed: 10,
      total: 10,
    }), 99);
    assert.equal(operationProgressPercent(command, {
      phase: "complete",
      completed: 1,
      total: 1,
    }), 99);
  }
});

test("virtual checkpoint history supports logical keyboard navigation", () => {
  assert.equal(checkpointNavigationIndex(1000, -1, "ArrowDown"), 0);
  assert.equal(checkpointNavigationIndex(1000, -1, "ArrowUp"), 999);
  assert.equal(checkpointNavigationIndex(1000, 499, "Home"), 0);
  assert.equal(checkpointNavigationIndex(1000, 499, "End"), 999);
  assert.equal(checkpointNavigationIndex(1000, 499, "PageDown", 20), 519);
  assert.equal(checkpointNavigationIndex(1000, 499, "PageUp", 20), 479);
  assert.equal(checkpointNavigationIndex(0, -1, "ArrowDown"), -1);
});

test("checkpoint commit and mutation phases cannot be cancelled", () => {
  assert.equal(progressPhaseCanCancel("readbackCheckpoint"), true);
  assert.equal(progressPhaseCanCancel("commitCheckpoint"), false);
  assert.equal(progressPhaseCanCancel("backingUp"), false);
  assert.equal(progressPhaseCanCancel("scan", true), true);
  assert.equal(progressPhaseCanCancel("planning", true), false);
});

test("checkpoint index states distinguish unavailable history from an empty history", () => {
  const current = checkpointIndexPresentation({ state: "current", rebuildable: false });
  const missing = checkpointIndexPresentation({
    state: "missing",
    rebuildable: true,
    detail: "index file is absent",
  });
  const corrupt = checkpointIndexPresentation({ state: "corrupt", rebuildable: true });

  assert.equal(current.available, true);
  assert.equal(current.message, "");
  assert.equal(missing.available, false);
  assert.equal(missing.rebuildable, true);
  assert.match(missing.message, /索引がありません/);
  assert.equal(missing.detail, "index file is absent");
  assert.equal(corrupt.available, false);
  assert.match(corrupt.message, /読み込めません/);
});

test("missing checkpoint index can render without dereferencing a null storage summary", () => {
  assert.equal(storageSummaryWithRetainedSize(null, 2048), null);
  assert.deepEqual(
    storageSummaryWithRetainedSize({ checkpointCount: 3, storedSizeBytes: null }, 2048),
    { checkpointCount: 3, storedSizeBytes: 2048 },
  );
  assert.deepEqual(
    storageSummaryWithRetainedSize({ checkpointCount: 3, storedSizeBytes: 1024 }, 2048),
    { checkpointCount: 3, storedSizeBytes: 1024 },
  );
});

test("known backend errors use actionable Japanese text and preserve raw detail", () => {
  const display = localizedErrorDisplay(
    "unsupportedFormat",
    "unsupported snapshot schema version 2; this CheckPo supports version 1",
  );

  assert.match(display.message, /CheckPoを更新/);
  assert.match(display.detail, /snapshot schema/);
  assert.equal(display.kind, "unsupportedFormat");
});

test("folder metadata rejection has a specific user-facing error", () => {
  const display = localizedErrorDisplay(
    "unsafeFolderMetaOperation",
    "folder metadata cannot be changed independently: Assets/Folder.meta",
  );
  assert.match(display.message, /\.meta だけを戻すことはできません/);
  assert.match(display.detail, /Assets\/Folder\.meta/);
});

test("unknown errors keep their original message", () => {
  const display = localizedErrorDisplay("custom", "custom failure");

  assert.deepEqual(display, { kind: "custom", message: "custom failure" });
});

test("structured recovery details are preserved for the detailed result view", () => {
  const failures = [{ transactionId: "abc", error: "raw recovery error" }];
  const display = localizedErrorDisplay(
    "transactionRecoveryFailed",
    "recovery failed",
    failures,
  );

  assert.deepEqual(display.detail, failures);
  assert.match(display.message, /安全な場所へ退避/);
});

test("only failures that are still pending remain available for quarantine", () => {
  const failures = retainPendingTransactionFailures(
    [{ transactionId: "pending-b" }, { transactionId: "pending-c" }],
    [
      { transactionId: "resolved-a", error: "resolved" },
      { transactionId: "pending-b", error: "failed" },
    ],
  );

  assert.deepEqual(failures, [{ transactionId: "pending-b", error: "failed" }]);
});

test("shift selection follows visible tree order", () => {
  const result = selectChangePaths({
    selectedPaths: new Set(["Assets/A.prefab"]),
    anchorPath: "Assets/A.prefab",
    targetPath: "ProjectSettings/C.asset",
    visiblePaths: ["Assets/A.prefab", "Packages/B.json", "ProjectSettings/C.asset"],
    shiftKey: true,
  });
  assert.deepEqual([...result.selectedPaths], [
    "Assets/A.prefab",
    "Packages/B.json",
    "ProjectSettings/C.asset",
  ]);
  assert.equal(result.anchorPath, "Assets/A.prefab");
});

test("shift selection can cross rows outside the virtual DOM window", () => {
  const visiblePaths = Array.from({ length: 200 }, (_, index) => `Assets/File${index}.asset`);
  const result = selectChangePaths({
    selectedPaths: new Set([visiblePaths[5]]),
    anchorPath: visiblePaths[5],
    targetPath: visiblePaths[150],
    visiblePaths,
    shiftKey: true,
  });

  assert.equal(result.selectedPaths.size, 146);
  assert.ok(result.selectedPaths.has(visiblePaths[100]));
});

test("shift selection becomes a single selection when anchor is hidden", () => {
  const result = selectChangePaths({
    selectedPaths: new Set(["Assets/Hidden.prefab", "Assets/Old.prefab"]),
    anchorPath: "Assets/Hidden.prefab",
    targetPath: "Assets/Visible.prefab",
    visiblePaths: ["Assets/Visible.prefab"],
    shiftKey: true,
  });
  assert.deepEqual([...result.selectedPaths], ["Assets/Visible.prefab"]);
  assert.equal(result.anchorPath, "Assets/Visible.prefab");
});

test("ctrl selection toggles only the target", () => {
  const result = selectChangePaths({
    selectedPaths: new Set(["Assets/A.prefab", "Assets/B.prefab"]),
    anchorPath: "Assets/B.prefab",
    targetPath: "Assets/A.prefab",
    visiblePaths: ["Assets/A.prefab", "Assets/B.prefab"],
    toggleKey: true,
  });
  assert.deepEqual([...result.selectedPaths], ["Assets/B.prefab"]);
  assert.equal(result.anchorPath, "Assets/A.prefab");
});

test("filter changes remove selections that are no longer visible", () => {
  const result = retainVisibleChangeSelection(
    new Set(["Assets/Added.asset", "Assets/Deleted.asset"]),
    "Assets/Added.asset",
    ["Assets/Deleted.asset"],
  );

  assert.deepEqual([...result.selectedPaths], ["Assets/Deleted.asset"]);
  assert.equal(result.anchorPath, null);
});

test("removing a project from history does not alter the other entries", () => {
  const history = [
    { path: "C:/AvatarA", name: "A" },
    { path: "D:/AvatarB", name: "B" },
  ];

  assert.deepEqual(removeProjectFromHistory(history, "C:/AvatarA"), [history[1]]);
  assert.equal(history.length, 2);
});

test("last project is restored only while it remains registered", () => {
  const history = [{ path: "C:/AvatarA" }, { path: "D:/AvatarB" }];

  assert.equal(restorableLastProjectPath(history, "D:/AvatarB"), "D:/AvatarB");
  assert.equal(restorableLastProjectPath(history, "E:/Removed"), null);
  assert.equal(restorableLastProjectPath(history, ""), null);
});

test("queued diff refresh never weakens an exact request to metadata-only", () => {
  const queued = mergeDiffRefreshOptions(
    { refreshProject: true },
    { silent: true, metadataOnly: true },
  );

  assert.equal(queued.refreshProject, true);
  assert.equal(queued.metadataOnly, false);
  assert.equal(queued.silent, false);
});

test("metadata-only zero diff remains unknown instead of claiming exact zero", () => {
  assert.deepEqual(latestDiffState({
    added: [],
    modified: [],
    deleted: [],
    warnings: [],
  }, false), {
    warnings: [],
    exact: false,
    changeCount: null,
  });
});

test("metadata-only zero diff is presented as provisional", () => {
  const diff = {
    added: [], modified: [], deleted: [], warnings: [], exact: false,
  };
  assert.equal(diffResultIsProvisionalZero(diff), true);
  assert.equal(diffResultIsProvisionalZero({ ...diff, exact: true }), false);
  assert.equal(diffResultIsProvisionalZero({
    ...diff, added: ["Assets/New.asset"],
  }), false);
  assert.equal(diffResultIsProvisionalZero({
    ...diff, exact: true, warnings: ["Assets/Unreadable.asset"],
  }), true);
});

test("exact zero diff and visible metadata changes keep useful counts", () => {
  assert.equal(latestDiffState({
    added: [], modified: [], deleted: [], warnings: [],
  }, true).changeCount, 0);
  assert.equal(latestDiffState({
    added: ["Assets/New.asset"], modified: [], deleted: [], warnings: [],
  }, false).changeCount, 1);
  assert.equal(latestDiffCountText(0, true), "0");
  assert.equal(latestDiffCountText(0, false), "…");
  assert.equal(latestDiffCountText(1, false), "1+");
  assert.equal(detectedDiffCountText(2), "2");
  assert.equal(detectedDiffCountText(2, "+"), "2+");
  assert.equal(detectedDiffCountText(2, "?"), "2?");
});

test("diff warnings make the latest change count unknown", () => {
  assert.deepEqual(latestDiffState({
    added: ["Assets/New.asset"],
    modified: [],
    deleted: [],
    warnings: ["Assets/Unreadable.asset"],
  }, true), {
    warnings: ["Assets/Unreadable.asset"],
    exact: false,
    changeCount: null,
  });
});

test("incomplete diff makes the latest change count unknown without relying on warnings", () => {
  assert.deepEqual(latestDiffState({
    added: [],
    modified: [],
    deleted: [],
    unknown: ["Packages/locked.json"],
    complete: false,
    warnings: [],
  }, true), {
    warnings: [],
    exact: false,
    changeCount: null,
  });
});

test("queued metadata refresh stays metadata-only only when every request is metadata-only", () => {
  const queued = mergeDiffRefreshOptions(
    { silent: true, metadataOnly: true },
    { silent: true, metadataOnly: true, allowBusy: true },
  );

  assert.equal(queued.metadataOnly, true);
  assert.equal(queued.silent, true);
  assert.equal(queued.allowBusy, true);
});

test("foreground work cancels an active background operation and waits for it to finish", async () => {
  let active = true;
  let cancelCount = 0;
  const completed = await cancelAndWaitForIdle({
    isActive: () => active,
    cancel: async () => {
      cancelCount += 1;
      active = false;
    },
    sleep: async () => {},
    timeoutMs: 5000,
    intervalMs: 100,
    now: () => 0,
  });

  assert.equal(completed, true);
  assert.equal(cancelCount, 1);
});

test("foreground work stops waiting when background cancellation does not finish", async () => {
  let now = 0;
  const completed = await cancelAndWaitForIdle({
    isActive: () => true,
    cancel: async () => {},
    sleep: async (duration) => {
      now += duration;
    },
    timeoutMs: 500,
    intervalMs: 100,
    now: () => now,
  });

  assert.equal(completed, false);
  assert.equal(now, 500);
});

test("large change trees flatten without requiring one DOM node per row", () => {
  const changes = [];
  for (let folder = 0; folder < 100; folder += 1) {
    for (let file = 0; file < 100; file += 1) {
      changes.push({
        path: `Assets/Folder${String(folder).padStart(3, "0")}/File${String(file).padStart(3, "0")}.asset`,
        type: "modified",
      });
    }
  }
  const root = buildChangeTreeModel(changes);
  const collapsedRows = flattenChangeTreeRows(root, new Set());
  const openPaths = new Set(collectChangeTreeFolderPaths(root));
  const rows = flattenChangeTreeRows(root, openPaths);
  const range = virtualTreeWindowRange(rows.length, 160000, 640, 32, 8);

  assert.equal(collapsedRows.length, 1);
  assert.equal(rows.length, 10101);
  assert.ok(range.end - range.start <= 36);
  assert.equal(rows[0].kind, "folder");
  assert.equal(rows[0].depth, 0);
  assert.equal(rows[1].depth, 1);
  assert.equal(rows[2].depth, 2);
});

test("virtual window clamps stale scroll positions after collapsing rows", () => {
  const range = virtualTreeWindowRange(3, 100000, 640, 36, 10);

  assert.deepEqual(range, { start: 0, end: 3 });
});

test("change trees start collapsed and expose sibling positions for ARIA", () => {
  const root = buildChangeTreeModel([
    { path: "Assets/A.asset", type: "added" },
    { path: "Packages/B.json", type: "modified" },
  ]);
  const rows = flattenChangeTreeRows(root, new Set());

  assert.equal(rows.length, 2);
  assert.deepEqual(rows.map((row) => row.posInSet), [1, 2]);
  assert.deepEqual(rows.map((row) => row.setSize), [2, 2]);
  assert.ok(rows.every((row) => row.depth === 0));
});

test("GUI usability guards keep dialogs reachable and accessible", () => {
  const appJs = fs.readFileSync(path.join(__dirname, "app.js"), "utf8");
  const dialogsJs = fs.readFileSync(path.join(__dirname, "dialogs.js"), "utf8");
  const indexHtml = fs.readFileSync(path.join(__dirname, "index.html"), "utf8");
  const stylesCss = fs.readFileSync(path.join(__dirname, "styles.css"), "utf8");
  const themeJs = fs.readFileSync(path.join(__dirname, "theme.js"), "utf8");
  const tauriConfig = JSON.parse(fs.readFileSync(
    path.join(__dirname, "..", "src-tauri", "tauri.conf.json"),
    "utf8",
  ));

  assert.match(appJs, /async function reconnectProjectStorageAfterLoadFailure/);
  assert.match(appJs, /storageRootUnavailable" && storageRootPath\) throw error/);
  assert.match(appJs, /statusBannerText/);
  assert.match(appJs, /contextMenuReturnFocus/);
  assert.match(appJs, /visibleModalOverlay\(\) \|\| document\.body/);
  assert.match(stylesCss, /\.confirm-box \{[\s\S]*?max-height: calc\(100vh - 36px\);[\s\S]*?overflow: auto;/);
  assert.match(stylesCss, /#projectStatusPath \{[\s\S]*?text-overflow: ellipsis;/);
  assert.match(indexHtml, /id="checkpointName"[^>]*aria-label="チェックポイント名"/);
  assert.match(indexHtml, /id="themeSystem"[^>]*aria-pressed="true"/);
  assert.match(indexHtml, /class="busy-box"[^>]*tabindex="-1"/);
  assert.match(themeJs, /setAttribute\("aria-pressed", String\(selected\)\)/);
  assert.match(dialogsJs, /\[role='dialog'\]\[tabindex\]/);
  assert.equal(tauriConfig.app.windows[0].minWidth, 480);
  assert.equal(tauriConfig.app.windows[0].minHeight, 560);
});

test("GUI uses user-facing maintenance and storage reconnect wording", () => {
  const appJs = fs.readFileSync(path.join(__dirname, "app.js"), "utf8");
  const indexHtml = fs.readFileSync(path.join(__dirname, "index.html"), "utf8");
  const i18nJs = fs.readFileSync(path.join(__dirname, "i18n.js"), "utf8");
  const agents = fs.readFileSync(path.join(__dirname, "..", "..", "..", "AGENTS.md"), "utf8");

  assert.match(indexHtml, /手動移動済みの保存データへ再接続/);
  assert.match(i18nJs, /gcDryRun: "不要なバックアップデータ"/);
  assert.match(i18nJs, /transactionCleanup: "復旧用データの片付け"/);
  assert.doesNotMatch(appJs, /不要 object|manifest chunk|inventory node|transaction cleanup/);
  assert.match(agents, /更新確認・ダウンロード・適用はMVP対象/);
});
