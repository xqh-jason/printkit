const PLACEHOLDER = /\{([a-zA-Z_][a-zA-Z0-9_]*)\}/g;

/** Supported UI locales, in negotiation order. */
export const LOCALES = ["en", "zh-CN"];

/**
 * Map one language tag to a supported locale, or undefined when unsupported.
 * Chinese regions and scripts all collapse to `zh-CN`; everything else that
 * starts with `en` maps to `en`.
 */
export function matchLocale(value) {
  if (typeof value !== "string") return undefined;
  const primary = value.trim().toLowerCase().split(/[-_]/)[0];
  if (primary === "zh") return "zh-CN";
  if (primary === "en") return "en";
  return undefined;
}

/** Like {@link matchLocale}, but an unknown tag falls back to `en`. */
export function normalizeLocale(value) {
  return matchLocale(value) ?? "en";
}

/** First supported tag in a browser-style preference list. */
export function negotiateLocale(candidates = []) {
  const list = Array.isArray(candidates) ? candidates : [candidates];
  for (const candidate of list) {
    const locale = matchLocale(candidate);
    if (locale) return locale;
  }
  return "en";
}

/** Replace `{name}` placeholders; unknown or missing parameters stay visible. */
export function interpolate(message, params) {
  if (typeof message !== "string") return "";
  if (!params) return message;
  return message.replace(PLACEHOLDER, (match, key) =>
    Object.prototype.hasOwnProperty.call(params, key) ? String(params[key]) : match,
  );
}

/**
 * Resolve one key: current catalogue, then fallback catalogue, then the key.
 * The key is returned deliberately so a missing translation is visible in a
 * page review instead of becoming an empty button.
 */
export function translate(messages, fallbackMessages, key, params) {
  const current = messages && messages[key];
  const fallback = fallbackMessages && fallbackMessages[key];
  const text = current ?? fallback ?? key;
  return interpolate(text, params);
}

/** Build the locale-aware formatters once for one tag. */
export function createFormatters(locale) {
  const tag = normalizeLocale(locale);
  const dateFormatters = new Map();
  const number = new Intl.NumberFormat(tag);
  const percent = new Intl.NumberFormat(tag, {
    style: "percent",
    maximumFractionDigits: 1,
  });

  const asDate = (value) => {
    const date = value instanceof Date ? value : new Date(value);
    return Number.isNaN(date.getTime()) ? undefined : date;
  };
  const dateFormatter = (options) => {
    const cacheKey = JSON.stringify(options);
    let formatter = dateFormatters.get(cacheKey);
    if (!formatter) {
      formatter = new Intl.DateTimeFormat(tag, options);
      dateFormatters.set(cacheKey, formatter);
    }
    return formatter;
  };
  const formatWith = (value, options) => {
    const date = asDate(value);
    return date ? dateFormatter(options).format(date) : String(value ?? "");
  };

  return {
    formatNumber: (value) => number.format(Number(value ?? 0)),
    formatPercent: (value) => percent.format(Number(value ?? 0)),
    formatDate: (value, options = {}) => formatWith(value, options),
    formatTime: (value, options = {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hourCycle: "h23",
    }) => formatWith(value, options),
    formatDateTime: (value, options = {
      dateStyle: "medium",
      timeStyle: "medium",
    }) => formatWith(value, options),
  };
}

/**
 * Framework-free facade. `getLocale` is read on every call so a Vue ref can
 * drive it without a second implementation.
 */
export function createI18n({ locale = "en", messages = {}, fallbackMessages = {}, getLocale } = {}) {
  let current = normalizeLocale(locale);
  const read = () => normalizeLocale(getLocale ? getLocale() : current);
  const formatters = new Map();
  const formatter = () => {
    const tag = read();
    let value = formatters.get(tag);
    if (!value) {
      value = createFormatters(tag);
      formatters.set(tag, value);
    }
    return value;
  };

  return {
    get locale() {
      return read();
    },
    setLocale(next) {
      current = normalizeLocale(next);
    },
    t(key, params) {
      const tag = read();
      return translate(messages[tag] || messages.en, fallbackMessages || messages.en, key, params);
    },
    formatNumber: (value) => formatter().formatNumber(value),
    formatPercent: (value) => formatter().formatPercent(value),
    formatDate: (value, options) => formatter().formatDate(value, options),
    formatTime: (value, options) => formatter().formatTime(value, options),
    formatDateTime: (value, options) => formatter().formatDateTime(value, options),
  };
}

/** The error codes the pages can surface, mapped to their catalogue titles. */
export const ERROR_TITLES = {
  E_CONNECT: "error.code.E_CONNECT.title",
  E_AUTH: "error.code.E_AUTH.title",
  E_ORIGIN: "error.code.E_ORIGIN.title",
  E_CSRF: "error.code.E_CSRF.title",
  E_SCHEMA: "error.code.E_SCHEMA.title",
  E_PRINTER_NOT_FOUND: "error.code.E_PRINTER_NOT_FOUND.title",
  E_PAPER_UNSUPPORTED: "error.code.E_PAPER_UNSUPPORTED.title",
  E_JOB_FAILED: "error.code.E_JOB_FAILED.title",
  E_TIMEOUT: "error.code.E_TIMEOUT.title",
  E_PROTOCOL: "error.code.E_PROTOCOL.title",
  E_UNSUPPORTED: "error.code.E_UNSUPPORTED.title",
  E_CANCEL_UNSUPPORTED: "error.code.E_CANCEL_UNSUPPORTED.title",
  E_RAW_UNSUPPORTED: "error.code.E_RAW_UNSUPPORTED.title",
  E_PDF_ENCRYPTED: "error.code.E_PDF_ENCRYPTED.title",
  E_NOT_FOUND: "error.code.E_NOT_FOUND.title",
  E_TOO_LARGE: "error.code.E_TOO_LARGE.title",
  E_RENDER_CRASH: "error.code.E_RENDER_CRASH.title",
  E_ALREADY_RUNNING: "error.code.E_ALREADY_RUNNING.title",
  "client.network": "error.client.network",
  "client.http": "error.client.http",
  "client.session": "error.client.session",
  "client.recordsSession": "error.client.recordsSession",
};

/** Record status values that have a human label, keyed by the stored value. */
export const STATUS_TITLES = {
  done: "records.statusValue.done",
  failed: "records.statusValue.failed",
};

/**
 * Normalise a rejected Tauri command (usually the Rust `Display` string
 * `"E_SCHEMA (HTTP 400): detail"`) or an Error into the fields `errorText`
 * understands. Rust keeps the detail in English on purpose.
 */
export function describeError(error) {
  if (typeof error === "string") return parseErrorString(error);
  if (error instanceof Error) {
    const shaped = error;
    return { code: shaped.code, status: shaped.status, message: shaped.message };
  }
  if (error && typeof error === "object") {
    const shaped = error;
    if (typeof shaped.code === "string" || typeof shaped.message === "string") {
      return {
        code: typeof shaped.code === "string" ? shaped.code : undefined,
        status: typeof shaped.status === "number" ? shaped.status : undefined,
        message: typeof shaped.message === "string" ? shaped.message : undefined,
      };
    }
  }
  return { message: String(error ?? "") };
}

const ERROR_PREFIX = /^([A-Z][A-Z0-9_]*)(?:\s*\(HTTP (\d{3})\))?:\s*([\s\S]*)$/u;

function parseErrorString(value) {
  const text = value.trim();
  const match = ERROR_PREFIX.exec(text);
  if (!match) return { message: text };
  return {
    code: match[1],
    status: match[2] ? Number(match[2]) : undefined,
    message: match[3] || text,
  };
}

/**
 * Localized title for an API/transport error, with the original server text
 * kept on a second line for diagnostics.
 */
export function errorText(t, error) {
  const code = error && error.code;
  const status = error && error.status;
  const key = (code && ERROR_TITLES[code])
    || (status ? "error.client.http" : undefined)
    || "error.unknownCode";
  let title = t(key, { status });
  if (error && error.message) {
    title += `\n${t("error.rawDetail")} ${error.message}`;
  }
  return title;
}

/** Localized record status, falling back to the stored value. */
export function statusText(t, value) {
  const key = STATUS_TITLES[value];
  return key ? t(key) : String(value ?? "");
}
