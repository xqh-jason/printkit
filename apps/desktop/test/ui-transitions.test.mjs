import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = (path) => readFileSync(join(here, "..", path), "utf8");

test("page navigation uses an out-in transition with no stacked layers", () => {
  const shell = source("src/views/BusinessShell.vue");
  assert.match(shell, /<Transition name="pk-page" mode="out-in">/u);
  assert.match(shell, /<KeepAlive v-if="!optional" :max="4">/u);
  assert.match(shell, /:key="current"/u);
  // The old page is removed before the new one enters.
  assert.doesNotMatch(shell, /<Transition[^>]*mode="in-out"/u);
});

test("page motion is 120-180ms, transform+opacity only, and reduced", () => {
  const motion = source("src/styles/motion.css");
  const tokens = source("src/styles/tokens.css");
  assert.match(motion, /\.page\s*\{\s*animation: pk-page-in var\(--pk-motion-fast\)/u);
  assert.match(motion, /translateY\(5px\)/u);
  assert.match(motion, /translateY\(-4px\)/u, "leave moves 4px");
  assert.match(motion, /\.pk-page-enter-active/u);
  assert.match(motion, /\.pk-page-leave-active/u);
  assert.match(tokens, /--pk-motion-fast: 120ms/u);
  const reduce = motion.slice(motion.indexOf('data-motion="reduce"'));
  assert.match(reduce, /\.pk-page-enter-active/u);
  assert.match(reduce, /\.pk-page-leave-active/u);
  const media = motion.slice(motion.indexOf("prefers-reduced-motion: reduce"));
  assert.match(media, /\.pk-page-enter-active/u);
  assert.match(media, /\.el-icon\.is-loading/u, "reduced motion freezes the spinner");
});

test("skeletons appear only after a short delay but stay while slow", () => {
  const skeleton = source("src/components/Skeleton.vue");
  assert.match(skeleton, /delay: 130/u);
  assert.match(skeleton, /Math\.max\(0, props\.delay\)/u);
  assert.match(skeleton, /window\.setTimeout/u);
});

test("Overview keeps its previous snapshot during refresh and slow loads", () => {
  const overview = source("src/views/OverviewPage.vue");
  assert.doesNotMatch(overview, /resetData\(\)/u);
  assert.match(overview, /const hasData = computed/u);
  assert.match(overview, /loading && !hasData/u);
  assert.match(overview, /\(hasData \|\| !loading\)/u);
  assert.match(overview, /error\.value = errorText\(describeError\(reason\)\)/u);
});

test("Records refresh replaces rows only after the new page arrives", () => {
  const records = source("src/views/BusinessRecords.vue");
  assert.doesNotMatch(
    records,
    /loading\.value = true;[\s\S]{0,120}rows\.value = \[\]/u,
  );
  assert.match(records, /summary\.value = nextSummary;[\s\S]*?rows\.value = list\.records/u);
  assert.match(records, /<Skeleton v-if="showSkeleton"/u);
});

test("Plugins shows a delayed list skeleton instead of an empty flash", () => {
  const plugins = source("src/views/PluginsPage.vue");
  assert.match(plugins, /const initialized = ref\(false\)/u);
  assert.match(plugins, /initialized\.value = true/u);
  assert.match(plugins, /!initialized \|\| \(busy === 'list'/u);
  assert.match(plugins, /<Skeleton/u);
  assert.match(plugins, /variant="list"/u);
});

test("Jobs and Logs use the shared skeleton only on the first load", () => {
  const jobs = source("src/views/DevJobs.vue");
  const logs = source("src/views/DevLogs.vue");
  assert.match(jobs, /<Skeleton\s+v-if="initialLoading"/u);
  assert.match(logs, /<Skeleton\s+v-if="loading && rows\.length === 0"/u);
});

test("action buttons expose aria-busy while keeping their loading prop", () => {
  for (const path of [
    "src/views/OverviewPage.vue",
    "src/views/PrintersPage.vue",
    "src/views/BusinessRecords.vue",
    "src/views/PluginsPage.vue",
    "src/views/SettingsPage.vue",
  ]) {
    assert.match(source(path), /:aria-busy=/u, path);
  }
});

test("Settings keeps its allowed origins when a refresh fails", () => {
  const settings = source("src/views/SettingsPage.vue");
  const load = settings.slice(settings.indexOf("async function loadOrigins"), settings.indexOf("async function addAllowedOrigin"));
  assert.doesNotMatch(load, /origins\.value = \[\]/u, "a failed refresh must not clear the list");
  assert.match(load, /originsError\.value = t\("business\.settings\.originsUnavailable"/u);
  assert.match(settings, /origins\.length > 0 \|\| \(!originsLoading && !originsError\)/u);
});

test("plugin navigation excludes removed plugins", () => {
  const pages = source("src/plugins/pluginPages.js");
  assert.doesNotMatch(pages, /templates|calibration|designer/iu);
  assert.match(pages, /id: "records"/u);
  assert.match(pages, /owner: "observability"/u);
});

test("loading buttons keep an icon slot so the spinner replaces it in place", () => {
  const settings = source("src/views/SettingsPage.vue");
  const printers = source("src/views/PrintersPage.vue");
  const overview = source("src/views/OverviewPage.vue");
  assert.match(settings, /:icon="Refresh" :loading="busy === 'config'"/u);
  assert.match(settings, /:icon="Check"\s*:loading="retentionBusy"/u);
  assert.match(printers, /:icon="PrinterIcon"\s*:loading="pdfBusy"/u);
  assert.match(printers, /:icon="VideoPlay" :loading="busy"/u);
  assert.match(overview, /:icon="SwitchButton"\s*:loading="busy === 'stop'"/u);
  // No per-button fixed width workaround.
  assert.doesNotMatch(settings, /width: configButtonWidth/u);
});
