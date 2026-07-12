const test = require("node:test");
const assert = require("node:assert/strict");
const {
  buildChangeTreeModel,
  cancelAndWaitForIdle,
  collectChangeTreeFolderPaths,
  flattenChangeTreeRows,
  localizedErrorDisplay,
  mergeDiffRefreshOptions,
  projectChanged,
  projectScopedStateReset,
  removeProjectFromHistory,
  restorableLastProjectPath,
  retainPendingTransactionFailures,
  retainVisibleChangeSelection,
  selectChangePaths,
  virtualTreeWindowRange,
} = require("./frontend-state.js");

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
  assert.equal(first.rollbackPlan, null);
  assert.equal(first.gcPlan, null);
  assert.equal(first.tempCleanupPlan, null);
  assert.deepEqual(first.pendingTransactions, []);
  assert.deepEqual(first.unresolvedQuarantines, []);
  assert.deepEqual(first.failedTransactions, []);
  assert.equal(first.currentDiffFilter, "all");
  assert.equal(first.selectedCheckpointId, null);
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
