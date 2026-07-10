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
    operationBusy: "別の処理が実行中です。完了してから、もう一度実行してください。",
    pendingTransaction: "中断された作業があります。先に復旧してください。",
    unresolvedTransactionQuarantine: "プロジェクトの状態を安全と確認できません。既知のチェックポイントへ全体復元してください。",
    transactionRecoveryFailed: "自動復旧できない作業があります。内容を安全な場所へ退避できます。",
    indexUnavailable: "一覧用データを読み込めませんでした。インデックスの再構築を実行してください。",
    unsupportedFormat: "この保存データは、より新しいCheckPoで作られています。データを変更せず、CheckPoを更新してください。",
    copiedProjectSuspected: "同じプロジェクトのコピーが見つかりました。使用する場所を確認してください。",
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
      selectedCheckpointId: null,
      renamingCheckpointId: null,
      gcPlan: null,
      tempCleanupPlan: null,
      rollbackPlan: null,
      pendingTransactions: [],
      unresolvedQuarantines: [],
      failedTransactions: [],
      currentDiff: null,
      currentDiffFilter: "all",
      diffTreeOpenPaths: new Set(),
      diffTreeTouched: false,
      currentDiffSelectedPaths: new Set(),
      lastSelectedChangePath: null,
    };
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

  return {
    projectChanged,
    projectIdentity,
    projectScopedStateReset,
    localizedErrorDisplay,
    retainPendingTransactionFailures,
    selectChangePaths,
  };
});
