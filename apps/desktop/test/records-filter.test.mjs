import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { logResultKey, visibleLogColumns } from "../src/components/businessColumns.js";
import { recordsRequestable, todayMetrics } from "../src/overviewData.js";

const here = dirname(fileURLToPath(import.meta.url));
const source = (path) => readFileSync(join(here, "..", path), "utf8");

test("records summary and list share the active filter query", () => {
  const records = source("src/views/BusinessRecords.vue");
  const api = source("src/api.ts");
  assert.match(api, /recordsSummary\(query: RecordsWindow = \{\}\)/u);
  assert.match(api, /status\?: string/u);
  assert.match(api, /printer\?: string/u);
  assert.match(api, /tags\?: string\[\]/u);
  assert.match(records, /recordsSummary\(query\)/u);
  assert.match(records, /recordsList\(\{ \.\.\.query, limit: 50 \}\)/u);
  assert.doesNotMatch(
    records,
    /loading\.value = true;[\s\S]{0,120}rows\.value = \[\]/u,
    "a refresh keeps the visible rows",
  );
  assert.match(records, /summary\.value = nextSummary;[\s\S]*?rows\.value = list\.records/u);
  assert.match(records, /watch\(dataDown/u);
});

test("records loadMore rejects stale and unavailable pages", () => {
  const records = source("src/views/BusinessRecords.vue");
  const loadMore = records.slice(records.indexOf("async function loadMore"), records.indexOf("async function exportCsv"));
  assert.match(loadMore, /disabled\.value \|\| dataDown\.value \|\| loadingMore\.value/u);
  assert.match(loadMore, /const run = generation/u);
  assert.match(loadMore, /const cursor = nextCursor\.value/u);
  assert.match(loadMore, /run !== generation \|\| disabled\.value \|\| dataDown\.value/u);
  assert.match(records, /:disabled="loading \|\| dataDown"/u);
});

test("records KPIs and tables keep the failure row inline", () => {
  const records = source("src/views/BusinessRecords.vue");
  const css = source("src/styles/components.css");
  assert.match(records, /summary\?\.todayDone/u);
  assert.match(records, /records\.unit\.prints/u);
  assert.match(records, /kpi-strip records-kpi/u);
  assert.match(records, /grid-template-columns: repeat\(5, minmax\(0, 1fr\)\)/u);
  assert.match(css, /\.data-table \.el-table__cell \{[\s\S]*?white-space: nowrap/u);
});


test("overview binds the four today fields and uses the 900px fallback", () => {
  const overview = source("src/views/OverviewPage.vue");
  const types = source("src/types.ts");
  assert.match(overview, /todayMetrics\(summary\.value\)/u);
  assert.match(overview, /summary\?\.todayDone/u);
  assert.match(overview, /today\.todayDone/u);
  assert.match(overview, /today\.todayFailed/u);
  assert.match(overview, /today\.todayCancelled/u);
  for (const field of ["todaySheets", "todayDone", "todayFailed", "todayCancelled"]) {
    assert.match(types, new RegExp(`\\b${field}: number`, "u"));
  }
  const breakpoint = source("src/composables/useBreakpoint.ts");
  assert.match(breakpoint, /width > 0 \? width : 900/u);
});

test("overview today metrics zero safely and respect service gating", () => {
  assert.deepEqual(todayMetrics({ todayDoneSheets: 3, todayDone: 4, todayFailed: 2, todayCancelled: 0 }), {
    todayDone: 4,
    todayFailed: 2,
    todayCancelled: 0,
  });
  assert.deepEqual(todayMetrics(null), {
    todayDone: 0,
    todayFailed: 0,
    todayCancelled: 0,
  });
  assert.equal(recordsRequestable(true, true), false);
  assert.equal(recordsRequestable(false, false), false);
  assert.equal(recordsRequestable(false, true), true);
});

test("logs result mapping and column collapse are pure behavior", () => {
  assert.equal(logResultKey(true), "logs.result.ok");
  assert.equal(logResultKey(false), "logs.result.failed");
  assert.deepEqual(visibleLogColumns(900), { seq: false, stage: true, time: true, trace: true, result: true, duration: true });
  assert.deepEqual(visibleLogColumns(600), { seq: false, stage: false, time: true, trace: true, result: true, duration: true });
});

test("records odd-card override stays production layout", () => {
  const records = source("src/views/BusinessRecords.vue");
  assert.match(records, /\.records-kpi \.kpi-item:last-child:nth-child\(odd\) \{[\s\S]*?grid-column: auto/u);
  assert.match(records, /grid-template-columns: repeat\(5, minmax\(0, 1fr\)\)/u);
});
