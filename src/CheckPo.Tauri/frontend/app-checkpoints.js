function createWorkingCheckpointSection(changeCount) {
  const section = document.createElement("section");
  section.className = "checkpoint-section working-section";
  section.setAttribute("role", "group");
  const heading = document.createElement("div");
  heading.className = "checkpoint-section-label";
  heading.textContent = t("workingFolder");
  const working = document.createElement("button");
  working.type = "button";
  working.className = "checkpoint-row working-row";
  working.setAttribute("role", "option");
  working.setAttribute("aria-selected", "false");
  working.innerHTML = `
    <span class="checkpoint-id mono">now</span>
    <strong class="checkpoint-title">未保存の変更</strong>
    <span class="checkpoint-meta"></span>
  `;
  working.querySelector(".checkpoint-meta").textContent =
    `${CheckPoFrontendState.latestDiffCountText(changeCount, state.latestDiffExact)}${t("fileUnit")}`;
  working.addEventListener("click", async () => {
    await run("再読み込み中", async () => {
      await refreshProject();
      await refreshLatestDiff({ allowBusy: true });
    });
  });
  section.append(heading, working);
  return section;
}

function updateWorkingCheckpointRow() {
  const list = $("checkpointList");
  if (!list) return;
  const changeCount = latestChangeCount();
  let section = list.querySelector(".working-section");
  if (changeCount <= 0) {
    if (section) {
      section.remove();
      scheduleCheckpointVirtualWindowRender();
    }
    return;
  }
  if (!section) {
    section = createWorkingCheckpointSection(changeCount);
    list.prepend(section);
    scheduleCheckpointVirtualWindowRender();
  } else {
    section.querySelector(".checkpoint-meta").textContent =
      `${CheckPoFrontendState.latestDiffCountText(changeCount, state.latestDiffExact)}${t("fileUnit")}`;
  }
  section.hidden = Boolean($("checkpointSearch").value.trim());
}

function renderCheckpointVirtualWindow() {
  const list = $("checkpointList");
  const section = list?.querySelector(".saved-section");
  const spacer = section?.querySelector(":scope > .checkpoint-virtual-spacer");
  if (!list || !spacer) return;
  const sectionScrollTop = Math.max(0, list.scrollTop - spacer.offsetTop);
  const range = CheckPoFrontendState.virtualTreeWindowRange(
    filteredCheckpoints.length,
    sectionScrollTop,
    list.clientHeight,
    CHECKPOINT_ROW_HEIGHT_PX,
    CHECKPOINT_VIRTUAL_OVERSCAN,
  );
  const fragment = document.createDocumentFragment();
  for (let index = range.start; index < range.end; index += 1) {
    const row = checkpointRow(filteredCheckpoints[index]);
    row.classList.add("checkpoint-virtual-row");
    row.style.transform = `translateY(${index * CHECKPOINT_ROW_HEIGHT_PX}px)`;
    row.setAttribute("aria-posinset", String(index + 1));
    row.setAttribute("aria-setsize", String(filteredCheckpoints.length));
    fragment.append(row);
  }
  spacer.style.height = `${filteredCheckpoints.length * CHECKPOINT_ROW_HEIGHT_PX}px`;
  spacer.replaceChildren(fragment);
  updateCheckpointListActiveDescendant();
}

function scheduleCheckpointVirtualWindowRender() {
  if (checkpointScrollFrame !== null) return;
  checkpointScrollFrame = requestAnimationFrame(() => {
    checkpointScrollFrame = null;
    renderCheckpointVirtualWindow();
  });
}

function applyCheckpointSearchFilter(options = {}) {
  if (checkpointSearchTimer !== null) clearTimeout(checkpointSearchTimer);
  checkpointSearchTimer = null;
  const query = $("checkpointSearch").value.trim().toLowerCase();
  const list = $("checkpointList");
  const section = list.querySelector(".saved-section");
  if (!section) return;
  const empty = section.querySelector(":scope > .checkpoint-search-empty");
  filteredCheckpoints = CheckPoFrontendState.filterCheckpoints(state.checkpoints, query);
  if (options.resetScroll !== false) list.scrollTop = 0;
  const working = list.querySelector(".working-section");
  if (working) working.hidden = Boolean(query);
  if (empty) {
    empty.textContent = query && state.checkpoints.length > 0
      ? t("checkpointSearchNoMatches")
      : t("checkpointListEmpty");
    empty.hidden = filteredCheckpoints.length > 0;
  }
  renderCheckpointVirtualWindow();
}

function scheduleCheckpointSearchFilter() {
  if (checkpointSearchTimer !== null) clearTimeout(checkpointSearchTimer);
  checkpointSearchTimer = setTimeout(applyCheckpointSearchFilter, CHECKPOINT_SEARCH_DEBOUNCE_MS);
}

function updateCheckpointSelectionInDom() {
  $("checkpointList")?.querySelectorAll(".checkpoint-row[data-checkpoint-id]").forEach((row) => {
    const selected = row.dataset.checkpointId === state.selectedCheckpointId;
    row.classList.toggle("is-selected", selected);
    row.setAttribute("aria-selected", String(selected));
  });
  updateCheckpointListActiveDescendant();
}

function checkpointOptionId(checkpointId) {
  return `checkpoint-option-${checkpointId}`;
}

function updateCheckpointListActiveDescendant() {
  const list = $("checkpointList");
  if (!list) return;
  const selected = list.querySelector(
    `.checkpoint-row[data-checkpoint-id="${CSS.escape(state.selectedCheckpointId || "")}"]`,
  );
  if (selected) list.setAttribute("aria-activedescendant", selected.id);
  else list.removeAttribute("aria-activedescendant");
}

function checkpointById(checkpointId) {
  return state.checkpoints.find((checkpoint) => checkpoint.checkpointId === checkpointId) || null;
}

function checkpointSearchText(checkpoint) {
  return CheckPoFrontendState.checkpointSearchText(checkpoint);
}

function checkpointRow(checkpoint) {
  const existing = checkpointRowCache.get(checkpoint.checkpointId);
  const shouldRename = checkpoint.checkpointId === state.renamingCheckpointId;
  const isRenaming = existing?.classList.contains("is-renaming") || false;
  if (!existing || shouldRename !== isRenaming) {
    const created = createCheckpointRow(checkpoint);
    checkpointRowCache.set(checkpoint.checkpointId, created);
    return created;
  }
  const selected = checkpoint.checkpointId === state.selectedCheckpointId;
  existing.hidden = false;
  existing.classList.toggle("is-selected", selected);
  existing.setAttribute("aria-selected", String(selected));
  existing.dataset.searchText = checkpointSearchText(checkpoint);
  if (!isRenaming) {
    existing.querySelector(".checkpoint-id").textContent =
      String(checkpoint.checkpointId || "").slice(0, 4) || "----";
    existing.querySelector(".checkpoint-title").textContent = checkpoint.name || checkpoint.checkpointId;
    existing.querySelector(".checkpoint-meta").textContent =
      `${formatCompactDate(checkpoint.createdAtUtc)} · ${checkpoint.fileCount ?? 0}${t("fileUnit")}`;
  }
  return existing;
}

function createCheckpointRow(checkpoint) {
  const isRenaming = checkpoint.checkpointId === state.renamingCheckpointId;
  const row = document.createElement(isRenaming ? "div" : "button");
  if (!isRenaming) row.type = "button";
  row.className = `checkpoint-row${checkpoint.checkpointId === state.selectedCheckpointId ? " is-selected" : ""}${isRenaming ? " is-renaming" : ""}`;
  row.id = checkpointOptionId(checkpoint.checkpointId);
  row.dataset.checkpointId = checkpoint.checkpointId;
  row.dataset.searchText = checkpointSearchText(checkpoint);
  row.setAttribute("role", "option");
  row.setAttribute("aria-selected", String(checkpoint.checkpointId === state.selectedCheckpointId));
  if (isRenaming) row.tabIndex = 0;
  row.innerHTML = `
    <span class="checkpoint-id mono"></span>
    <strong class="checkpoint-title"></strong>
    <span class="checkpoint-meta"></span>
  `;
  row.querySelector(".checkpoint-id").textContent = String(checkpoint.checkpointId || "").slice(0, 4) || "----";
  row.querySelector(".checkpoint-meta").textContent =
    `${formatCompactDate(checkpoint.createdAtUtc)} · ${checkpoint.fileCount ?? 0}${t("fileUnit")}`;
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
  }
  return row;
}

function patchCheckpointRow(checkpointId) {
  const checkpoint = checkpointById(checkpointId);
  const existing = $("checkpointList")?.querySelector(
    `.checkpoint-row[data-checkpoint-id="${CSS.escape(checkpointId)}"]`,
  );
  const query = $("checkpointSearch").value.trim().toLowerCase();
  const matchesQuery = Boolean(checkpoint)
    && (!query || checkpointSearchText(checkpoint).includes(query));
  const patchMode = CheckPoFrontendState.checkpointRowPatchMode(
    Boolean(checkpoint),
    Boolean(existing),
    matchesQuery,
  );
  if (patchMode === "rerender") {
    renderCheckpoints();
    return;
  }
  checkpointRowCache.delete(checkpointId);
  const replacement = checkpointRow(checkpoint);
  if (patchMode === "replace") {
    filteredCheckpoints = filteredCheckpoints.map((item) => (
      item.checkpointId === checkpointId ? checkpoint : item
    ));
    replacement.classList.add("checkpoint-virtual-row");
    replacement.style.transform = existing.style.transform;
    replacement.setAttribute("aria-posinset", existing.getAttribute("aria-posinset") || "1");
    replacement.setAttribute("aria-setsize", existing.getAttribute("aria-setsize") || String(filteredCheckpoints.length));
    existing.replaceWith(replacement);
    return;
  }
  applyCheckpointSearchFilter({ resetScroll: false });
}

async function handleCheckpointListClick(event) {
  if (event.target.closest(".checkpoint-rename-input")) return;
  const row = event.target.closest(".checkpoint-row[data-checkpoint-id]");
  if (!row || !$("checkpointList").contains(row) || row.classList.contains("is-renaming")) return;
  const checkpointId = row.dataset.checkpointId;
  if (!checkpointById(checkpointId)) return;
  selectCheckpoint(checkpointId);
  await refreshLatestDiff({ metadataOnly: true });
}

function scrollCheckpointIndexIntoView(index) {
  const list = $("checkpointList");
  const spacer = list?.querySelector(".checkpoint-virtual-spacer");
  if (!list || !spacer || index < 0) return;
  const rowTop = spacer.offsetTop + (index * CHECKPOINT_ROW_HEIGHT_PX);
  const rowBottom = rowTop + CHECKPOINT_ROW_HEIGHT_PX;
  if (rowTop < list.scrollTop) list.scrollTop = rowTop;
  else if (rowBottom > list.scrollTop + list.clientHeight) {
    list.scrollTop = Math.max(0, rowBottom - list.clientHeight);
  }
  renderCheckpointVirtualWindow();
}

async function handleCheckpointListKeyDown(event) {
  if (event.target.closest(".checkpoint-rename-input")) return;
  const navigationKeys = new Set(["ArrowDown", "ArrowUp", "Home", "End", "PageDown", "PageUp"]);
  if (navigationKeys.has(event.key)) {
    event.preventDefault();
    const currentIndex = filteredCheckpoints.findIndex(
      (checkpoint) => checkpoint.checkpointId === state.selectedCheckpointId,
    );
    const pageSize = Math.max(1, Math.floor($("checkpointList").clientHeight / CHECKPOINT_ROW_HEIGHT_PX));
    const index = CheckPoFrontendState.checkpointNavigationIndex(
      filteredCheckpoints.length,
      currentIndex,
      event.key,
      pageSize,
    );
    const checkpoint = filteredCheckpoints[index];
    if (!checkpoint) return;
    selectCheckpoint(checkpoint.checkpointId, { render: false });
    scrollCheckpointIndexIntoView(index);
    updateCheckpointSelectionInDom();
    await refreshLatestDiff({ metadataOnly: true });
    return;
  }
  if (event.key === "F2" && state.selectedCheckpointId) {
    event.preventDefault();
    const index = filteredCheckpoints.findIndex(
      (checkpoint) => checkpoint.checkpointId === state.selectedCheckpointId,
    );
    if (index >= 0) scrollCheckpointIndexIntoView(index);
    beginRenameCheckpoint(state.selectedCheckpointId);
    return;
  }
  if ((event.key === "ContextMenu" || (event.shiftKey && event.key === "F10"))
    && state.selectedCheckpointId) {
    event.preventDefault();
    const checkpoint = checkpointById(state.selectedCheckpointId);
    const row = document.getElementById(checkpointOptionId(state.selectedCheckpointId));
    if (!checkpoint || !row) return;
    const rect = row.getBoundingClientRect();
    showCheckpointContextMenu(rect.left + 16, rect.bottom, checkpoint);
  }
}

function handleCheckpointListContextMenu(event) {
  if (event.target.closest(".checkpoint-rename-input")) return;
  const row = event.target.closest(".checkpoint-row[data-checkpoint-id]");
  if (!row || !$("checkpointList").contains(row)) return;
  const checkpoint = checkpointById(row.dataset.checkpointId);
  if (!checkpoint) return;
  event.preventDefault();
  selectCheckpoint(checkpoint.checkpointId, { render: !row.classList.contains("is-renaming") });
  showCheckpointContextMenu(event.clientX, event.clientY, checkpoint);
}

function renderCheckpoints() {
  const list = $("checkpointList");
  list.replaceChildren();
  const currentCheckpointIds = new Set(state.checkpoints.map((checkpoint) => checkpoint.checkpointId));
  for (const checkpointId of checkpointRowCache.keys()) {
    if (!currentCheckpointIds.has(checkpointId)) checkpointRowCache.delete(checkpointId);
  }
  const index = CheckPoFrontendState.checkpointIndexPresentation(state.checkpointIndex);
  if (!index.available) {
    const unavailable = document.createElement("p");
    unavailable.className = "empty-list checkpoint-index-empty";
    unavailable.textContent = "一覧は索引の再構築後に表示されます。";
    list.append(unavailable);
    return;
  }
  const changeCount = latestChangeCount();
  if (changeCount > 0) list.append(createWorkingCheckpointSection(changeCount));
  const checkpoints = state.checkpoints;
  const checkpointSection = document.createElement("section");
  checkpointSection.className = "checkpoint-section saved-section";
  checkpointSection.setAttribute("role", "group");
  if (changeCount > 0) {
    const heading = document.createElement("div");
    heading.className = "checkpoint-section-label";
    heading.textContent = "チェックポイント";
    checkpointSection.append(heading);
  }
  const empty = document.createElement("p");
  empty.className = "empty-list checkpoint-search-empty";
  checkpointSection.append(empty);
  const spacer = document.createElement("div");
  spacer.className = "checkpoint-virtual-spacer";
  checkpointSection.append(spacer);
  list.append(checkpointSection);
  applyCheckpointSearchFilter();
}

function selectCheckpoint(checkpointId, options = {}) {
  const changed = state.selectedCheckpointId !== checkpointId;
  state.selectedCheckpointId = checkpointId;
  state.rollbackPlan = null;
  state.rollbackPlanContext = null;
  if (changed) state.rollbackRequestSerial += 1;
  if (changed) clearCurrentDiff();
  if (options.render !== false) updateCheckpointSelectionInDom();
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
  const previouslyRenaming = state.renamingCheckpointId;
  state.renamingCheckpointId = checkpointId;
  if (previouslyRenaming && previouslyRenaming !== checkpointId) {
    patchCheckpointRow(previouslyRenaming);
  }
  patchCheckpointRow(checkpointId);
}

function setupCheckpointRenameInput(input, checkpoint) {
  let committing = false;
  const previousName = String(checkpoint.name || checkpoint.checkpointId || "").trim();
  const cancel = () => {
    if (committing) return;
    state.renamingCheckpointId = null;
    patchCheckpointRow(checkpoint.checkpointId);
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
      patchCheckpointRow(updatedId);
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

function checkpointHasKnownExactNoChanges(checkpointId) {
  return state.currentDiff?.checkpointId === checkpointId
    && state.currentDiff?.exact
    && CheckPoFrontendState.diffResultIsComplete(state.currentDiff, state.diffRefreshFailure)
    && currentChangeCount() === 0;
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
      disabled: locationBlocked
        || pendingBlocked
        || CheckPoFrontendState.restorePreviewIsRedundant(
          checkpointHasKnownExactNoChanges(checkpoint.checkpointId),
          state.unresolvedQuarantines.length,
        ),
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

let contextMenuReturnFocus = null;

function visibleModalOverlay() {
  return [
    "errorOverlay",
    "confirmOverlay",
    "rollbackOverlay",
    "recoveryConflictOverlay",
    "projectRegistrationOverlay",
    "projectSelectionOverlay",
    "advancedOverlay",
    "settingsOverlay",
  ].map((id) => $(id)).find((overlay) => overlay && !overlay.hidden) || null;
}

function showContextMenu(x, y, items) {
  const menu = $("contextMenu");
  if (!menu) return;
  contextMenuReturnFocus = document.activeElement;
  (visibleModalOverlay() || document.body).append(menu);
  menu.inert = false;
  menu.removeAttribute("aria-hidden");
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
      hideContextMenu({ restoreFocus: true });
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

function hideContextMenu(options = {}) {
  const menu = $("contextMenu");
  if (menu) {
    menu.hidden = true;
    if (menu.parentElement !== document.body) document.body.append(menu);
  }
  const returnFocus = contextMenuReturnFocus;
  contextMenuReturnFocus = null;
  if (options.restoreFocus && returnFocus?.isConnected && !returnFocus.closest("[hidden]")) {
    returnFocus.focus({ preventScroll: true });
  }
}

function handleContextMenuKeyDown(event) {
  const menu = $("contextMenu");
  if (!menu || menu.hidden) return;
  const items = Array.from(menu.querySelectorAll("button:not(:disabled)"));
  if (event.key === "Escape") {
    event.preventDefault();
    event.stopImmediatePropagation();
    hideContextMenu({ restoreFocus: true });
    return;
  }
  if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key) || items.length === 0) return;
  event.preventDefault();
  const currentIndex = Math.max(0, items.indexOf(document.activeElement));
  const nextIndex = event.key === "Home"
    ? 0
    : event.key === "End"
      ? items.length - 1
      : event.key === "ArrowDown"
        ? (currentIndex + 1) % items.length
        : (currentIndex - 1 + items.length) % items.length;
  items[nextIndex].focus();
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
  $("rollbackOverlay").hidden = true;
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
    invalidateStoredSize();
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
    button.setAttribute("role", "option");
    button.setAttribute("aria-selected", String(project.path === state.projectPath));
    button.innerHTML = "<strong></strong><span></span>";
    button.querySelector("strong").textContent = project.name || basename(project.path);
    const pathLabel = button.querySelector("span");
    pathLabel.textContent = project.path;
    pathLabel.title = project.path;
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
  clearDialogStatus("projectRegistrationStatus");
  $("projectRegistrationOverlay").hidden = false;
  updateControls();
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
      setOperationWarnings(snapshot.initialCheckpoint.warnings);
      setStatus(`初回チェックポイントを作成しましたが、警告があります。${warningText}`);
      setResult({
        warning: "初回チェックポイントを作成しましたが、警告があります。",
        details: snapshot.initialCheckpoint.warnings,
      });
    } else {
      setOperationWarnings([]);
      setStatus("初回チェックポイントを作成しました。");
    }
  } else if (snapshot.initialCheckpointError) {
    const message = `プロジェクトは開始しましたが、初回チェックポイント作成に失敗しました: ${errorText(snapshot.initialCheckpointError)}`;
    setOperationWarnings([message]);
    setStatus(message);
  } else if (snapshot.initialCheckpointCancelled) {
    setOperationWarnings([]);
    setStatus("初回チェックポイントの作成を中止しました。");
  } else if (successMessage) {
    setOperationWarnings([]);
    setStatus(successMessage);
  }
}
