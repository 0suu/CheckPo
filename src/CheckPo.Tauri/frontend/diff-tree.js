const DIFF_TREE_ROW_HEIGHT = 36;
const DIFF_TREE_OVERSCAN = 10;

const diffVirtualView = {
  container: null,
  spacer: null,
  root: null,
  rows: [],
  rowIndexByKey: new Map(),
  rowHeight: DIFF_TREE_ROW_HEIGHT,
  activeKey: null,
  revision: 0,
  renderFrame: null,
  resizeObserver: null,
};

function renderDiff(diff, checkpointId = state.selectedCheckpointId, options = {}) {
  const previousCheckpointId = state.currentDiff?.checkpointId || null;
  const checkpoint = state.checkpoints.find((item) => item.checkpointId === checkpointId) || null;
  state.currentDiff = { ...diff, checkpointId, checkpoint, exact: Boolean(options.exact) };
  if (checkpointId === latestCheckpointId()) {
    const latest = CheckPoFrontendState.latestDiffState(diff, state.currentDiff.exact);
    state.latestDiffCheckpointId = checkpointId;
    state.latestDiffWarnings = latest.warnings;
    state.latestDiffExact = latest.exact;
    state.latestDiffRefreshFailure = null;
    state.latestDiffChangeCount = latest.changeCount;
  }
  state.diffRefreshFailure = null;
  if (!state.diffTreeTouched) state.diffTreeOpenPaths = new Set();
  renderCurrentDiff(state.currentDiff, {
    resetScroll: previousCheckpointId !== checkpointId,
  });
  renderPendingFileCount();
  updateWorkingCheckpointRow();
  renderWarningBanner();
}

function renderCurrentDiff(diff, options = {}) {
  const addedCount = diff.added?.length ?? 0;
  const editedCount = diff.modified?.length ?? 0;
  const deletedCount = diff.deleted?.length ?? 0;
  const unchangedCount = diff.unchangedCount ?? 0;
  const checkpoint = diff.checkpoint || null;
  const unconfirmedZero = CheckPoFrontendState.diffResultIsProvisionalZero(diff);
  const hasWarnings = Array.isArray(diff.warnings) && diff.warnings.length > 0;
  const detectedOnly = !diff.exact || hasWarnings;
  $("diffSummary").textContent = unconfirmedZero
    ? t(hasWarnings ? "warningFileChangesSummary" : "provisionalFileChangesSummary")
    : detectedOnly
      ? tf(hasWarnings ? "warningDetectedFileChangesSummary" : "detectedFileChangesSummary", {
        total: addedCount + editedCount + deletedCount,
        added: addedCount,
        modified: editedCount,
        deleted: deletedCount,
      })
    : tf("fileChangesSummary", {
      total: addedCount + editedCount + deletedCount,
      added: addedCount,
      modified: editedCount,
      deleted: deletedCount,
      unchanged: unchangedCount,
    });
  $("activeCheckpointTitle").textContent =
    `${checkpoint?.name || checkpoint?.checkpointId || t("latestCheckpoint")} → ${t("workingFolder")}`;
  updateFilterChips(addedCount, editedCount, deletedCount);
  renderChangeGroups([
    [t("added"), diff.added || [], "added"],
    [t("edited"), diff.modified || [], "modified"],
    [t("deleted"), diff.deleted || [], "deleted"],
  ], options);
}

function renderChangeGroups(groups, options = {}) {
  const container = $("diffGroups");
  initializeVirtualTree(container);
  const allChanges = groups.flatMap(([, paths, type]) => paths.map((path) => ({ path, type })));
  const changes = state.currentDiffFilter === "all"
    ? allChanges
    : allChanges.filter((change) => change.type === state.currentDiffFilter);
  const retainedSelection = CheckPoFrontendState.retainVisibleChangeSelection(
    state.currentDiffSelectedPaths,
    state.lastSelectedChangePath,
    changes.map((change) => change.path),
  );
  state.currentDiffSelectedPaths = retainedSelection.selectedPaths;
  state.lastSelectedChangePath = retainedSelection.anchorPath;

  invalidateVirtualTreeRender();
  diffVirtualView.root = null;
  setVirtualTreeRows([]);
  diffVirtualView.activeKey = null;
  container.setAttribute("aria-busy", "true");
  container.replaceChildren();

  if (changes.length === 0) {
    resetTreeAccessibility(container);
    state.currentDiffSelectedPaths.clear();
    state.lastSelectedChangePath = null;
    updateSelectedDiffButton();
    const empty = document.createElement("div");
    empty.className = "diff-empty-state";
    const message = document.createElement("p");
    const unconfirmedZero = CheckPoFrontendState.diffResultIsProvisionalZero(state.currentDiff);
    const hasWarnings = Array.isArray(state.currentDiff?.warnings)
      && state.currentDiff.warnings.length > 0;
    message.textContent = unconfirmedZero
      ? t(hasWarnings ? "warningNoFileChanges" : "provisionalNoFileChanges")
      : t("noFileChanges");
    const button = document.createElement("button");
    button.className = "button primary";
    button.type = "button";
    button.textContent = t("runDiff");
    button.addEventListener("click", runExactDiff);
    empty.append(message, button);
    container.append(empty);
    return;
  }

  diffVirtualView.root = buildChangeTree(changes);
  setVirtualTreeRows(CheckPoFrontendState.flattenChangeTreeRows(
    diffVirtualView.root,
    state.diffTreeOpenPaths,
  ));
  retainLogicalTreeSelection();
  diffVirtualView.activeKey = diffVirtualView.rows[0]?.key || null;
  container.setAttribute("role", "tree");
  container.setAttribute("aria-label", "変更ファイル");
  container.setAttribute("aria-multiselectable", "true");
  container.tabIndex = 0;
  const spacer = document.createElement("div");
  spacer.className = "diff-tree-spacer";
  spacer.setAttribute("role", "presentation");
  diffVirtualView.spacer = spacer;
  container.append(spacer);
  if (options.resetScroll) container.scrollTop = 0;
  updateVirtualTreeHeight();
  renderVirtualTreeWindow(true);
  container.setAttribute("aria-busy", "false");
  updateSelectedDiffButton();
  updateControls();
}

function initializeVirtualTree(container) {
  if (diffVirtualView.container === container) return;
  if (diffVirtualView.resizeObserver) diffVirtualView.resizeObserver.disconnect();
  diffVirtualView.container = container;
  diffVirtualView.rowHeight = Number.parseFloat(
    getComputedStyle(container).getPropertyValue("--diff-tree-row-height"),
  ) || DIFF_TREE_ROW_HEIGHT;
  container.addEventListener("scroll", scheduleVirtualTreeRender, { passive: true });
  container.addEventListener("keydown", handleVirtualTreeKeydown);
  container.addEventListener("focus", () => renderVirtualTreeWindow(true));
  if (typeof ResizeObserver === "function") {
    diffVirtualView.resizeObserver = new ResizeObserver(scheduleVirtualTreeRender);
    diffVirtualView.resizeObserver.observe(container);
  }
}

function resetVirtualDiffTree() {
  invalidateVirtualTreeRender();
  diffVirtualView.root = null;
  setVirtualTreeRows([]);
  diffVirtualView.activeKey = null;
  diffVirtualView.spacer = null;
  const container = $("diffGroups");
  container.replaceChildren();
  resetTreeAccessibility(container);
}

function resetTreeAccessibility(container) {
  container.removeAttribute("role");
  container.removeAttribute("aria-label");
  container.removeAttribute("aria-multiselectable");
  container.removeAttribute("aria-activedescendant");
  container.removeAttribute("aria-busy");
  container.removeAttribute("tabindex");
}

function invalidateVirtualTreeRender() {
  diffVirtualView.revision += 1;
  if (diffVirtualView.renderFrame !== null) {
    cancelAnimationFrame(diffVirtualView.renderFrame);
    diffVirtualView.renderFrame = null;
  }
}

function scheduleVirtualTreeRender() {
  if (diffVirtualView.renderFrame !== null) return;
  const revision = diffVirtualView.revision;
  diffVirtualView.renderFrame = requestAnimationFrame(() => {
    diffVirtualView.renderFrame = null;
    if (revision !== diffVirtualView.revision) return;
    renderVirtualTreeWindow();
  });
}

function updateVirtualTreeHeight() {
  if (!diffVirtualView.spacer) return;
  diffVirtualView.spacer.style.height = `${diffVirtualView.rows.length * diffVirtualView.rowHeight}px`;
}

function renderVirtualTreeWindow(force = false) {
  const { container, spacer, rows, revision } = diffVirtualView;
  if (!container || !spacer || !spacer.isConnected) return;
  const range = CheckPoFrontendState.virtualTreeWindowRange(
    rows.length,
    container.scrollTop,
    container.clientHeight,
    diffVirtualView.rowHeight,
    DIFF_TREE_OVERSCAN,
  );
  if (!force
    && Number(spacer.dataset.start) === range.start
    && Number(spacer.dataset.end) === range.end
    && spacer.dataset.activeKey === (diffVirtualView.activeKey || "")) return;

  spacer.dataset.start = String(range.start);
  spacer.dataset.end = String(range.end);
  spacer.dataset.activeKey = diffVirtualView.activeKey || "";
  const fragment = document.createDocumentFragment();
  const indexes = [];
  for (let index = range.start; index < range.end; index += 1) indexes.push(index);
  const activeIndex = diffVirtualView.rowIndexByKey.get(diffVirtualView.activeKey) ?? -1;
  if (activeIndex >= 0 && (activeIndex < range.start || activeIndex >= range.end)) {
    indexes.push(activeIndex);
    indexes.sort((left, right) => left - right);
  }
  for (const index of indexes) {
    fragment.append(createVirtualTreeRow(rows[index], index, revision));
  }
  spacer.replaceChildren(fragment);
  updateActiveDescendant();
  updateControls();
}

function createVirtualTreeRow(rowData, index, revision) {
  const row = document.createElement("div");
  row.id = `diff-tree-row-${revision}-${index}`;
  row.className = `tree-row virtual-row ${rowData.kind}`;
  row.dataset.rowKey = rowData.key;
  row.style.top = `${index * diffVirtualView.rowHeight}px`;
  row.style.setProperty("--ind", `${rowData.depth * 20}px`);
  row.setAttribute("role", "treeitem");
  row.setAttribute("aria-level", String(rowData.depth + 1));
  row.setAttribute("aria-posinset", String(rowData.posInSet));
  row.setAttribute("aria-setsize", String(rowData.setSize));
  row.classList.toggle("is-active", diffVirtualView.activeKey === rowData.key);
  if (rowData.kind === "folder") populateVirtualFolderRow(row, rowData, revision);
  else populateVirtualFileRow(row, rowData, revision);
  return row;
}

function populateVirtualFolderRow(row, rowData, revision) {
  row.classList.toggle("is-open", rowData.isOpen);
  row.setAttribute("aria-expanded", String(rowData.isOpen));
  row.setAttribute("aria-label", `フォルダー ${rowData.node.path}`);
  const triangle = document.createElement("span");
  triangle.className = "tree-triangle";
  triangle.textContent = "▶";
  const icon = document.createElement("span");
  icon.className = "file-icon";
  icon.textContent = "📁";
  const label = document.createElement("span");
  label.className = "tree-name";
  const chain = document.createElement("span");
  chain.className = "folder-chain";
  chain.textContent = rowData.names.length > 1 ? `${rowData.names.slice(0, -1).join(" / ")} / ` : "";
  const leaf = document.createElement("span");
  leaf.className = "folder-leaf";
  leaf.textContent = rowData.names[rowData.names.length - 1];
  label.append(chain, leaf);
  const aggregate = document.createElement("span");
  aggregate.className = "agg";
  renderAggregateBadges(aggregate, rowData.node.counts);
  const grow = document.createElement("span");
  grow.className = "grow";
  const actionLabel = t("discardFolderChanges");
  const action = createTreeAction(
    actionLabel,
    () => {
      if (revision !== diffVirtualView.revision) return;
      discardPaths(CheckPoFrontendState.collectChangeTreeFilePaths(rowData.node));
    },
    `${actionLabel}: ${rowData.node.path}`,
  );
  action.classList.add("folder-op");
  row.append(triangle, icon, label, aggregate, grow, action);
  row.addEventListener("click", (event) => {
    event.currentTarget.closest(".diff-tree")?.focus({ preventScroll: true });
    activateVirtualRow(rowData.key);
    toggleVirtualFolder(rowData);
  });
}

function populateVirtualFileRow(row, rowData, revision) {
  const change = rowData.change;
  row.classList.toggle("is-deleted", change.type === "deleted");
  row.classList.toggle("is-selected", state.currentDiffSelectedPaths.has(change.path));
  row.dataset.path = change.path;
  row.dataset.type = change.type;
  row.setAttribute("aria-selected", String(state.currentDiffSelectedPaths.has(change.path)));
  row.setAttribute("aria-label", `${change.path}: ${changeFileDescription(change.type)}`);
  const mark = document.createElement("span");
  mark.className = `status-dot ${change.type}`;
  mark.textContent = change.type === "added" ? "＋" : change.type === "deleted" ? "−" : "✎";
  const label = document.createElement("span");
  label.className = "tree-name grow";
  label.textContent = change.name || basename(change.path);
  const meta = document.createElement("span");
  meta.className = `change-meta ${change.type}`;
  meta.textContent = changeFileDescription(change.type);
  const actionLabel = change.type === "deleted" ? t("restoreDeletedFile") : t("discardFileChange");
  const action = createTreeAction(
    actionLabel,
    () => {
      if (revision !== diffVirtualView.revision) return;
      discardPaths([change.path]);
    },
    `${actionLabel}: ${change.path}`,
  );
  row.append(mark, label, meta, action);
  row.addEventListener("click", (event) => {
    event.currentTarget.closest(".diff-tree")?.focus({ preventScroll: true });
    activateVirtualRow(rowData.key);
    selectChangeFile(change.path, event);
    renderVirtualTreeWindow(true);
  });
}

function createTreeAction(label, handler, accessibleLabel = label) {
  const action = document.createElement("button");
  action.className = "tree-op";
  action.type = "button";
  action.tabIndex = -1;
  action.dataset.destructive = "true";
  action.textContent = label;
  action.setAttribute("aria-label", accessibleLabel);
  action.addEventListener("pointerdown", (event) => event.preventDefault());
  action.addEventListener("click", (event) => {
    event.stopPropagation();
    handler();
  });
  return action;
}

function activateVirtualRow(key) {
  diffVirtualView.activeKey = key;
  updateActiveDescendant();
}

function updateActiveDescendant() {
  const { container, activeKey } = diffVirtualView;
  if (!container || !activeKey) {
    container?.removeAttribute("aria-activedescendant");
    return;
  }
  const active = [...container.querySelectorAll(".virtual-row")]
    .find((row) => row.dataset.rowKey === activeKey);
  if (active) container.setAttribute("aria-activedescendant", active.id);
  else container.removeAttribute("aria-activedescendant");
}

function toggleVirtualFolder(rowData, forceOpen) {
  state.diffTreeTouched = true;
  const shouldOpen = forceOpen ?? !state.diffTreeOpenPaths.has(rowData.node.path);
  if (shouldOpen) state.diffTreeOpenPaths.add(rowData.node.path);
  else state.diffTreeOpenPaths.delete(rowData.node.path);
  reflattenVirtualTree(rowData.key);
}

function reflattenVirtualTree(preferredActiveKey) {
  if (!diffVirtualView.root) return;
  invalidateVirtualTreeRender();
  setVirtualTreeRows(CheckPoFrontendState.flattenChangeTreeRows(
    diffVirtualView.root,
    state.diffTreeOpenPaths,
  ));
  retainLogicalTreeSelection();
  const preferredExists = diffVirtualView.rows.some((row) => row.key === preferredActiveKey);
  diffVirtualView.activeKey = preferredExists
    ? preferredActiveKey
    : diffVirtualView.rows[0]?.key || null;
  updateVirtualTreeHeight();
  renderVirtualTreeWindow(true);
  updateSelectedDiffButton();
}

function setVirtualTreeRows(rows) {
  diffVirtualView.rows = rows;
  diffVirtualView.rowIndexByKey = new Map(rows.map((row, index) => [row.key, index]));
}

function retainLogicalTreeSelection() {
  const visibleFilePaths = diffVirtualView.rows
    .filter((row) => row.kind === "file")
    .map((row) => row.change.path);
  const retained = CheckPoFrontendState.retainVisibleChangeSelection(
    state.currentDiffSelectedPaths,
    state.lastSelectedChangePath,
    visibleFilePaths,
  );
  state.currentDiffSelectedPaths = retained.selectedPaths;
  state.lastSelectedChangePath = retained.anchorPath;
}

function selectChangeFile(path, event) {
  const visiblePaths = diffVirtualView.rows
    .filter((row) => row.kind === "file")
    .map((row) => row.change.path);
  const result = CheckPoFrontendState.selectChangePaths({
    selectedPaths: state.currentDiffSelectedPaths,
    anchorPath: state.lastSelectedChangePath,
    targetPath: path,
    visiblePaths,
    shiftKey: event.shiftKey,
    toggleKey: event.metaKey || event.ctrlKey,
  });
  state.currentDiffSelectedPaths = result.selectedPaths;
  state.lastSelectedChangePath = result.anchorPath;
  updateSelectedDiffButton();
}

function handleVirtualTreeKeydown(event) {
  if (event.target !== diffVirtualView.container || diffVirtualView.rows.length === 0) return;
  let index = diffVirtualView.rowIndexByKey.get(diffVirtualView.activeKey) ?? -1;
  if (index < 0) index = 0;
  const row = diffVirtualView.rows[index];
  let nextIndex = index;
  if (event.key === "ArrowDown") nextIndex = Math.min(diffVirtualView.rows.length - 1, index + 1);
  else if (event.key === "ArrowUp") nextIndex = Math.max(0, index - 1);
  else if (event.key === "Home") nextIndex = 0;
  else if (event.key === "End") nextIndex = diffVirtualView.rows.length - 1;
  else if (event.key === "ArrowRight" && row.kind === "folder") {
    if (!row.isOpen) {
      toggleVirtualFolder(row, true);
      event.preventDefault();
      return;
    }
    const childIndex = index + 1 < diffVirtualView.rows.length
      && diffVirtualView.rows[index + 1].parentKey === row.key
      ? index + 1
      : -1;
    if (childIndex >= 0) nextIndex = childIndex;
  } else if (event.key === "ArrowLeft") {
    if (row.kind === "folder" && row.isOpen) {
      toggleVirtualFolder(row, false);
      event.preventDefault();
      return;
    }
    if (row.parentKey) {
      const parentIndex = diffVirtualView.rowIndexByKey.get(row.parentKey) ?? -1;
      if (parentIndex >= 0) nextIndex = parentIndex;
    }
  } else if (event.key === "Enter" || event.key === " ") {
    if (row.kind === "folder") toggleVirtualFolder(row);
    else {
      selectChangeFile(row.change.path, event);
      renderVirtualTreeWindow(true);
    }
    event.preventDefault();
    return;
  } else if (event.key === "Delete") {
    if (diffDestructiveActionBlocked()) return;
    if (row.kind === "folder") {
      discardPaths(CheckPoFrontendState.collectChangeTreeFilePaths(row.node));
    } else {
      discardPaths([row.change.path]);
    }
    event.preventDefault();
    return;
  } else {
    return;
  }
  event.preventDefault();
  setActiveVirtualRow(nextIndex);
}

function setActiveVirtualRow(index) {
  const row = diffVirtualView.rows[index];
  if (!row) return;
  diffVirtualView.activeKey = row.key;
  const top = index * diffVirtualView.rowHeight;
  const bottom = top + diffVirtualView.rowHeight;
  const container = diffVirtualView.container;
  if (top < container.scrollTop) container.scrollTop = top;
  else if (bottom > container.scrollTop + container.clientHeight) {
    container.scrollTop = bottom - container.clientHeight;
  }
  renderVirtualTreeWindow(true);
}

function diffDestructiveActionBlocked() {
  return state.busy
    || state.confirming
    || !state.projectPath
    || state.pendingTransactions.length > 0
    || state.unresolvedQuarantines.length > 0
    || state.projectLocationStatus === "copiedSuspected"
    || !CheckPoFrontendState.diffResultIsComplete(state.currentDiff, state.diffRefreshFailure);
}

function changeFileDescription(type) {
  if (type === "added") return t("addedDescription");
  if (type === "deleted") return t("deletedDescription");
  return t("modifiedDescription");
}

function currentChangeCount() {
  return CheckPoFrontendState.diffChangeCount(state.currentDiff);
}

function latestChangeCount() {
  return Number.isFinite(state.latestDiffChangeCount)
    ? state.latestDiffChangeCount
    : 0;
}

function renderPendingFileCount() {
  const count = state.latestDiffChangeCount;
  $("pendingFileCount").textContent =
    `${CheckPoFrontendState.latestDiffCountText(count, state.latestDiffExact)}${t("fileUnit")}`;
}

function currentFilteredChanges() {
  const diff = state.currentDiff || {};
  const changes = [
    ...(diff.added || []).map((path) => ({ path, type: "added" })),
    ...(diff.modified || []).map((path) => ({ path, type: "modified" })),
    ...(diff.deleted || []).map((path) => ({ path, type: "deleted" })),
  ];
  return state.currentDiffFilter === "all"
    ? changes
    : changes.filter((change) => change.type === state.currentDiffFilter);
}

function updateFilterChips(addedCount, modifiedCount, deletedCount) {
  const hasWarnings = Array.isArray(state.currentDiff?.warnings)
    && state.currentDiff.warnings.length > 0;
  const detectedOnly = Boolean(state.currentDiff) && (!state.currentDiff.exact || hasWarnings);
  const counts = {
    all: addedCount + modifiedCount + deletedCount,
    added: addedCount,
    modified: modifiedCount,
    deleted: deletedCount,
  };
  document.querySelectorAll("[data-diff-filter]").forEach((button) => {
    const filter = button.dataset.diffFilter;
    const prefix = filter === "added" ? "＋" : filter === "modified" ? "✎" : filter === "deleted" ? "−" : t("all");
    const marker = hasWarnings
      ? "?"
      : detectedOnly && (filter === "all" || filter === "modified")
        ? "+"
        : "";
    button.textContent = `${prefix} ${CheckPoFrontendState.detectedDiffCountText(counts[filter] ?? 0, marker)}`;
    button.classList.toggle("is-active", state.currentDiffFilter === filter);
  });
}

function buildChangeTree(changes) {
  return CheckPoFrontendState.buildChangeTreeModel(changes);
}

function collectFolderPaths(root) {
  return CheckPoFrontendState.collectChangeTreeFolderPaths(root);
}

function updateSelectedDiffButton() {
  const button = $("discardSelectedDiffButton");
  if (!button) return;
  const selectedCount = state.currentDiffSelectedPaths.size;
  button.textContent = selectedCount > 0
    ? `${t("discardSelectedChanges")} (${selectedCount})`
    : t("discardSelectedChanges");
  button.disabled = selectedCount === 0
    || state.busy
    || state.confirming
    || state.pendingTransactions.length > 0
    || state.unresolvedQuarantines.length > 0
    || state.projectLocationStatus === "copiedSuspected"
    || !CheckPoFrontendState.diffResultIsComplete(state.currentDiff, state.diffRefreshFailure);
}

function renderAggregateBadges(container, counts) {
  container.replaceChildren();
  const badges = [
    ["added", "+", counts.added],
    ["modified", "✎", counts.modified],
    ["deleted", "−", counts.deleted],
  ];
  for (const [type, mark, count] of badges) {
    if (!count) continue;
    const badge = document.createElement("span");
    badge.className = type;
    badge.textContent = `${mark}${count}`;
    container.append(badge);
  }
}
