import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const en = readJson(join(here, "../catalog/en.json"));
const zh = readJson(join(here, "../catalog/zh-CN.json"));
const enKeys = Object.keys(en.messages);
const zhKeys = Object.keys(zh.messages);
const problems = [];
const warnings = [];

const placeholders = (text) =>
  new Set([...String(text).matchAll(/\{([a-zA-Z_][a-zA-Z0-9_]*)\}/g)].map((match) => match[1]));

assert.equal(en.locale, "en", "en catalogue locale");
assert.equal(zh.locale, "zh-CN", "zh-CN catalogue locale");
assert.deepEqual([...enKeys].sort(), enKeys, "en keys must be sorted");
assert.deepEqual([...zhKeys].sort(), zhKeys, "zh-CN keys must be sorted");

for (const key of enKeys) {
  if (!zh.messages[key]) problems.push(`zh-CN is missing ${key}`);
}
for (const key of zhKeys) {
  if (!en.messages[key]) problems.push(`en is missing ${key}`);
}
for (const [locale, catalog] of [["en", en], ["zh-CN", zh]]) {
  for (const [key, value] of Object.entries(catalog.messages)) {
    if (!String(value).trim()) problems.push(`${locale} has an empty ${key}`);
    if (/\|/u.test(value) || /@:/u.test(value)) {
      problems.push(`${locale} uses unsupported message syntax in ${key}`);
    }
  }
}
for (const key of enKeys) {
  if (!en.messages[key] || !zh.messages[key]) continue;
  const left = [...placeholders(en.messages[key])].sort();
  const right = [...placeholders(zh.messages[key])].sort();
  if (JSON.stringify(left) !== JSON.stringify(right)) {
    problems.push(`${key} placeholders differ: en=${left} zh=${right}`);
  }
}

const used = new Set();
const sourceFiles = [];
const walk = (path) => {
  for (const entry of readdirSync(path)) {
    const full = join(path, entry);
    if (entry === "node_modules" || entry === "dist" || entry === "target") continue;
    const stat = statSync(full);
    if (stat.isDirectory()) walk(full);
    else if (/\.(?:vue|js|ts|mjs|html)$/u.test(entry)) sourceFiles.push(full);
  }
};
try {
  walk(join(root, "apps/desktop/src"));
} catch {
  // A missing surface is not an i18n failure; the desktop worker adds keys.
}

const checkKey = (key, where) => {
  if (key === "i18n_bootstrap") return;
  used.add(key);
  if (!en.messages[key]) problems.push(`${where} references missing key ${key}`);
};

// Template keys the scanner cannot resolve. Keep one entry per expression so
// an unlisted dynamic key fails instead of silently becoming an unused key.
const knownDynamic = [
  {
    file: "apps/desktop/src/views/BusinessOverview.vue",
    template: "shell.service.state.${state}",
    keys: [
      "shell.service.state.runningOwned",
      "shell.service.state.runningExternal",
      "shell.service.state.stopped",
    ],
  },
  {
    file: "apps/desktop/src/components/shell/AppSidebar.vue",
    template: "window.group.${section.id}",
    keys: ["window.group.core", "window.group.plugins"],
  },
  {
    file: "apps/desktop/src/views/PrintersPage.vue",
    template: "business.printPdf.status.${status}",
    keys: [
      "business.printPdf.status.queued",
      "business.printPdf.status.printing",
      "business.printPdf.status.done",
      "business.printPdf.status.failed",
      "business.printPdf.status.cancelled",
    ],
  },
  {
    file: "apps/desktop/src/views/OverviewPage.vue",
    template: "plugins.recovery.${id}",
    keys: [
      "plugins.recovery.records",
      "plugins.recovery.observability",
    ],
  },
  {
    file: "apps/desktop/src/views/PluginDisabled.vue",
    template: "plugins.${props.pluginId}.name",
    keys: [
      "plugins.records.name",
      "plugins.observability.name",
    ],
  },
  {
    file: "apps/desktop/src/views/PluginDisabled.vue",
    template: "plugins.disabled.state.${pluginState}",
    keys: [
      "plugins.disabled.state.pending",
      "plugins.disabled.state.incompatible",
      "plugins.disabled.state.broken",
      "plugins.disabled.state.invalid",
    ],
  },
  {
    file: "apps/desktop/src/views/PrintersPage.vue",
    template: "business.printers.kind.${kind}",
    keys: ["business.printers.kind.named", "business.printers.kind.custom"],
  },
  {
    file: "apps/desktop/src/views/PluginsPage.vue",
    template: "plugins.bridge.${state}",
    keys: [
      "plugins.bridge.allowed",
      "plugins.bridge.denied",
      "plugins.bridge.unknown",
    ],
  },
];

for (const file of sourceFiles) {
  const text = readFileSync(file, "utf8");
  for (const match of text.matchAll(/\{\{([a-zA-Z0-9_.-]+)\}\}/g)) {
    checkKey(match[1], file);
  }
  for (const match of text.matchAll(/PK\.t\("([a-zA-Z0-9_.-]+)"/g)) {
    checkKey(match[1], file);
  }
  for (const match of text.matchAll(/\bt\(\s*["'`]([a-z][A-Za-z0-9_-]*\.[A-Za-z0-9_.-]+)["'`]/g)) {
    checkKey(match[1], file);
  }
  for (const match of text.matchAll(/\bt\(\s*`([^`]+)`/g)) {
    if (!match[1].includes("${")) continue;
    const entry = knownDynamic.find(
      (candidate) => file.endsWith(candidate.file) && candidate.template === match[1],
    );
    if (!entry) {
      problems.push(`${file}: dynamic key template ${match[1]} is not in knownDynamic`);
      continue;
    }
    for (const key of entry.keys) checkKey(key, file);
  }
  // Page tables keep their catalogue key in data instead of a t() call.
  for (const match of text.matchAll(/["'`]([a-z][A-Za-z0-9_-]*\.[A-Za-z0-9_.-]+)["'`]/g)) {
    if (en.messages[match[1]]) used.add(match[1]);
  }
}

// The desktop UI never asks for a credential: the service logs itself in with
// its 0600 admin token, and a one-time D5 token is displayed as text only.
for (const file of sourceFiles) {
  const lines = readFileSync(file, "utf8").split("\n");
  for (const [index, line] of lines.entries()) {
    if (/type\s*=\s*['"`]password['"`]/u.test(line)) {
      problems.push(`${file}:${index + 1}: the UI must not render a password input`);
    }
    if (/v-model\s*=\s*['"`][^'"`]*[Tt]oken/u.test(line)) {
      problems.push(`${file}:${index + 1}: the UI must not bind a token input`);
    }
    if (/admin-token/u.test(line)) {
      problems.push(`${file}:${index + 1}: the automatic credential must not reach the UI`);
    }
  }
}

const coreSource = readFileSync(join(here, "../src/core.js"), "utf8");
for (const match of coreSource.matchAll(/"(error\.[A-Za-z0-9_.]+|records\.statusValue\.[A-Za-z0-9_.]+)"/g)) {
  checkKey(match[1], "core.js");
}

const codeSource = readFileSync(join(root, "crates/protocol/src/code.rs"), "utf8");
const protocolCodes = [...codeSource.matchAll(/rename = "(E_[A-Z_]+)"/g)].map((match) => match[1]);
for (const code of protocolCodes) {
  const mapping = `${code}: "error.code.${code}.title"`;
  if (!coreSource.includes(mapping)) problems.push(`core.js does not map ${code}`);
}

// Keys consumed outside the Vue source scan: the Rust tray reads tray.*, and
// the service returns plugins.<id>.name as a builtin nameKey.
const intentionalUnused = new Set([
  "tray.appQuit",
  "tray.quit",
  "tray.quitKeep",
  "tray.show",
  "tray.start",
  "tray.stop",
  "tray.tooltip",
]);
const unused = enKeys.filter((key) => !used.has(key) && !intentionalUnused.has(key));
if (unused.length > 0) {
  warnings.push(`unused keys (fine while a surface is still unported): ${unused.join(", ")}`);
}

if (problems.length > 0) {
  for (const problem of problems) console.error(`error: ${problem}`);
  process.exit(1);
}
console.log(`i18n catalogue ok: ${enKeys.length} keys, ${protocolCodes.length} protocol codes`);
for (const warning of warnings) console.warn(`warning: ${warning}`);
