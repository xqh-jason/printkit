import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  STAGES,
  aggregateJobs,
  applyCaps,
  facetValues,
  filterJobs,
  followAfterScroll,
  formatMs,
  mergeEvents,
  nearBottom,
  orderJobs,
  rangeMinutes,
  serializeTrace,
  stageTimeline,
  traceNeighbors,
  waterfall,
} from "../src/components/observability/jobTraces.js";

const here = dirname(fileURLToPath(import.meta.url));

function event(seq, traceId, stage, options = {}) {
  return {
    seq,
    traceId,
    stage,
    at: options.at ?? "2026-11-12T03:00:00.000Z",
    ...(options.durMs === undefined ? {} : { durMs: options.durMs }),
    ok: options.ok ?? true,
    ...(options.code === undefined ? {} : { code: options.code }),
    ...(options.detail === undefined ? {} : { detail: options.detail }),
    ...(options.origin === undefined ? {} : { origin: options.origin }),
    ...(options.printer === undefined ? {} : { printer: options.printer }),
    ...(options.pages === undefined ? {} : { pages: options.pages }),
    ...(options.queuePos === undefined ? {} : { queuePos: options.queuePos }),
    fields: options.fields ?? {},
  };
}

test("jobs aggregate by traceId and keep only observed fields", () => {
  const events = [
    event(1, "trace-aaaa", "recv", { origin: "cli", at: "2026-11-12T03:00:00.100Z" }),
    event(2, "trace-bbbb", "recv", { origin: "web", printer: "Office", at: "2026-11-12T03:00:01Z" }),
    event(4, "trace-aaaa", "queue", {
      durMs: 300,
      queuePos: 4,
      at: "2026-11-12T03:00:00.300Z",
    }),
    event(7, "trace-bbbb", "render", {
      ok: false,
      code: "E_RENDER_CRASH",
      detail: "worker exited",
      at: "2026-11-12T03:00:01.200Z",
    }),
    event(6, "trace-aaaa", "result", {
      durMs: 900,
      pages: 3,
      printer: "Lab",
      fields: { copies: 2 },
      at: "2026-11-12T03:00:01Z",
    }),
  ];

  const jobs = aggregateJobs(events, 200);
  assert.equal(jobs.length, 2);
  assert.equal(jobs[0].traceId, "trace-bbbb", "newest trace (highest seq) comes first");

  const bbbb = jobs[0];
  assert.equal(bbbb.state, "failed");
  assert.equal(bbbb.code, "E_RENDER_CRASH");
  assert.equal(bbbb.currentStage, "render");
  assert.equal(bbbb.queuePos, undefined, "no queuePos is invented");
  assert.equal(bbbb.pages, undefined);
  assert.equal(bbbb.shortId, "trace-bb");
  assert.deepEqual(Object.keys(bbbb.extras), []);

  const aaaa = jobs[1];
  assert.equal(aaaa.state, "ok");
  assert.equal(aaaa.currentStage, "result");
  assert.equal(aaaa.totalMs, 900);
  assert.equal(aaaa.queuePos, 4);
  assert.equal(aaaa.pages, 3);
  assert.equal(aaaa.printer, "Lab");
  assert.equal(aaaa.extras.copies, 2);
  assert.equal(aaaa.endedAt, "2026-11-12T03:00:01Z");
  assert.equal(aaaa.byStage.submit, undefined);
});

test("the timeline has seven ordered slots and never fabricates a stage", () => {
  const jobs = aggregateJobs([
    event(1, "trace-a", "recv", { at: "2026-11-12T03:00:00Z" }),
    event(3, "trace-a", "render", { durMs: 250, at: "2026-11-12T03:00:00.400Z" }),
    event(7, "trace-a", "result", { durMs: 1200, at: "2026-11-12T03:00:01Z" }),
  ]);
  const slots = stageTimeline(jobs[0]);
  assert.equal(slots.length, 7);
  assert.deepEqual(
    slots.map((slot) => slot.stage),
    [...STAGES],
  );
  assert.equal(slots[0].event?.stage, "recv");
  assert.equal(slots[1].event, null);
  assert.equal(slots[2].event, null);
  assert.equal(slots[3].event, null);
  assert.equal(slots[4].event?.durMs, 250);
  assert.equal(slots[5].event, null);
  assert.equal(slots[6].event?.stage, "result");

  const failed = aggregateJobs([
    event(8, "trace-b", "render", {
      ok: false,
      code: "E_JOB_FAILED",
      at: "2026-11-12T03:00:00Z",
    }),
  ]);
  assert.equal(failed[0].state, "failed");
  assert.equal(failed[0].code, "E_JOB_FAILED");
  assert.equal(stageTimeline(failed[0])[4].event.ok, false);
});

test("waterfall scales stage durations and marks the unexplained remainder", () => {
  const jobs = aggregateJobs([
    event(1, "trace-a", "queue", { durMs: 300, queuePos: 2, at: "2026-11-12T03:00:00Z" }),
    event(2, "trace-a", "result", { durMs: 900, at: "2026-11-12T03:00:01Z" }),
  ]);
  const segments = waterfall(jobs[0]);
  assert.deepEqual(
    segments.map((segment) => segment.stage),
    ["queue", "other"],
    "result carries the total duration, not a stage segment",
  );
  assert.equal(segments[0].queuePos, 2);
  assert.equal(segments[1].ms, 600, "900 total minus 300 measured");
  const sum = segments.reduce((total, segment) => total + segment.share, 0);
  assert.ok(Math.abs(sum - 1) < 1e-9);
  assert.equal(formatMs(undefined), "—", "missing duration is not zero");
  assert.equal(formatMs(450), "450 ms");
  assert.equal(formatMs(2500), "2.5 s");
});

test("the tail drops events at or below the cursor and duplicate seq values", () => {
  const current = [event(1, "trace-a", "recv"), event(2, "trace-a", "auth")];
  const incoming = [
    event(2, "trace-a", "auth", { at: "2026-11-12T03:00:01Z" }),
    event(3, "trace-a", "validate"),
    event(3, "trace-a", "validate"),
    event(4, "trace-a", "queue"),
  ];
  const merged = mergeEvents(current, incoming, 2, { maxRows: 500, maxTraces: 200 });
  assert.deepEqual(
    merged.events.map((row) => row.seq),
    [1, 2, 3, 4],
  );
  assert.equal(merged.lastSeq, 4);
  assert.equal(merged.events.filter((row) => row.seq === 3).length, 1);

  const ignored = mergeEvents(current, [event(1, "trace-a", "recv"), event(2, "trace-a", "auth")], 2);
  assert.deepEqual(
    ignored.events.map((row) => row.seq),
    [1, 2],
    "existing content survives; only the incoming cursor overlap is dropped",
  );
});

test("caps keep whole traces and drop the oldest first", () => {
  const events = [];
  let seq = 0;
  for (let index = 0; index < 240; index += 1) {
    for (const stage of ["recv", "render", "result"]) {
      seq += 1;
      events.push(event(seq, `trace-${index}`, stage));
    }
  }
  const capped = applyCaps(events, { maxRows: 500, maxTraces: 200 });
  assert.ok(capped.length <= 500, `rows ${capped.length}`);
  const traces = new Set(capped.map((row) => row.traceId));
  assert.ok(traces.size <= 200, `traces ${traces.size}`);
  assert.ok(traces.has("trace-239"), "the newest trace stays");
  assert.ok(!traces.has("trace-0"), "the oldest trace is dropped");
  for (const traceId of traces) {
    assert.equal(capped.filter((row) => row.traceId === traceId).length, 3, `${traceId} is whole`);
  }
});

test("follow only re-arms at the bottom and stops beyond one screen", () => {
  assert.equal(nearBottom(0, 400, 400), true);
  assert.equal(nearBottom(0, 400, 900), false);
  assert.equal(followAfterScroll({ scrollTop: 0, clientHeight: 400, scrollHeight: 400, wasFollowing: true }), true);
  assert.equal(
    followAfterScroll({ scrollTop: 400, clientHeight: 400, scrollHeight: 900, wasFollowing: true }),
    true,
    "within one screen keeps following",
  );
  assert.equal(
    followAfterScroll({ scrollTop: 0, clientHeight: 400, scrollHeight: 1000, wasFollowing: true }),
    false,
    "more than one screen above stops following",
  );
  assert.equal(followAfterScroll({ scrollTop: 600, clientHeight: 400, scrollHeight: 1000, wasFollowing: false }), true);
  assert.equal(followAfterScroll({ scrollTop: 0, clientHeight: 400, scrollHeight: 500, wasFollowing: false }), false);
});

test("facets filter by stage, outcome, printer, origin, keyword and range", () => {
  const jobs = aggregateJobs([
    event(1, "trace-speed", "recv", { origin: "cli", printer: "Office", at: "2026-11-12T02:00:00Z" }),
    event(2, "trace-speed", "result", {
      durMs: 400,
      ok: true,
      at: "2026-11-12T02:00:00.400Z",
    }),
    event(3, "trace-crash", "render", {
      ok: false,
      code: "E_RENDER_CRASH",
      origin: "web",
      printer: "Lab",
      at: "2026-11-12T03:00:00Z",
    }),
  ]);

  assert.equal(filterJobs(jobs, { stages: ["render"] }).length, 1);
  assert.equal(filterJobs(jobs, { stages: ["recv", "queue"] }).length, 1);
  assert.equal(filterJobs(jobs, { outcome: "failed" })[0].traceId, "trace-crash");
  assert.equal(filterJobs(jobs, { printer: "Office" })[0].traceId, "trace-speed");
  assert.equal(filterJobs(jobs, { origin: "web" })[0].traceId, "trace-crash");
  assert.equal(filterJobs(jobs, { keyword: "E_RENDER" })[0].traceId, "trace-crash");
  assert.equal(filterJobs(jobs, { keyword: "speed" })[0].traceId, "trace-speed");
  assert.equal(
    filterJobs(jobs, { fromMs: Date.parse("2026-11-12T02:30:00Z") }).length,
    1,
    "only the newer trace survives the cutoff",
  );
  assert.equal(filterJobs(jobs, { printer: "Office", outcome: "failed" }).length, 0);

  const values = facetValues(jobs);
  assert.deepEqual(values.printers, ["Lab", "Office"]);
  assert.deepEqual(values.origins, ["cli", "web"]);
});

test("time ranges map to a bounded service window", () => {
  const now = Date.parse("2026-11-12T12:00:00Z");
  const pad = (value) => String(value).padStart(2, "0");
  const localInput = (ms) => {
    const date = new Date(ms);
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
      date.getHours(),
    )}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  };
  assert.equal(rangeMinutes("15", "", now), 15);
  assert.equal(rangeMinutes("60", "", now), 60);
  assert.equal(rangeMinutes("1440", "", now), 1440);
  assert.equal(rangeMinutes("custom", localInput(now - 120 * 60_000), now), 120);
  assert.equal(rangeMinutes("custom", "", now), null, "an empty start keeps the current window");
  assert.equal(
    rangeMinutes("custom", localInput(now + 60 * 60_000), now),
    null,
    "a future start is invalid",
  );
  assert.equal(rangeMinutes("bogus", "", now), null);
});

test("export serialises the same event count as the timeline", () => {
  const jobs = aggregateJobs([
    event(1, "trace-a", "recv"),
    event(2, "trace-a", "result", { durMs: 100 }),
  ]);
  const json = JSON.parse(serializeTrace(jobs[0], "json"));
  assert.equal(json.length, 2);
  const lines = serializeTrace(jobs[0], "jsonl").split("\n").filter(Boolean);
  assert.equal(lines.length, 2);
  assert.deepEqual(
    lines.map((line) => JSON.parse(line).seq),
    [1, 2],
  );
});

test("neighbours walk the newest-first order", () => {
  const jobs = aggregateJobs([
    event(1, "trace-old", "recv"),
    event(2, "trace-mid", "recv"),
    event(3, "trace-new", "recv"),
  ]);
  assert.deepEqual(traceNeighbors(jobs, "trace-mid"), { prev: "trace-new", next: "trace-old" });
  assert.deepEqual(traceNeighbors(jobs, "trace-new"), { prev: null, next: "trace-mid" });
  assert.deepEqual(traceNeighbors(jobs, "trace-missing"), { prev: null, next: null });
});

test("card order follows first observation and does not move on updates", () => {
  const initial = aggregateJobs([
    event(1, "trace-first", "recv"),
    event(2, "trace-second", "recv"),
  ]);
  assert.deepEqual(orderJobs(initial).map((job) => job.traceId), ["trace-first", "trace-second"]);

  const updated = aggregateJobs([
    event(1, "trace-first", "recv"),
    event(2, "trace-second", "recv"),
    event(3, "trace-first", "queue"),
    event(4, "trace-second", "render"),
  ]);
  assert.equal(updated[0].traceId, "trace-second", "aggregate still ranks by latest seq");
  assert.deepEqual(
    orderJobs(updated).map((job) => job.traceId),
    ["trace-first", "trace-second"],
    "polling updates must not reorder cards",
  );

  const appended = aggregateJobs([
    event(1, "trace-first", "recv"),
    event(2, "trace-second", "recv"),
    event(5, "trace-third", "recv"),
  ]);
  assert.deepEqual(orderJobs(appended).map((job) => job.traceId), [
    "trace-first",
    "trace-second",
    "trace-third",
  ]);
});

test("the jobs page has no live animation or remount path", () => {
  const jobs = readFileSync(join(here, "..", "src/views/DevJobs.vue"), "utf8");
  assert.doesNotMatch(jobs, /TransitionGroup|pk-enter-card|pk-breathe|CountUp/u);
  assert.match(jobs, /orderJobs/u);
  assert.match(jobs, /scrollToBottom\("auto"\)/u);
  assert.match(jobs, /initialLoading/u);
  assert.match(jobs, /:loading="initialLoading"/u);
  const card = readFileSync(
    join(here, "..", "src/components/observability/JobCard.vue"),
    "utf8",
  );
  assert.doesNotMatch(card, /pk-enter-card|CountUp|pk-breathe/u);
  assert.match(card, /\{\{ formatMs\(job\.totalMs\) \}\}/u);

  const timeline = readFileSync(
    join(here, "..", "src/components/observability/StageTimeline.vue"),
    "utf8",
  );
  assert.doesNotMatch(timeline, /pk-breathe/u);

  const live = readFileSync(
    join(here, "..", "src/components/observability/LiveBar.vue"),
    "utf8",
  );
  assert.doesNotMatch(live, /pk-enter-card/u);
  assert.match(live, /loading && rowCount === 0/u);

  const liveTail = readFileSync(
    join(here, "..", "src/composables/useLiveTail.ts"),
    "utf8",
  );
  assert.match(liveTail, /initialLoading/u);
  assert.match(liveTail, /attempted\.value = true/u);
  assert.match(liveTail, /attempted\.value = false/u);
  assert.match(liveTail, /updatedAt\.value = null/u);

  const css = readFileSync(join(here, "..", "src/styles/observability.css"), "utf8");
  assert.doesNotMatch(css, /@keyframes|animation:|transition:|\.job-enter|\.pk-breathe/u);

  const motion = readFileSync(join(here, "..", "src/styles/motion.css"), "utf8");
  assert.match(motion, /\.pk-breathe/u, "global pending-state pulse may be reused elsewhere");
  assert.match(motion, /\.page\s*\{\s*animation: pk-page-in/u, "one-time page fade remains");
  assert.match(motion, /data-motion="reduce"/u, "the shared reduce-motion switch remains");
  assert.match(motion, /prefers-reduced-motion: reduce/u);
});

test("the jobs page composition follows the S3 contract", () => {
  const window = readFileSync(join(here, "..", "src/plugins/pluginPages.js"), "utf8");
  assert.match(window, /id: "jobs"/u);
  assert.match(window, /DevJobs/u);
  const jobs = readFileSync(join(here, "..", "src/views/DevJobs.vue"), "utf8");
  assert.match(jobs, /useLiveTail/u);
  assert.match(jobs, /useJobTraces/u);
  assert.match(jobs, /filterJobs/u);
  const detail = readFileSync(
    join(here, "..", "src/components/observability/TraceDetail.vue"),
    "utf8",
  );
  assert.match(detail, /unregisteredCode/u);
  const css = readFileSync(join(here, "..", "src/styles/observability.css"), "utf8");
  assert.match(css, /backdrop-filter: saturate\(1\.6\) blur\(16px\)/u);
  assert.doesNotMatch(css, /#[0-9a-fA-F]{3,8}\b/u, "observability styles use tokens only");
});
