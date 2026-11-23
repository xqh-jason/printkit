import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  RETENTION_MAX_DAYS,
  RETENTION_MIN_DAYS,
  retentionDaysValid,
  retentionSourceKey,
} from "../src/settingsRetention.js";

const here = dirname(fileURLToPath(import.meta.url));

function source(path) {
  return readFileSync(join(here, "..", path), "utf8");
}

function catalog(locale) {
  return JSON.parse(readFileSync(join(here, "..", "..", "..", "packages/i18n/catalog", `${locale}.json`), "utf8"));
}

test("retention days accept only 1..3650 whole days", () => {
  assert.equal(RETENTION_MIN_DAYS, 1);
  assert.equal(RETENTION_MAX_DAYS, 3650);
  for (const value of [1, 30, 90, 365, 3650]) assert.equal(retentionDaysValid(value), true);
  for (const value of [0, -1, 1.5, 3651, Number.NaN, "90", null, undefined]) {
    assert.equal(retentionDaysValid(value), false);
  }
});

test("RetentionView and both commands are declared", () => {
  const types = source("src/types.ts");
  assert.match(types, /interface RetentionView/u);
  for (const field of [
    "days",
    "source",
    "logDays",
    "logSource",
    "recordsDays",
    "recordsSource",
    "restartRequired",
  ]) {
    assert.match(types, new RegExp(`\\b${field}:`, "u"));
  }

  const api = source("src/api.ts");
  assert.match(api, /invoke<RetentionView>\("get_retention_days"\)/u);
  assert.match(api, /invoke<RetentionView>\("set_retention_days", \{ days \}\)/u);
});

test("Settings loads the backend default and guards invalid input", () => {
  const settings = source("src/views/SettingsPage.vue");
  assert.match(settings, /await getRetentionDays\(\)/u);
  assert.match(settings, /retentionDraft\.value = view\.days/u);
  assert.doesNotMatch(settings, /retentionDraft = ref\(90\)/u, "90 is a backend default, not a UI literal");
  assert.match(settings, /retentionDaysValid\(days\)/u);
  assert.match(settings, /:min="RETENTION_MIN_DAYS"/u);
  assert.match(settings, /:disabled="retentionInvalid/u);
  assert.match(settings, /t\("business\.settings\.retentionInvalid"\)/u);

  assert.match(settings, /let retentionGeneration = 0/u);
  assert.match(settings, /if \(generation !== retentionGeneration\) return/u);
  assert.match(settings, /if \(generation === retentionGeneration\) retentionLoading\.value = false/u);
  assert.match(settings, /if \(retentionBusy\.value \|\| retentionLoading\.value\) return/u);
});

test("retention write command is business-only and owned-only", () => {
  const rust = source("src-tauri/src/commands.rs");
  const command = rust.slice(rust.indexOf("pub async fn set_retention_days"));
  assert.match(command, /window: tauri::WebviewWindow/u);
  assert.match(command, /require_window\(&window, WindowCapability::Business\)/u);
  assert.match(command, /require_owned\(&app\)/u);
  assert.ok(command.indexOf("require_owned") < command.indexOf("refresh_config"));
});

test("Settings saves by command, shows restart and handles errors", () => {
  const settings = source("src/views/SettingsPage.vue");
  assert.match(settings, /await setRetentionDays\(days\)/u);
  assert.match(settings, /view\.restartRequired/u);
  assert.match(settings, /retentionSavedRestart/u);
  assert.match(settings, /retentionLoadError/u);
  assert.match(settings, /retentionSaveError/u);
  assert.match(settings, /ElMessage\.error\(retentionSaveError\.value\)/u);
  assert.match(settings, /retentionSourceLabel\(retention\.source\)/u);
  assert.match(settings, /if \(view\.restartRequired\) retentionRestart\.value = true/u);
  assert.match(settings, /retentionRestart\.value = view\.restartRequired/u);
  assert.match(settings, /if \(retentionBusy\.value\) return/u);
  assert.match(settings, /if \(syncRestart\) retentionRestart\.value = view\.restartRequired/u);
  assert.match(settings, /void loadRetention\(true\)/u);
  assert.match(settings, /if \(!previous \|\| state === previous \|\| retentionBusy\.value\) return/u);
  assert.match(settings, /retentionSaveFailed/u);
  assert.match(settings, /@click="loadRetention\(\)"/u);
  assert.doesNotMatch(settings, /@click="loadRetention"/u);
  assert.match(settings, /await loadRetention\(\)/u);
});
test("backend source values map to catalogue keys", () => {
  assert.equal(retentionSourceKey("cli"), "business.settings.retentionSource.cli");
  assert.equal(retentionSourceKey("env"), "business.settings.retentionSource.env");
  assert.equal(retentionSourceKey("file"), "business.settings.retentionSource.file");
  assert.equal(retentionSourceKey("db"), "business.settings.retentionSource.db");
  assert.equal(retentionSourceKey("default"), "business.settings.retentionSource.default");
  assert.equal(retentionSourceKey("future"), "business.settings.retentionSource.unknown");
});

test("retention catalogue keys exist in both locales", () => {
  const keys = [
    "retention",
    "retentionComponents",
    "retentionCurrent",
    "retentionDays",
    "retentionHint",
    "retentionInvalid",
    "retentionLoading",
    "retentionRange",
    "retentionRestart",
    "retentionRestartHint",
    "retentionRetry",
    "retentionSave",
    "retentionSaveFailed",
    "retentionSaved",
    "retentionSavedRestart",
    "retentionSource",
    "retentionSource.cli",
    "retentionSource.db",
    "retentionSource.default",
    "retentionSource.env",
    "retentionSource.file",
    "retentionSource.unknown",
    "retentionUnavailable",
    "retentionUnified",
  ];
  for (const locale of ["en", "zh-CN"]) {
    const messages = catalog(locale).messages;
    for (const key of keys) assert.ok(messages[`business.settings.${key}`], `${locale}: ${key}`);
  }
});
