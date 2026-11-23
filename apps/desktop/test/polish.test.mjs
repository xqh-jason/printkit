import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = (path) => readFileSync(join(here, "..", path), "utf8");
const root = (path) => readFileSync(join(here, "..", "..", "..", path), "utf8");
const catalog = (name) => JSON.parse(readFileSync(join(here, "..", "..", "..", "packages/i18n/catalog", name), "utf8")).messages;

test("logs are presented as an event stream with success/failure", () => {
  const logs = source("src/views/DevLogs.vue");
  assert.match(logs, /logs\.subtitle/u);
  assert.match(logs, /logResultKey\(row\.ok\)/u);
  assert.equal(catalog("en.json")["shell.nav.logs"], "Event stream");
  assert.equal(catalog("zh-CN.json")["shell.nav.logs"], "\u4e8b\u4ef6\u6d41");
});

test("overview copy names print tasks, failure denominator and system printers", () => {
  const overview = source("src/views/OverviewPage.vue");
  assert.match(overview, /business\.overview\.unknownPrinter/u);
  assert.match(overview, /compact\.successfulPrintsHint/u);
  assert.match(overview, /business\.overview\.failureRateHint/u);
  assert.equal(catalog("en.json")["compact.printerCount"], "System printers");
  assert.equal(catalog("en.json")["business.overview.failureRate"], "Print failure rate");
  assert.equal(catalog("en.json")["business.overview.printersTitle"], "Frequent printers");
  assert.equal(catalog("en.json")["business.overview.recentTitle"], "Recent print tasks");
});

test("settings keeps locked paths and no diagnostics panel", () => {
  const settings = source("src/views/SettingsPage.vue");
  assert.doesNotMatch(settings, /advancedOpen|el-collapse|business\.settings\.advanced/u);
  assert.doesNotMatch(settings, /business\.settings\.(config|doctor|output|runDoctor)/u);
  assert.doesNotMatch(settings, /configJson|recentSidecarOutput|runDoctor/u);
  assert.match(settings, /business\.settings\.noLogDir/u);
  assert.match(settings, /business\.settings\.noLockPath/u);
  assert.match(settings, /<details class="tech-details">/u);
  assert.match(settings, /business\.settings\.technical/u);
  assert.equal(catalog("en.json")["business.settings.technical"], "Runtime info");
  assert.equal(catalog("zh-CN.json")["business.settings.technical"], "\u8fd0\u884c\u4fe1\u606f");
  assert.equal(catalog("en.json")["plugins.techDetails"], "Plugin details");
  assert.equal(catalog("zh-CN.json")["plugins.techDetails"], "\u63d2\u4ef6\u8be6\u60c5");
  assert.doesNotMatch(settings, /plugins\.techDetails/u, "Settings must not reuse the plugin label");
});

test("plugins collapse technical fields and lock configure", () => {
  const plugins = source("src/views/PluginsPage.vue");
  assert.match(plugins, /<details class="plugin-tech"/u);
  assert.match(plugins, /plugins\.techDetails/u);
  assert.match(plugins, /:disabled="locked"/u);
  assert.match(plugins, /plugins\.configureLocked/u);
});

test("printers disclose v1 capability and test-print limits", () => {
  const printers = source("src/views/PrintersPage.vue");
  assert.match(printers, /business\.printers\.v1CapabilityHint/u);
  assert.match(printers, /business\.printTest\.knownLimit/u);
});

test("public docs describe base print counts without label assumptions", () => {
  const readme = root("README.md");
  const zh = root("README.zh-CN.md");
  assert.match(readme, /one successful print request counts as 1/u);
  assert.doesNotMatch(readme, /1 page as 1 label/u);
  assert.doesNotMatch(zh, /1 \u9875\u6309 1 \u5f20\u6807\u7b7e\u8ba1/u);
  assert.match(zh, /\u4e00\u6b21\u6210\u529f\u6253\u5370\u8bf7\u6c42\u8ba1 1/u);
  assert.match(root("apps/desktop/README.md"), /deprecated compatibility field/u);
});
