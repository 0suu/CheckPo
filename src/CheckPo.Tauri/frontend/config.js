const immediatelyCancellableCommands = new Set([
  "create_checkpoint",
  "diff_checkpoint_full",
  "preview_restore",
  "apply_restore",
  "preview_discard_files",
  "apply_discard_files",
  "verify_project",
  "rebuild_index",
]);

const progressCancellableStartCommands = new Set([
  "init_project",
  "start_as_separate_project",
]);
