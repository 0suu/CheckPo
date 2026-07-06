function confirmAction(message, okLabel = "戻す", options = {}) {
  return new Promise((resolve) => {
    const overlay = $("confirmOverlay");
    const okButton = $("confirmOkButton");
    const cancelButton = $("confirmCancelButton");
    const initialCheckpointChoice = $("confirmInitialCheckpointChoice");
    const includeInitialCheckpointChoice = Boolean(options.initialCheckpointChoice);
    $("confirmMessage").textContent = message;
    okButton.textContent = okLabel;
    initialCheckpointChoice.hidden = !includeInitialCheckpointChoice;
    if (includeInitialCheckpointChoice) {
      resetInitialCheckpointChoice("confirmInitialCheckpoint");
    }
    overlay.hidden = false;

    const finish = (value) => {
      overlay.hidden = true;
      initialCheckpointChoice.hidden = true;
      okButton.removeEventListener("click", onOk);
      cancelButton.removeEventListener("click", onCancel);
      overlay.removeEventListener("click", onOverlayClick);
      document.removeEventListener("keydown", onKeyDown);
      resolve(value);
    };
    const result = (confirmed) => includeInitialCheckpointChoice
      ? { confirmed, createInitialCheckpoint: confirmed && wantsInitialCheckpoint("confirmInitialCheckpoint") }
      : confirmed;
    const onOk = () => finish(result(true));
    const onCancel = () => finish(result(false));
    const onOverlayClick = (event) => {
      if (event.target === overlay) finish(result(false));
    };
    const onKeyDown = (event) => {
      if (event.key === "Escape") finish(result(false));
    };

    okButton.addEventListener("click", onOk);
    cancelButton.addEventListener("click", onCancel);
    overlay.addEventListener("click", onOverlayClick);
    document.addEventListener("keydown", onKeyDown);
    cancelButton.focus();
  });
}

function chooseCopiedProjectAction(message) {
  return new Promise((resolve) => {
    const overlay = $("confirmOverlay");
    const okButton = $("confirmOkButton");
    const cancelButton = $("confirmCancelButton");
    const initialCheckpointChoice = $("confirmInitialCheckpointChoice");
    $("confirmMessage").textContent = message;
    cancelButton.textContent = "この場所を使う";
    okButton.textContent = "別プロジェクトとして開始";
    initialCheckpointChoice.hidden = false;
    resetInitialCheckpointChoice("confirmInitialCheckpoint");
    overlay.hidden = false;

    const finish = (value) => {
      overlay.hidden = true;
      initialCheckpointChoice.hidden = true;
      cancelButton.textContent = "キャンセル";
      okButton.removeEventListener("click", onStartSeparate);
      cancelButton.removeEventListener("click", onUseLocation);
      overlay.removeEventListener("click", onOverlayClick);
      document.removeEventListener("keydown", onKeyDown);
      resolve(value);
    };
    const onStartSeparate = () => finish({
      action: "startSeparate",
      createInitialCheckpoint: wantsInitialCheckpoint("confirmInitialCheckpoint"),
    });
    const onUseLocation = () => finish({ action: "useLocation" });
    const onOverlayClick = (event) => {
      if (event.target === overlay) finish(null);
    };
    const onKeyDown = (event) => {
      if (event.key === "Escape") finish(null);
    };

    okButton.addEventListener("click", onStartSeparate);
    cancelButton.addEventListener("click", onUseLocation);
    overlay.addEventListener("click", onOverlayClick);
    document.addEventListener("keydown", onKeyDown);
    cancelButton.focus();
  });
}
