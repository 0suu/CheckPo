function renderDiff(diff) {
  state.currentDiff = { ...diff, checkpoint: state.checkpoints[0] || null };
  if (!state.diffTreeTouched) {
    state.diffTreeOpenPaths = new Set();
  }
  renderCurrentDiff(state.currentDiff);
  renderCheckpoints();
}

function renderCurrentDiff(diff) {
  const addedCount = diff.added?.length ?? 0;
  const editedCount = diff.modified?.length ?? 0;
  const deletedCount = diff.deleted?.length ?? 0;
  const unchangedCount = diff.unchangedCount ?? 0;
  const checkpoint = diff.checkpoint || state.checkpoints[0] || null;
  $("diffSummary").textContent = tf("fileChangesSummary", {
    total: addedCount + editedCount + deletedCount,
    added: addedCount,
    modified: editedCount,
    deleted: deletedCount,
    unchanged: unchangedCount,
  });
  $("activeCheckpointTitle").textContent =
    `${checkpoint?.name || checkpoint?.checkpointId || t("latestCheckpoint")} → ${t("workingFolder")}`;
  $("pendingFileCount").textContent = `${addedCount + editedCount + deletedCount}${t("fileUnit")}`;
  updateFilterChips(addedCount, editedCount, deletedCount);
  renderChangeGroups([
    [t("added"), diff.added || [], "added"],
    [t("edited"), diff.modified || [], "modified"],
    [t("deleted"), diff.deleted || [], "deleted"],
  ]);
}

function renderChangeGroups(groups) {
  const container = $("diffGroups");
  container.replaceChildren();
  const allChanges = groups.flatMap(([, paths, type]) => paths.map((path) => ({ path, type })));
  const allChangePathSet = new Set(allChanges.map((change) => change.path));
  const changes = state.currentDiffFilter === "all"
    ? allChanges
    : allChanges.filter((change) => change.type === state.currentDiffFilter);
  state.currentDiffSelectedPaths = new Set(
    [...state.currentDiffSelectedPaths].filter((path) => allChangePathSet.has(path)),
  );
  updateSelectedDiffButton();
  if (changes.length === 0) {
    state.currentDiffSelectedPaths.clear();
    state.lastSelectedChangePath = null;
    updateSelectedDiffButton();
    const empty = document.createElement("div");
    empty.className = "diff-empty-state";
    const message = document.createElement("p");
    message.textContent = t("noFileChanges");
    const button = document.createElement("button");
    button.className = "button primary";
    button.type = "button";
    button.textContent = t("runDiff");
    button.addEventListener("click", runExactDiff);
    empty.append(message, button);
    container.append(empty);
    return;
  }

  const root = buildChangeTree(changes);
  if (!state.diffTreeTouched && state.diffTreeOpenPaths.size === 0) {
    collectFolderPaths(root).forEach((path) => state.diffTreeOpenPaths.add(path));
  }
  const renderContext = { groups };
  for (const folder of [...root.children.values()].sort(compareTreeNode)) {
    renderFolderNode(container, folder, 0, renderContext);
  }
  for (const file of root.files.sort(compareTreeFile)) {
    container.append(changeFileRow(file, renderContext, 0));
  }
  updateControls();
}

function changeFileRow(change, renderContext, depth = 0) {
  const row = document.createElement("div");
  row.className = `tree-row file ${change.type === "deleted" ? "is-deleted" : ""}`;
  row.dataset.path = change.path;
  row.dataset.type = change.type;
  row.style.setProperty("--ind", `${depth * 20}px`);
  row.tabIndex = 0;
  const mark = document.createElement("span");
  mark.className = `status-dot ${change.type}`;
  mark.textContent = change.type === "added" ? "＋" : change.type === "deleted" ? "−" : "✎";
  const label = document.createElement("span");
  label.className = "tree-name grow";
  label.textContent = basename(change.path);
  const meta = document.createElement("span");
  meta.className = `change-meta ${change.type}`;
  meta.textContent = changeFileDescription(change.type);
  const action = document.createElement("button");
  action.className = "tree-op";
  action.type = "button";
  action.dataset.destructive = "true";
  action.textContent = change.type === "deleted" ? t("restoreDeletedFile") : t("discardFileChange");
  action.addEventListener("click", (event) => {
    event.stopPropagation();
    discardPaths([change.path]);
  });
  row.append(mark, label, meta, action);
  const select = (event) => {
    selectChangeFile(change.path, event);
    renderChangeGroups(renderContext.groups);
  };
  row.addEventListener("click", select);
  row.addEventListener("keydown", (event) => {
    if (event.key === "Enter") select(event);
  });
  row.classList.toggle("is-selected", state.currentDiffSelectedPaths.has(change.path));
  return row;
}

function selectChangeFile(path, event) {
  const visiblePaths = [...$("diffGroups").querySelectorAll(".tree-row.file")]
    .map((row) => row.dataset.path);
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

function changeFileDescription(type) {
  if (type === "added") return t("addedDescription");
  if (type === "deleted") return t("deletedDescription");
  return t("modifiedDescription");
}

function currentChangeCount() {
  const diff = state.currentDiff;
  return (diff?.added?.length ?? 0) + (diff?.modified?.length ?? 0) + (diff?.deleted?.length ?? 0);
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
  const counts = {
    all: addedCount + modifiedCount + deletedCount,
    added: addedCount,
    modified: modifiedCount,
    deleted: deletedCount,
  };
  document.querySelectorAll("[data-diff-filter]").forEach((button) => {
    const filter = button.dataset.diffFilter;
    const prefix = filter === "added" ? "＋" : filter === "modified" ? "✎" : filter === "deleted" ? "−" : t("all");
    button.textContent = `${prefix} ${counts[filter] ?? 0}`;
    button.classList.toggle("is-active", state.currentDiffFilter === filter);
  });
}

function buildChangeTree(changes) {
  const root = createTreeNode("", "", null);
  for (const change of changes) {
    const parts = String(change.path).split(/[\\/]/).filter(Boolean);
    const fileName = parts.pop() || change.path;
    let node = root;
    for (const part of parts) {
      const path = node.path ? `${node.path}/${part}` : part;
      if (!node.children.has(part)) node.children.set(part, createTreeNode(part, path, node));
      node = node.children.get(part);
      node.counts[change.type] += 1;
      node.paths.push(change.path);
    }
    node.files.push({ ...change, name: fileName });
  }
  return root;
}

function createTreeNode(name, path, parent) {
  return {
    name,
    path,
    parent,
    children: new Map(),
    files: [],
    counts: { added: 0, modified: 0, deleted: 0 },
    paths: [],
  };
}

function collectFolderPaths(node, paths = []) {
  for (const child of node.children.values()) {
    const compressed = compressedFolder(child);
    paths.push(compressed.node.path);
    collectFolderPaths(compressed.node, paths);
  }
  return paths;
}

function compressedFolder(node) {
  const names = [node.name];
  let current = node;
  while (current.files.length === 0 && current.children.size === 1) {
    const next = [...current.children.values()][0];
    names.push(next.name);
    current = next;
  }
  return { node: current, names };
}

function renderFolderNode(container, sourceNode, depth, renderContext) {
  const { node, names } = compressedFolder(sourceNode);
  const row = document.createElement("div");
  const isOpen = state.diffTreeOpenPaths.has(node.path);
  row.className = `tree-row folder ${isOpen ? "is-open" : ""}`;
  row.style.setProperty("--ind", `${depth * 20}px`);
  row.dataset.folderPath = node.path;
  const chain = names.length > 1 ? `${names.slice(0, -1).join(" / ")} / ` : "";
  row.innerHTML = `
    <span class="tree-triangle">▶</span>
    <span class="file-icon">📁</span>
    <span class="tree-name"><span class="folder-chain"></span><span class="folder-leaf"></span></span>
    <span class="agg"></span>
    <span class="grow"></span>
  `;
  row.querySelector(".folder-chain").textContent = chain;
  row.querySelector(".folder-leaf").textContent = names[names.length - 1];
  renderAggregateBadges(row.querySelector(".agg"), node.counts);
  const action = document.createElement("button");
  action.className = "tree-op folder-op";
  action.type = "button";
  action.dataset.destructive = "true";
  action.textContent = t("discardFolderChanges");
  action.addEventListener("click", (event) => {
    event.stopPropagation();
    discardPaths([...new Set(node.paths)]);
  });
  row.append(action);
  row.addEventListener("click", () => {
    state.diffTreeTouched = true;
    if (state.diffTreeOpenPaths.has(node.path)) state.diffTreeOpenPaths.delete(node.path);
    else state.diffTreeOpenPaths.add(node.path);
    renderChangeGroups(renderContext.groups);
  });
  container.append(row);

  if (!isOpen) return;
  const children = document.createElement("div");
  children.className = "tree-children";
  children.style.setProperty("--ind", `${depth * 20 + 14}px`);
  for (const child of [...node.children.values()].sort(compareTreeNode)) {
    renderFolderNode(children, child, depth + 1, renderContext);
  }
  for (const file of node.files.sort(compareTreeFile)) {
    children.append(changeFileRow(
      file,
      renderContext,
      depth + 1,
    ));
  }
  container.append(children);
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
    || state.projectLocationStatus === "copiedSuspected";
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

function compareTreeNode(a, b) {
  return a.name.localeCompare(b.name, "ja");
}

function compareTreeFile(a, b) {
  if (a.type !== b.type) return changeTypeOrder(a.type) - changeTypeOrder(b.type);
  return a.name.localeCompare(b.name, "ja");
}

function changeTypeOrder(type) {
  if (type === "added") return 0;
  if (type === "modified") return 1;
  return 2;
}
