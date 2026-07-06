function systemTheme() {
  return window.matchMedia?.("(prefers-color-scheme: dark)")?.matches ? "dark" : "light";
}

function applyTheme() {
  const selected = ["system", "light", "dark"].includes(state.theme) ? state.theme : "system";
  const resolved = selected === "system" ? systemTheme() : selected;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.dataset.themePreference = selected;
  updateThemeControls();
}

function updateThemeControls() {
  document.querySelectorAll("[data-theme-option]").forEach((button) => {
    button.classList.toggle("is-active", button.dataset.themeOption === state.theme);
  });
}

function setTheme(theme) {
  state.theme = ["system", "light", "dark"].includes(theme) ? theme : "system";
  writeLocalSetting("theme", state.theme);
  applyTheme();
}

const systemThemeQuery = window.matchMedia?.("(prefers-color-scheme: dark)");
systemThemeQuery?.addEventListener("change", () => {
  if (state.theme === "system") applyTheme();
});
