import { ref } from "vue";

import { createI18n, errorText, normalizeLocale, statusText } from "./core.js";

/**
 * Vue adapter over the shared runtime. The returned `t` reads the reactive
 * locale ref during render, so a switch re-renders every component that used
 * it without a page reload.
 */
export function createVueI18n({ locale = "en", messages = {}, fallbackMessages = {} } = {}) {
  const current = ref(normalizeLocale(locale));
  const i18n = createI18n({
    getLocale: () => current.value,
    messages,
    fallbackMessages,
  });

  return {
    locale: current,
    setLocale(next) {
      current.value = normalizeLocale(next);
    },
    t: (key, params) => i18n.t(key, params),
    formatNumber: (value) => i18n.formatNumber(value),
    formatPercent: (value) => i18n.formatPercent(value),
    formatDate: (value, options) => i18n.formatDate(value, options),
    formatTime: (value, options) => i18n.formatTime(value, options),
    formatDateTime: (value, options) => i18n.formatDateTime(value, options),
    errorText: (error) => errorText(i18n.t, error),
    statusText: (value) => statusText(i18n.t, value),
  };
}

export { LOCALES, matchLocale, negotiateLocale, normalizeLocale } from "./core.js";
