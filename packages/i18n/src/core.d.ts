export type Locale = "en" | "zh-CN";

export interface Messages {
  [key: string]: string;
}

export interface Formatters {
  formatNumber(value: unknown): string;
  formatPercent(value: unknown): string;
  formatDate(value: unknown, options?: Intl.DateTimeFormatOptions): string;
  formatTime(value: unknown, options?: Intl.DateTimeFormatOptions): string;
  formatDateTime(value: unknown, options?: Intl.DateTimeFormatOptions): string;
}

export interface I18n extends Formatters {
  readonly locale: Locale;
  setLocale(next: string): void;
  t(key: string, params?: Record<string, unknown>): string;
}

export interface I18nOptions {
  locale?: string;
  messages?: Record<string, Messages>;
  fallbackMessages?: Messages;
  getLocale?: () => string;
}

export const LOCALES: readonly Locale[];

export function matchLocale(value: unknown): Locale | undefined;

export function normalizeLocale(value: unknown): Locale;

export function negotiateLocale(candidates?: string | readonly string[]): Locale;

export function interpolate(message: string, params?: Record<string, unknown>): string;

export function translate(
  messages: Messages | undefined,
  fallbackMessages: Messages | undefined,
  key: string,
  params?: Record<string, unknown>,
): string;

export function createFormatters(locale: string): Formatters;

export function createI18n(options?: I18nOptions): I18n;

export const ERROR_TITLES: Record<string, string>;

export const STATUS_TITLES: Record<string, string>;

export interface ShapedError {
  code?: string;
  message?: string;
  status?: number;
}

export function describeError(error: unknown): ShapedError;

export function errorText(
  t: (key: string, params?: Record<string, unknown>) => string,
  error: ShapedError | null | undefined,
): string;

export function statusText(
  t: (key: string, params?: Record<string, unknown>) => string,
  value: unknown,
): string;
