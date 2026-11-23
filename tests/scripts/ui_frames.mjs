#!/usr/bin/env node
/*
 * Visual evidence for page transitions and loading states.
 *
 * Requires a prebuilt apps/desktop/dist and an existing Playwright install
 * (no package is added to the repo):
 *
 *   DIST_DIR=/path/to/apps/desktop/dist \
 *   PLAYWRIGHT_MODULE=/path/to/node_modules/playwright/index.mjs \
 *   node tests/scripts/ui_frames.mjs
 *
 * Frames are written to _work/evidence/ui-frames (ignored); the JSON summary
 * printed to stdout is what the review records.
 */
import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../..");
const dist = resolve(process.env.DIST_DIR ?? join(root, "apps/desktop/dist"));
const out = resolve(process.env.UI_FRAME_OUT ?? join(root, "_work/evidence/ui-frames"));
const playwrightModule = process.env.PLAYWRIGHT_MODULE ?? "playwright";
const { chromium } = await import(playwrightModule);

const port = Number(process.env.UI_FRAME_PORT ?? 4177);
const base = `http://127.0.0.1:${port}`;
mkdirSync(out, { recursive: true });
const server = spawn("python3", ["-m", "http.server", String(port), "-d", dist], {
  stdio: "ignore",
});
await new Promise((resolveReady, reject) => {
  const started = Date.now();
  const poll = async () => {
    try {
      const response = await fetch(base + "/index.html");
      if (response.ok) return resolveReady();
    } catch {}
    if (Date.now() - started > 10000) return reject(new Error("static server did not start"));
    setTimeout(poll, 100);
  };
  void poll();
});

const plugins = [
  { id: "observability", name: "Observability", version: "1.0.0", kind: "builtin", state: "on", chosen: true, source: "builtin", requires: [], requiredBy: [], dataCount: 1200 },
  { id: "records", name: "Records", version: "1.0.0", kind: "builtin", state: "on", chosen: true, source: "builtin", requires: [], requiredBy: [], dataCount: 840 },
];
const record = (i, status, errorCode, template, traceId, copies) => ({
  id: i, seq: 100 + i, at: `2026-11-22T09:${String(10 + i).padStart(2, "0")}:00Z`,
  origin: "business", template, tags: i % 2 ? ["label"] : ["invoice"], printer: "Office LaserJet",
  pages: 2, copies, status, durMs: 180 + i * 37, errorCode, traceId,
});
const records = [
  record(1, "failed", "E_PRINTER_OFFLINE", "shipping-label", "4f9c2a7d1b8e", 1),
  record(2, "done", undefined, "invoice-a4", "7d1e5c93a204", 3),
  record(3, "cancelled", "E_CANCELLED", "shipping-label", "2b8f6d40c915", 1),
  record(4, "done", undefined, "invoice-a4", "9a3c7e15d682", 2),
];
const summary = { todaySheets: 12, todayDone: 8, todayFailed: 1, todayCancelled: 1, total: 4, failed: 1, cancelled: 1, pending: 0, avgDurMs: 236, slowestPrinter: "Office LaserJet", slowestAvgDurMs: 210 };
const statsDay = [{ key: "2026-11-22", count: 4, sheets: 12, avgDurMs: 236 }, { key: "2026-11-21", count: 3, sheets: 8, avgDurMs: 212 }];
const statsPrinter = [{ key: "Office LaserJet", count: 4, sheets: 12, avgDurMs: 236 }];
const printers = [{ name: "Office LaserJet", isDefault: true, raw: true, duplex: true, paperSource: true, mimeTypes: ["application/pdf"] }];
const retention = { days: 30, source: "config", logDays: 30, logSource: "config", recordsDays: 30, recordsSource: "config", restartRequired: false };

function mockInit({ state = "runningOwned", reachable = true, pending = [], plugins, records, summary, statsDay, statsPrinter, printers, retention }) {
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "business" }, currentWebview: { label: "business" } },
    transformCallback() { return 1; },
    unregisterCallback() {},
    convertFileSrc(path) { return path; },
    async invoke(cmd, args) {
      if (pending.includes(cmd)) return new Promise(() => {});
      if (cmd === "snapshot") return {
        sidecar: { version: "0.2.1", target: "aarch64-apple-darwin", compatible: true, message: "" },
        config: { dataDir: join(homedir(), "Library", "Application Support", "printkit"), port: 19080, json: {} },
        configError: null,
        status: { state, pid: 4821, port: 19080, reachable, lockPath: "/tmp/printkit.lock", staleLock: false, detail: null },
        autoStart: { state: "started", error: null },
        plugins, pluginsError: null, pluginsStale: false, pluginsReachable: true, pluginsPreset: "all", pluginsBroken: null,
        cliPath: "/Applications/printkit.app/Contents/MacOS/printkit",
      };
      if (cmd === "plugin_list") return { plugins, preset: "all", stale: false, reachable: true };
      if (cmd === "list_printers") return printers;
      if (cmd === "list_papers") return [{ name: "A4", widthMm: 210, heightMm: 297, kind: "named" }];
      if (cmd === "records_summary") return summary;
      if (cmd === "records_list") return { records, nextCursor: null };
      if (cmd === "records_stats") return args?.groupBy === "printer" ? statsPrinter : statsDay;
      if (cmd === "get_retention_days") return retention;
      if (cmd === "origins_list") return { origins: ["https://app.example.com"] };
      return null;
    },
  };
  localStorage.setItem("printkit.locale", "en");
}

const browser = await chromium.launch();
const baseData = { plugins, records, summary, statsDay, statsPrinter, printers, retention };
const metrics = {};

async function openContext(options = {}) {
  const context = await browser.newContext({
    viewport: { width: 900, height: 1000 },
    colorScheme: options.colorScheme ?? "light",
    reducedMotion: options.reducedMotion,
  });
  await context.addInitScript(mockInit, { ...baseData, state: options.state ?? "runningOwned", pending: options.pending ?? [] });
  const page = await context.newPage();
  const session = await context.newCDPSession(page);
  await session.send("Network.setCacheDisabled", { cacheDisabled: true });
  await page.goto(base + "/", { waitUntil: "networkidle" });
  await page.waitForSelector('[data-page="overview"]', { timeout: 15000 });
  return { context, page };
}

async function settleOverview(page) {
  await page.click('[data-page="overview"]');
  await page.waitForTimeout(400);
}

async function pageState(page) {
  return page.evaluate(() => {
    const pages = [...document.querySelectorAll(".page")];
    return {
      pageCount: pages.length,
      classes: pages.map((node) => node.className),
      opacity: pages.map((node) => getComputedStyle(node).opacity),
      skeletons: document.querySelectorAll(".pk-skeleton").length,
      activeNav: document.querySelector("[aria-current='page']")?.getAttribute("data-page") ?? null,
    };
  });
}

async function frames(target, prefix) {
  const { context, page } = await openContext();
  await page.waitForTimeout(1200);
  await settleOverview(page);
  await page.click(`[data-page="${target}"]`);
  const start = await pageState(page);
  await page.screenshot({ path: join(out, `${prefix}-0-start.png`) });
  await page.waitForTimeout(70);
  const mid = await pageState(page);
  await page.screenshot({ path: join(out, `${prefix}-1-mid.png`) });
  await page.waitForTimeout(220);
  const end = await pageState(page);
  await page.screenshot({ path: join(out, `${prefix}-2-end.png`) });
  metrics[prefix] = { start, mid, end };
  await context.close();
}

await frames("settings", "overview-to-settings");
await frames("plugins", "overview-to-plugins");

async function skeletonAppearance(name, target, pending, ready) {
  const { context, page } = await openContext({ pending });
  if (target !== "overview") await page.click(`[data-page="${target}"]`);
  await page.waitForSelector(ready, { timeout: 10000 });
  const started = Date.now();
  let skeletonMs = null;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if ((await pageState(page)).skeletons > 0) {
      skeletonMs = Date.now() - started;
      break;
    }
    await page.waitForTimeout(10);
  }
  metrics[name] = { readySelector: ready, skeletonMs, state: await pageState(page) };
  await page.screenshot({ path: join(out, `${name}.png`) });
  await context.close();
}
async function loadingShot(name) {
  const { context, page } = await openContext({ pending: ["records_summary"] });
  await page.waitForTimeout(240);
  metrics[name] = await pageState(page);
  await page.screenshot({ path: join(out, `${name}.png`) });
  await context.close();
}
await loadingShot("overview-loading");
await skeletonAppearance("records-loading", "records", ["records_summary"], ".records-kpi");
await skeletonAppearance("plugins-loading", "plugins", ["plugin_list"], ".plugin-toolbar");

const { context: ext, page: ep } = await openContext({ state: "runningExternal" });
await ep.waitForTimeout(1000);
await ep.click('[data-page="printers"]');
await ep.waitForTimeout(500);
metrics["printers-external"] = await pageState(ep);
await ep.screenshot({ path: join(out, "printers-external.png") });
await ext.close();

const { context: rm, page: rmp } = await openContext({ reducedMotion: "reduce" });
await rmp.waitForTimeout(1000);
await settleOverview(rmp);
await rmp.click('[data-page="settings"]');
await rmp.waitForTimeout(800);
metrics["reduced-motion"] = await pageState(rmp);
await rmp.screenshot({ path: join(out, "overview-to-settings-reduced.png") });
await rm.close();

const { context: btn, page: bp } = await openContext({ pending: ["refresh_config"] });
await bp.waitForTimeout(900);
await bp.click('[data-page="settings"]');
await bp.waitForTimeout(400);
const refresh = bp.getByRole("button", { name: /refresh config/i }).first();
const before = await refresh.evaluate((node) => ({
  width: node.getBoundingClientRect().width,
  height: node.getBoundingClientRect().height,
  padding: getComputedStyle(node).padding,
  children: [...node.children].map((c) => ({ tag: c.tagName, cls: c.className, width: c.getBoundingClientRect().width, pos: getComputedStyle(c).position })),
}));
await refresh.click();
await bp.waitForTimeout(80);
const after = await refresh.evaluate((node) => ({
  width: node.getBoundingClientRect().width,
  height: node.getBoundingClientRect().height,
  busy: node.getAttribute("aria-busy"),
  disabled: node.disabled,
  children: [...node.children].map((c) => ({ tag: c.tagName, cls: c.className, width: c.getBoundingClientRect().width, pos: getComputedStyle(c).position })),
}));
metrics["button-loading"] = { before, after };
await bp.screenshot({ path: join(out, "settings-button-loading.png") });
await btn.close();


async function loadingButtonShot(name, target, pending, buttonName, setup) {
  const { context, page } = await openContext({ pending });
  await page.waitForTimeout(600);
  await page.click(`[data-page="${target}"]`);
  await page.waitForTimeout(500);
  if (setup) await setup(page);
  const button = page.getByRole("button", { name: buttonName }).first();
  const before = await button.evaluate((node) => ({ width: node.getBoundingClientRect().width, height: node.getBoundingClientRect().height }));
  await button.click();
  const confirm = page.locator(".el-message-box__btns .el-button--primary");
  if (await confirm.count()) {
    await confirm.first().click();
  }
  await page.waitForTimeout(120);
  const after = await button.evaluate((node) => ({
    width: node.getBoundingClientRect().width,
    height: node.getBoundingClientRect().height,
    busy: node.getAttribute("aria-busy"),
    disabled: node.disabled,
  }));
  metrics[name] = { before, after };
  await page.screenshot({ path: join(out, `${name}.png`) });
  await context.close();
}

await loadingButtonShot("stop-service-loading", "overview", ["stop_service"], /stop service/i);
await loadingButtonShot("settings-save-loading", "settings", ["set_retention_days"], /save/i, async (page) => {
  const plus = page.locator(".retention-card .el-input-number__increase").first();
  if (await plus.count()) await plus.click();
});
await loadingButtonShot("print-pdf-loading", "printers", ["print_pdf_bytes"], /print pdf/i, async (page) => {
  await page.setInputFiles('input[type="file"]', {
    name: "frame.pdf",
    mimeType: "application/pdf",
    buffer: Buffer.from("%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n"),
  });
  await page.waitForTimeout(200);
});

await browser.close();
server.kill("SIGTERM");
writeFileSync(join(out, "frames.json"), JSON.stringify(metrics, null, 2));
console.log(JSON.stringify(metrics, null, 2));
