const test = require("node:test");
const assert = require("node:assert/strict");
const {
  localizedErrorDisplay,
  projectChanged,
  projectScopedStateReset,
  retainPendingTransactionFailures,
  selectChangePaths,
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
