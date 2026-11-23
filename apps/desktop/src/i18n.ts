import { describeError } from "@printkit/i18n";
import { invoke } from "@tauri-apps/api/core";
import { createVueI18n, negotiateLocale, normalizeLocale } from "@printkit/i18n/vue";
import en from "@printkit/i18n/catalog/en.json";
import zh from "@printkit/i18n/catalog/zh-CN.json";

const STORAGE = "printkit.locale";

function initialLocale(): string {
  try {
    const stored = localStorage.getItem(STORAGE);
    if (stored) return normalizeLocale(stored);
  } catch {
    // Storage can be unavailable; fall back to the browser preference.
  }
  const preferences = navigator.languages?.length ? navigator.languages : [navigator.language];
  return negotiateLocale(preferences);
}

export const i18n = createVueI18n({
  locale: initialLocale(),
  messages: { en: en.messages, "zh-CN": zh.messages },
  fallbackMessages: en.messages,
});

/** Mirror the selected locale into the document language and page title. */
export function applyDocumentLocale(): void {
  document.documentElement.lang = i18n.locale.value;
  document.title = i18n.t("shell.windowTitle");
  try {
    void invoke("set_locale", { locale: i18n.locale.value }).catch(() => {});
  } catch {
    // Browser preview: there is no native menu to update.
  }
}

/** Switch in place: no reload, the locale is a reactive ref. */
export function setLocale(next: string): void {
  i18n.setLocale(next);
  try {
    localStorage.setItem(STORAGE, i18n.locale.value);
  } catch {
    // A blocked storage still leaves the in-memory locale usable.
  }
  applyDocumentLocale();
}

/**
 * Normalise a rejected Tauri command (usually the Rust `Display` string
 * `"E_SCHEMA (HTTP 400): detail"`) into the shape `errorText` understands.
 */
export { describeError };

