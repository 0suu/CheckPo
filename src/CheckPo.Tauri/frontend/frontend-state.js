(function exposeFrontendState(root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.CheckPoFrontendState = api;
})(typeof globalThis === "object" ? globalThis : this, () => {
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
      selectedCheckpointId: null,
      renamingCheckpointId: null,
      gcPlan: null,
      tempCleanupPlan: null,
      rollbackPlan: null,
      currentDiff: null,
      currentDiffFilter: "all",
      diffTreeOpenPaths: new Set(),
      diffTreeTouched: false,
      currentDiffSelectedPaths: new Set(),
      lastSelectedChangePath: null,
    };
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

  return {
    projectChanged,
    projectIdentity,
    projectScopedStateReset,
    selectChangePaths,
  };
});
