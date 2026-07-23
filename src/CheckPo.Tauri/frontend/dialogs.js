const MODAL_OVERLAY_SELECTOR = [
  ".settings-overlay",
  ".advanced-overlay",
  ".project-registration-overlay",
  ".project-selection-overlay",
  ".rollback-overlay",
  ".recovery-conflict-overlay",
  ".confirm-overlay",
  ".error-overlay",
  ".busy-overlay",
].join(",");

const MODAL_CLOSE_BUTTON_IDS = Object.freeze({
  settingsOverlay: "closeSettingsButton",
  advancedOverlay: "closeAdvancedButton",
  projectRegistrationOverlay: "closeProjectRegistrationButton",
  projectSelectionOverlay: "closeProjectSelectionButton",
  rollbackOverlay: "closeRollbackDialogButton",
  recoveryConflictOverlay: "closeRecoveryConflictButton",
  errorOverlay: "dismissErrorDialogButton",
});

const MODAL_FOCUSABLE_SELECTOR = [
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "a[href]",
  "[tabindex]:not([tabindex='-1'])",
].join(",");

function setupModalAccessibility() {
  let activeOverlay = null;
  let returnFocus = null;

  const visibleOverlays = () => Array.from(document.querySelectorAll(MODAL_OVERLAY_SELECTOR))
    .filter((overlay) => !overlay.hidden);
  const update = () => {
    const overlays = visibleOverlays();
    const nextOverlay = overlays.find((overlay) => overlay.id === "errorOverlay")
      || overlays.find((overlay) => overlay.id === "confirmOverlay")
      || overlays.at(-1)
      || null;
    const overlayChanged = activeOverlay !== nextOverlay;
    if (!activeOverlay && nextOverlay) returnFocus = document.activeElement;
    for (const child of document.body.children) {
      if (child.tagName === "SCRIPT") continue;
      const blocked = Boolean(nextOverlay) && child !== nextOverlay;
      child.inert = blocked;
      if (blocked) child.setAttribute("aria-hidden", "true");
      else child.removeAttribute("aria-hidden");
    }
    activeOverlay = nextOverlay;
    if (overlayChanged && activeOverlay && !activeOverlay.contains(document.activeElement)) {
      const first = activeOverlay.querySelector(MODAL_FOCUSABLE_SELECTOR)
        || activeOverlay.querySelector("[role='dialog'][tabindex], [role='alertdialog'][tabindex]");
      if (first) queueMicrotask(() => first.focus());
    }
    if (!activeOverlay && returnFocus?.isConnected) {
      returnFocus.focus();
      returnFocus = null;
    }
  };

  const observer = new MutationObserver(update);
  for (const overlay of document.querySelectorAll(MODAL_OVERLAY_SELECTOR)) {
    observer.observe(overlay, { attributes: true, attributeFilter: ["hidden"] });
  }
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && activeOverlay) {
      const closeButtonId = MODAL_CLOSE_BUTTON_IDS[activeOverlay.id];
      if (closeButtonId) {
        event.preventDefault();
        $(closeButtonId)?.click();
        return;
      }
    }
    if (event.key !== "Tab" || !activeOverlay) return;
    const focusable = Array.from(activeOverlay.querySelectorAll(MODAL_FOCUSABLE_SELECTOR))
      .filter((element) => !element.hidden && element.getClientRects().length > 0);
    if (!focusable.length) {
      event.preventDefault();
      activeOverlay
        .querySelector("[role='dialog'][tabindex], [role='alertdialog'][tabindex]")
        ?.focus();
      return;
    }
    const first = focusable[0];
    const last = focusable.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    } else if (!activeOverlay.contains(document.activeElement)) {
      event.preventDefault();
      first.focus();
    }
  });
  update();
}

window.addEventListener("DOMContentLoaded", setupModalAccessibility);

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
    okButton.disabled = false;
    cancelButton.disabled = false;

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
    okButton.disabled = false;
    cancelButton.disabled = false;

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
