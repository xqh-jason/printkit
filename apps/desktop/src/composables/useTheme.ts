import { computed, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";

export type ThemeSetting = "system" | "light" | "dark";

const STORAGE = "printkit.theme";

function stored(): ThemeSetting {
  try {
    const value = localStorage.getItem(STORAGE);
    if (value === "light" || value === "dark" || value === "system") return value;
  } catch {
    // Storage can be unavailable; fall back to the system preference.
  }
  return "system";
}

const setting = ref<ThemeSetting>(stored());
const media = typeof window !== "undefined" ? window.matchMedia("(prefers-color-scheme: dark)") : null;
const systemDark = ref(Boolean(media?.matches));
const resolved = computed<"light" | "dark">(() =>
  setting.value === "system" ? (systemDark.value ? "dark" : "light") : setting.value,
);

/**
 * `data-theme` is the single source of truth for both our tokens and the
 * Element Plus variable mapping. `html.dark` is kept as a standard alias and
 * `set_theme` only drives the native title bar.
 */
function apply(): void {
  if (typeof document === "undefined") return;
  const theme = resolved.value;
  const root = document.documentElement;
  root.dataset.theme = theme;
  root.classList.toggle("dark", theme === "dark");
  root.style.colorScheme = theme;
  try {
    void invoke("set_theme", { theme: setting.value }).catch(() => {});
  } catch {
    // Browser preview: no native window to theme.
  }
}

export function bootTheme(): void {
  // Runs before the app is mounted, so the first painted frame is correct.
  apply();
  media?.addEventListener("change", (event) => {
    systemDark.value = event.matches;
    if (setting.value === "system") apply();
  });
  if (typeof window !== "undefined") window.addEventListener("storage", (event) => {
    if (event.key !== STORAGE) return;
    const value = event.newValue;
    if (value === "light" || value === "dark" || value === "system") {
      setting.value = value;
      apply();
    }
  });
}

export function useTheme() {
  function setTheme(next: ThemeSetting): void {
    setting.value = next;
    try {
      localStorage.setItem(STORAGE, next);
    } catch {
      // The in-memory setting still applies.
    }
    apply();
  }
  return { theme: setting, resolved, systemDark, setTheme };
}
