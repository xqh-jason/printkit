import { LOCALES } from "@printkit/i18n";

import { i18n } from "./i18n";

/** One source for the language list used by the select and the compact panel. */
export function localeOptions(): Array<{ value: string; label: string }> {
  return LOCALES.map((value) => ({
    value,
    label: i18n.t(value === "zh-CN" ? "lang.zhCN" : "lang.en"),
  }));
}
