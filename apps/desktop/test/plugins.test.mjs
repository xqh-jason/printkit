import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  RECOVERY_PLUGIN_IDS,
  canRecoverPlugin,
  isPluginDisabledError,
  pluginAvailability,
  pluginDisabledKind,
  pluginIsOn,
  pluginsKnown,
  pluginTitle,
  recoveryPlugins,
  visiblePages,
} from "../src/plugins/availability.js";
import { BUILTIN_PAGES, PAGE_COMPONENTS, buildShellPages, pagesForPlugin } from "../src/plugins/pluginPages.js";

const here = dirname(fileURLToPath(import.meta.url));

function source(path) {
  return readFileSync(join(here, "..", path), "utf8");
}

function snapshot(overrides = {}) {
  return {
    status: { state: "runningOwned", reachable: true, port: 19080 },
    plugins: [],
    pluginsReachable: true,
    pluginsStale: false,
    pluginsPreset: "all",
    pluginsBroken: null,
    ...overrides,
  };
}

const on = { id: "records", state: "on", nameKey: "plugins.records.name" };
const off = { id: "observability", state: "off", name: "Observability" };

test("stale or unreachable plugin lists are unknown, never on", () => {
  assert.equal(pluginAvailability(null, "records"), "unknown");
  assert.equal(pluginAvailability(snapshot({ plugins: null }), "records"), "unknown");
  assert.equal(pluginAvailability(snapshot({ plugins: [on] }), "observability"), "unknown");
  assert.equal(
    pluginAvailability(snapshot({ plugins: [on], pluginsStale: true }), "records"),
    "unknown",
  );
  assert.equal(
    pluginAvailability(snapshot({ plugins: [on], pluginsReachable: false }), "records"),
    "unknown",
  );
  assert.equal(pluginIsOn(snapshot({ plugins: [on] }), "records"), true);
  assert.equal(pluginIsOn(snapshot({ plugins: [off] }), "observability"), false);
  assert.equal(pluginsKnown(snapshot({ plugins: [on] })), true);
});

test("plugin pages disappear while core pages stay", () => {
  const pages = [
    { id: "overview" },
    { id: "records", plugin: "records" },
    { id: "logs", plugin: "observability" },
    { id: "plugins" },
    { id: "settings" },
  ];
  assert.deepEqual(
    visiblePages(pages, snapshot({ plugins: [on] })).map((page) => page.id),
    ["overview", "records", "plugins", "settings"],
  );
  assert.deepEqual(
    visiblePages(pages, snapshot({ plugins: null })).map((page) => page.id),
    ["overview", "plugins", "settings"],
  );
});

test("recovery rows require owned, reachable, known and off", () => {
  const offRecords = { id: "records", state: "off" };
  const known = snapshot({ plugins: [offRecords, { id: "observability", state: "on" }] });
  assert.deepEqual(recoveryPlugins(known), ["records"]);
  assert.equal(canRecoverPlugin(known, "records"), true);
  assert.equal(canRecoverPlugin(known, "observability"), false);
  assert.deepEqual(RECOVERY_PLUGIN_IDS, ["records", "observability"]);
  assert.deepEqual(
    recoveryPlugins(snapshot({ status: { state: "runningExternal", reachable: true } })),
    [],
  );
  assert.deepEqual(
    recoveryPlugins(snapshot({ plugins: [offRecords], pluginsStale: true })),
    [],
  );
  for (const state of ["pending", "incompatible", "invalid", "broken"]) {
    assert.equal(
      canRecoverPlugin(snapshot({ plugins: [{ id: "records", state }] }), "records"),
      false,
      state,
    );
  }
});

test("disabled pages distinguish unreachable, unknown and disabled", () => {
  assert.equal(
    pluginDisabledKind(snapshot({ status: { state: "stopped", reachable: false } }), "records"),
    "unreachable",
  );
  assert.equal(pluginDisabledKind(snapshot({ plugins: null }), "records"), "unknown");
  const offRecords = { id: "records", state: "off" };
  assert.equal(pluginDisabledKind(snapshot({ plugins: [offRecords] }), "records"), "disabled");
  assert.equal(
    pluginDisabledKind(snapshot({ plugins: [{ id: "records", state: "on" }] }), "records"),
    null,
  );
});

test("disabled-plugin HTTP errors degrade instead of toasting", () => {
  assert.equal(isPluginDisabledError("E_UNSUPPORTED (HTTP 404): the records plugin is disabled"), true);
  assert.equal(isPluginDisabledError("E_NOT_FOUND: record 42"), false);
  assert.equal(isPluginDisabledError("E_AUTH: no admin session"), false);
});

test("plugin titles prefer the i18n key", () => {
  assert.equal(pluginTitle(on), "plugins.records.name");
  assert.equal(pluginTitle(off), "Observability");
  assert.equal(pluginTitle(undefined), "");
});

test("BusinessShell takes every page from pluginPages", () => {
  const shell = source("src/views/BusinessShell.vue");
  assert.match(shell, /buildShellPages\(snapshot\.value, t, locale\.value\)/u);
  assert.doesNotMatch(shell, /const PAGES|defineAsyncComponent|id: "overview"/u);
  assert.match(shell, /current\.value = "overview"/u);
  assert.match(shell, /<Transition name="pk-page" mode="out-in">/u);
  assert.match(shell, /<KeepAlive v-if="!optional" :max="4">/u);
});

test("pluginPages is the single ordered page source", () => {
  assert.deepEqual(Object.keys(PAGE_COMPONENTS).sort(), [
    "jobs",
    "logs",
    "overview",
    "plugins",
    "printers",
    "records",
    "settings",
  ]);
  assert.deepEqual(
    BUILTIN_PAGES.slice()
      .sort((left, right) => left.order - right.order)
      .map((page) => page.id),
    ["overview", "printers", "settings", "plugins", "records", "logs", "jobs"],
  );
  assert.ok(BUILTIN_PAGES.every((page) => page.slot === "navPage" && page.window === "business"));
  assert.equal(new Set(BUILTIN_PAGES.map((page) => page.icon)).size, 7);
  assert.deepEqual(
    BUILTIN_PAGES.filter((page) => page.group === "core").map((page) => page.id),
    ["overview", "printers", "settings"],
  );
  assert.deepEqual(
    BUILTIN_PAGES.filter((page) => page.group === "plugins").map((page) => page.id),
    ["plugins", "records", "logs", "jobs"],
  );
});

test("plugin management is the first plugin-group page", () => {
  const page = BUILTIN_PAGES.find((item) => item.id === "plugins");
  assert.equal(page.group, "plugins");
  assert.equal(page.titleKey, "shell.nav.pluginsManage");
  assert.deepEqual(pagesForPlugin("records").map((item) => item.id), ["records"]);
  assert.deepEqual(pagesForPlugin("observability").map((item) => item.id), ["logs", "jobs"]);
  assert.deepEqual(pagesForPlugin("external"), []);
});

test("buildShellPages orders core pages, gates owners and ignores unknown ids", () => {
  const core = snapshot({ plugins: [{ id: "records", state: "off" }] });
  assert.deepEqual(buildShellPages(core).map((page) => page.id), [
    "overview",
    "printers",
    "settings",
    "plugins",
  ]);

  const allOn = snapshot({
    plugins: [
      { id: "records", state: "on" },
      { id: "observability", state: "on" },
    ],
  });
  assert.deepEqual(buildShellPages(allOn).map((page) => page.id), [
    "overview",
    "printers",
    "settings",
    "plugins",
    "records",
    "logs",
    "jobs",
  ]);
  assert.deepEqual(buildShellPages(allOn).map((page) => page.group), [
    "core",
    "core",
    "core",
    "plugins",
    "plugins",
    "plugins",
    "plugins",
  ]);

  const withUnknown = snapshot({
    plugins: [
      { id: "records", state: "on" },
      {
        id: "records",
        state: "on",
        ui: [{ window: "business", page: "ghost", titles: { en: "Ghost" } }],
      },
    ],
  });
  assert.deepEqual(buildShellPages(withUnknown).map((page) => page.id), [
    "overview",
    "printers",
    "settings",
    "plugins",
    "records",
  ]);
  assert.ok(buildShellPages(withUnknown).every((page) => page.id !== "ghost"));
});

test("known page titles can be overridden by plugin ui metadata", () => {
  const withTitle = snapshot({
    plugins: [
      {
        id: "records",
        state: "on",
        ui: [
          {
            window: "business",
            page: "records",
            titles: { en: "Usage records", "zh-CN": "Usage records" },
          },
        ],
      },
    ],
  });
  assert.equal(buildShellPages(withTitle).find((page) => page.id === "records").label, "Usage records");
});

test("stale or broken plugin state locks every write control", () => {
  const page = source("src/views/PluginsPage.vue");
  assert.match(page, /const locked = computed/u);
  assert.match(page, /!plugins\.known\.value/u);
  assert.match(page, /Boolean\(broken\.value\)/u);
  assert.match(page, /:disabled="locked \|\| busy !== ''"/u);
  assert.match(page, /locked\.value \|\|/u);
  assert.match(page, /plugins\.state\.broken/u);
});

test("records page uses the records plugin and keeps one local-day window", () => {
  const records = source("src/views/BusinessRecords.vue");
  assert.match(records, /plugins\.isOn\("records"\)/u);
  assert.match(records, /plugin-id="records"/u);
  assert.match(records, /localIsoDay\(1 - days\.value\)/u);
  assert.match(records, /localIsoDay\(1\)/u);
  assert.match(records, /successRateUnavailable/u);
  assert.match(records, /records\.statusValue\.cancelled/u);
  assert.doesNotMatch(records, /BarChart|buildDailySeries|recordsStats|rank-/u);
});

test("default core preset leaves records off and calls no records API", () => {
  const core = snapshot({
    pluginsPreset: "core",
    plugins: [{ id: "records", state: "off" }],
  });
  assert.equal(pluginIsOn(core, "records"), false);
  const overview = source("src/views/OverviewPage.vue");
  assert.match(overview, /const recordsOn = computed\(\(\) => plugins\.isOn\("records"\)\)/u);
  assert.match(overview, /recordsOn\.value/u);
  assert.match(overview, /recoveryPlugins\(props\.snapshot\)/u);
  assert.match(overview, /pluginEnableOwned/u);
});

test("PluginDisabled and BusinessRecords route recovery inside the shell", () => {
  const disabled = source("src/views/PluginDisabled.vue");
  assert.match(disabled, /pluginId: "records" \| "observability"/u);
  assert.match(disabled, /emit\('openPage', 'plugins'\)/u);
  const records = source("src/views/BusinessRecords.vue");
  assert.match(records, /openPage: \[id: string\]/u);
  assert.match(records, /@open-page="emit\('openPage', 'plugins'\)"/u);
});

test("the plugin page uses registered controls and does not add el-form", () => {
  const page = source("src/views/PluginsPage.vue");
  assert.doesNotMatch(page, /<el-form/u);
  assert.match(page, /<el-switch/u);
  assert.match(page, /<el-select/u);
  assert.match(page, /<el-input-number/u);
  assert.match(page, /confirmDataCount/u);
  assert.match(page, /onPreset\('all'\)/u);
  assert.match(page, /plugins\.preset\.conflicts/u);
});

test("check-size asserts the single-shell and plugin-page chunks", () => {
  const script = source("scripts/check-size.mjs");
  assert.match(script, /isDynamicEntry/u);
  assert.match(script, /BusinessShell/u);
  assert.match(script, /PluginsPage/u);
  assert.match(script, /40_000/u);
});
