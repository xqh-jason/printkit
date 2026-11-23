import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  createFormatters,
  createI18n,
  describeError,
  errorText,
  interpolate,
  matchLocale,
  negotiateLocale,
  normalizeLocale,
  statusText,
  translate,
} from "../src/core.js";

const here = dirname(fileURLToPath(import.meta.url));
const en = JSON.parse(readFileSync(join(here, "../catalog/en.json"), "utf8")).messages;
const zh = JSON.parse(readFileSync(join(here, "../catalog/zh-CN.json"), "utf8")).messages;

test("locale matching collapses Chinese and rejects unknown tags", () => {
  assert.equal(matchLocale("zh-Hans-CN"), "zh-CN");
  assert.equal(matchLocale("zh-TW"), "zh-CN");
  assert.equal(matchLocale("en-GB"), "en");
  assert.equal(matchLocale("fr"), undefined);
  assert.equal(normalizeLocale("fr"), "en");
  assert.equal(negotiateLocale(["fr", "zh-CN"]), "zh-CN");
  assert.equal(negotiateLocale([]), "en");
});

test("interpolation and fallback keep missing keys visible", () => {
  assert.equal(interpolate("Hello {name}", { name: "Ada" }), "Hello Ada");
  assert.equal(interpolate("Hello {name}", {}), "Hello {name}");
  assert.equal(translate(en, en, "logs.table.time"), "Time");
  assert.equal(translate(zh, en, "logs.table.time"), "时间");
  assert.equal(translate({}, en, "logs.table.time"), "Time");
  assert.equal(translate({}, {}, "missing.key"), "missing.key");
});

test("formatters follow the selected locale", () => {
  const enFormatters = createFormatters("en");
  const zhFormatters = createFormatters("zh-CN");
  assert.equal(enFormatters.formatNumber(1234.5), "1,234.5");
  assert.equal(zhFormatters.formatNumber(1234.5), "1,234.5");
  assert.equal(enFormatters.formatPercent(0.123), "12.3%");
  assert.equal(zhFormatters.formatPercent(0.123), "12.3%");
  const when = "2026-11-09T14:30:00";
  assert.match(enFormatters.formatTime(when), /14:30:00/);
  assert.match(zhFormatters.formatTime(when), /14:30:00/);
});

test("createI18n switches locale behind a getter", () => {
  let locale = "en";
  const i18n = createI18n({
    getLocale: () => locale,
    messages: { en, "zh-CN": zh },
    fallbackMessages: en,
  });
  assert.equal(i18n.t("shell.nav.logs"), "Event stream");
  assert.equal(i18n.t("logs.result.ok"), "Success");
  locale = "zh-CN";
  assert.equal(i18n.t("shell.nav.logs"), "事件流");
  assert.equal(i18n.t("logs.result.ok"), "成功");
  assert.equal(i18n.t("logs.result.failed"), "失败");
  assert.equal(i18n.t("error.client.http", { status: 500 }), "请求失败（HTTP 500）。");
});

test("error and status labels keep the raw diagnostic", () => {
  const t = (key, params) => translate(zh, en, key, params);
  assert.equal(
    errorText(t, { code: "E_AUTH", message: "an administrator session is required" }),
    "需要登录\n原始信息： an administrator session is required",
  );
  assert.equal(errorText(t, { status: 500 }), "请求失败（HTTP 500）。");
  assert.equal(statusText(t, "done"), "完成");
  assert.equal(statusText(t, "queued"), "queued");
});

test("command rejections map their E_ code to a localized title", () => {
  const t = (key, params) => translate(zh, en, key, params);
  // These are the exact strings ControlError::fmt produces in the desktop shell.
  const shaped = describeError("E_SCHEMA (HTTP 400): invalid document: /pages/0/items");
  assert.equal(shaped.code, "E_SCHEMA");
  assert.equal(shaped.status, 400);
  assert.equal(shaped.message, "invalid document: /pages/0/items");
  assert.equal(
    errorText(t, shaped),
    "文档校验失败\n原始信息： invalid document: /pages/0/items",
  );

  const timeout = describeError("E_TIMEOUT: request timed out after 10s");
  assert.equal(timeout.code, "E_TIMEOUT");
  assert.equal(timeout.status, undefined);
  assert.equal(
    errorText(t, timeout),
    "操作超时\n原始信息： request timed out after 10s",
  );

  // A plain lifecycle message keeps its detail instead of inventing a code.
  const plain = describeError("The printkit sidecar is still being checked.");
  assert.equal(plain.code, undefined);
  assert.match(errorText(t, plain), /^请求失败\n原始信息： The printkit sidecar/u);
});
