import type { Ref } from "vue";

import type { Formatters, Locale, Messages } from "./core";

export interface VueI18n extends Formatters {
  locale: Ref<Locale>;
  setLocale(next: string): void;
  t(key: string, params?: Record<string, unknown>): string;
  errorText(error: { code?: string; message?: string; status?: number } | null | undefined): string;
  statusText(value: unknown): string;
}

export function createVueI18n(options?: {
  locale?: string;
  messages?: Record<string, Messages>;
  fallbackMessages?: Messages;
}): VueI18n;

export { LOCALES, matchLocale, negotiateLocale, normalizeLocale } from "./core";
