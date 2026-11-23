import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { resolveEmptyState, visibleLogColumns, visibleRecordColumns } from "../src/components/businessColumns.js";
import {
  failureRate,
  fillDaySeries,
  overviewWindow,
  recordsRequestable,
  statusDistribution,
  successRate,
  topPrinters,
} from "../src/overviewData.js";
import { LOOPBACK_HOST, serviceAddress } from "../src/sidebarAddress.js";

import { bridgePermission, bridgePermissions } from "../src/plugins/pluginBridge.js";

const here = dirname(fileURLToPath(import.meta.url));

function source(path) {
  return readFileSync(join(here, "..", path), "utf8");
}

test("record columns drop secondary fields below 760 and 560", () => {
  assert.deepEqual(visibleRecordColumns(1440), {
    narrow: false,
    tiny: false,
    printer: true,
    copies: true,
    pages: true,
    error: true,
    template: true,
    trace: true,
    duration: true,
  });
  assert.deepEqual(visibleRecordColumns(960), {
    narrow: false,
    tiny: false,
    printer: true,
    copies: true,
    pages: true,
    error: true,
    template: true,
    trace: true,
    duration: false,
  });
  const nineHundred = visibleRecordColumns(900);
  assert.equal(nineHundred.copies, true);
  assert.equal(nineHundred.error, true);
  assert.equal(nineHundred.template, true);
  assert.equal(nineHundred.trace, true);
  assert.equal(nineHundred.duration, false);
  const medium = visibleRecordColumns(759);
  assert.equal(medium.narrow, true);
  assert.equal(medium.printer, false);
  assert.equal(medium.copies, false);
  assert.equal(medium.pages, true);
  assert.equal(medium.tiny, false);
  const tiny = visibleRecordColumns(559);
  assert.equal(tiny.tiny, true);
  assert.equal(tiny.pages, false);
  assert.equal(visibleRecordColumns(760).printer, true);
  const desktop = visibleRecordColumns(900);
  assert.equal(desktop.copies, true);
  assert.equal(desktop.pages, true);
  assert.equal(desktop.printer, true);
});
test("table columns fit at 900 and collapse at 560", () => {
  assert.equal(visibleLogColumns(900).seq, false);
  assert.equal(visibleLogColumns(900).stage, true);
  assert.equal(visibleLogColumns(600).stage, false);
  assert.equal(visibleLogColumns(560).stage, false);
});
test("sidebar address uses only a reported loopback port and separates unreachable", () => {
  assert.equal(LOOPBACK_HOST, "127.0.0.1");
  const config = { port: 19080 };
  const owned = { state: "runningOwned", reachable: true, port: 19080 };
  assert.deepEqual(serviceAddress(owned, config), {
    state: "owned",
    running: true,
    unreachable: false,
    address: "127.0.0.1:19080",
    livePort: 19080,
    configuredPort: 19080,
  });
  assert.equal(serviceAddress({ ...owned, state: "runningExternal" }, config).state, "external");
  const stopped = serviceAddress({ state: "stopped", reachable: false, port: null }, config);
  assert.equal(stopped.state, "stopped");
  assert.equal(stopped.running, false);
  assert.equal(stopped.address, null);
  assert.equal(stopped.configuredPort, 19080);
  const unreachable = serviceAddress({ ...owned, reachable: false }, config);
  assert.equal(unreachable.state, "unreachable");
  assert.equal(unreachable.unreachable, true);
  assert.equal(unreachable.running, false);
  assert.equal(unreachable.address, null);
  const noReportedPort = serviceAddress({ ...owned, port: null }, config);
  assert.equal(noReportedPort.state, "no-port");
  assert.equal(noReportedPort.running, true);
  assert.equal(noReportedPort.address, null);
  assert.equal(noReportedPort.configuredPort, 19080);
});

test("business pages keep the port in the sidebar footer only", () => {
  const overview = source("src/views/OverviewPage.vue");
  const settings = source("src/views/SettingsPage.vue");
  const footer = source("src/components/shell/SidebarAddress.vue");
  assert.doesNotMatch(overview, /portText|compact-port|business\.settings\.port|shell\.service\.port/u);
  assert.doesNotMatch(settings, /business\.settings\.port/u);
  assert.match(footer, /serviceAddress\(props\.snapshot\?\.status, props\.snapshot\?\.config\)/u);
  assert.match(footer, /shell\.footer\.configuredPort/u);
});

test("overview window is six days ago through tomorrow, local and exclusive", () => {
  const now = new Date(2026, 10, 18, 12, 0, 0);
  const range = overviewWindow(now);
  assert.deepEqual(range.days, [
    "2026-11-12",
    "2026-11-13",
    "2026-11-14",
    "2026-11-15",
    "2026-11-16",
    "2026-11-17",
    "2026-11-18",
  ]);
  assert.match(range.from, /^2026-11-12T00:00:00[+-]\d{2}:\d{2}$/u);
  assert.match(range.to, /^2026-11-19T00:00:00[+-]\d{2}:\d{2}$/u);
});

test("day buckets sort by date and fill missing days with real zero", () => {
  const days = ["2026-11-12", "2026-11-13", "2026-11-14"];
  const series = fillDaySeries(
    [
      { key: "2026-11-14", count: 3, sheets: 6, avgDurMs: 20 },
      { key: "2026-11-12", count: 1, sheets: 2, avgDurMs: 10 },
    ],
    days,
  );
  assert.deepEqual(series, [
    { key: "2026-11-12", count: 1, sheets: 2, avgDurMs: 10 },
    { key: "2026-11-13", count: 0, sheets: 0, avgDurMs: 0 },
    { key: "2026-11-14", count: 3, sheets: 6, avgDurMs: 20 },
  ]);
});

test("duplicate day buckets use a count-weighted average", () => {
  const series = fillDaySeries(
    [
      { key: "2026-11-12", count: 2, sheets: 3, avgDurMs: 10 },
      { key: "2026-11-12", count: 3, sheets: 5, avgDurMs: 20 },
    ],
    ["2026-11-12"],
  );
  assert.deepEqual(series, [
    { key: "2026-11-12", count: 5, sheets: 8, avgDurMs: 16 },
  ]);
});

test("status split subtracts pending and never invents cancelled zero", () => {
  assert.deepEqual(statusDistribution({ total: 10, failed: 2, cancelled: 1, pending: 2 }), {
    total: 10,
    done: 5,
    failed: 2,
    cancelled: 1,
    cancelledKnown: true,
  });
  assert.deepEqual(statusDistribution({ total: 10, failed: 2, cancelled: 1 }), {
    total: 10,
    done: 7,
    failed: 2,
    cancelled: 1,
    cancelledKnown: true,
  });
  assert.deepEqual(statusDistribution({ total: 10, failed: 2, cancelled: null }), {
    total: 10,
    done: 8,
    failed: 2,
    cancelled: null,
    cancelledKnown: false,
  });
  assert.equal(failureRate({ total: 0, failed: 0 }), null);
  assert.equal(failureRate({ total: 10, failed: 2 }), null);
  assert.equal(failureRate({ total: 10, failed: 2, cancelled: 1 }), 2 / 9);
  assert.equal(failureRate({ total: 10, failed: 2, cancelled: 1, pending: 2 }), 2 / 7);
  assert.equal(successRate({ total: 10, failed: 2, cancelled: 1, pending: 2 }), 71);
  assert.equal(successRate({ total: 3, failed: 0, cancelled: 2, pending: 1 }), null);
  assert.equal(successRate({ total: 4, failed: 1, cancelled: 0, pending: 0 }), 75);
  assert.equal(failureRate({ total: 10, failed: 2, cancelled: 3 }), 2 / 7);
  assert.equal(failureRate({ total: 3, failed: 0, cancelled: 3 }), null);
});

test("printer Top 5 ranks by task count and keeps five entries", () => {
  const printers = topPrinters(
    [
      { key: "d", count: 2, sheets: 4, avgDurMs: 3 },
      { key: "a", count: 2, sheets: 9, avgDurMs: 2 },
      { key: "default", count: 4, sheets: 12, avgDurMs: 1 },
      { key: "e", count: 1, sheets: 1, avgDurMs: 4 },
      { key: "c", count: 3, sheets: 5, avgDurMs: 5 },
      { key: "b", count: 1, sheets: 5, avgDurMs: 6 },
    ],
    5,
  );
  assert.deepEqual(
    printers.map((printer) => printer.key),
    ["default", "c", "a", "d", "b"],
  );
  assert.deepEqual(topPrinters([], 5), []);
});

test("records requests require both a running service and records on", () => {
  assert.equal(recordsRequestable(true, true), false);
  assert.equal(recordsRequestable(false, false), false);
  assert.equal(recordsRequestable(false, true), true);
});

test("empty states distinguish empty data from an empty filter", () => {
  assert.equal(resolveEmptyState({ loading: true, count: 0, hasFilters: false }), "loading");
  assert.equal(resolveEmptyState({ loading: false, count: 0, hasFilters: false }), "empty");
  assert.equal(resolveEmptyState({ loading: false, count: 0, hasFilters: true }), "noMatch");
  assert.equal(resolveEmptyState({ loading: false, count: 2, hasFilters: true }), "rows");
});

test("bridge permissions distinguish true, false and undeclared", () => {
  assert.equal(bridgePermission(true), "allowed");
  assert.equal(bridgePermission(false), "denied");
  assert.equal(bridgePermission(undefined), "unknown");
  assert.deepEqual(
    bridgePermissions({ selfConfig: true, printers: false }),
    { selfConfig: "allowed", printers: "denied", records: "unknown" },
  );
  assert.deepEqual(bridgePermissions(undefined), {
    selfConfig: "unknown",
    printers: "unknown",
    records: "unknown",
  });
});

test("printers can print an existing PDF through base64 bytes", () => {
  const page = readFileSync(join(here, "..", "src/views/PrintersPage.vue"), "utf8");
  assert.match(page, /printPdfBytes/u);
  assert.match(page, /type="file"/u);
  assert.match(page, /accept="application\/pdf,\.pdf"/u);
  assert.match(page, /business\.printPdf\.run/u);
  const api = readFileSync(join(here, "..", "src/api.ts"), "utf8");
  assert.match(api, /invoke\("print_pdf_bytes"/u);
});

test("settings manages allowed web origins without tokens", () => {
  const page = readFileSync(join(here, "..", "src/views/SettingsPage.vue"), "utf8");
  assert.match(page, /listOrigins/u);
  assert.match(page, /addOrigin/u);
  assert.match(page, /removeOrigin/u);
  assert.match(page, /originRemoveConfirm/u);
  const api = readFileSync(join(here, "..", "src/api.ts"), "utf8");
  assert.match(api, /invoke<OriginsReply>\("origins_list"\)/u);
  assert.match(api, /invoke<OriginsReply>\("origin_add"/u);
  assert.match(api, /invoke<OriginsReply>\("origin_remove"/u);
});

test("records expose a detail drawer and an honest reprint message", () => {
  const page = readFileSync(join(here, "..", "src/views/BusinessRecords.vue"), "utf8");
  assert.match(page, /business\.records\.detail/u);
  assert.match(page, /copyRecord/u);
  assert.match(page, /business\.records\.reprintUnavailable/u);
  assert.match(page, /detail\.errorCode/u);
  assert.match(page, /detail\.traceId/u);
  assert.match(page, /tagText\(detail\)/u);
});
