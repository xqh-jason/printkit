/**
 * Pure model behind the job observability page.
 *
 * Everything here is free of Vue and Tauri so `node --test` can cover the
 * aggregation, ordering, tail de-duplication, caps and facet filtering without
 * a webview. The backend emits exactly seven stages per job; a stage that was
 * never recorded stays `null` in the timeline instead of being invented.
 */

/** The fixed print timeline, in wire order. */
export const STAGES = Object.freeze([
  "recv",
  "auth",
  "validate",
  "queue",
  "render",
  "submit",
  "result",
]);

/** Catalogue keys for the seven stage names; one definition for every page. */
export const STAGE_KEYS = Object.freeze({
  recv: "jobs.stage.recv",
  auth: "jobs.stage.auth",
  validate: "jobs.stage.validate",
  queue: "jobs.stage.queue",
  render: "jobs.stage.render",
  submit: "jobs.stage.submit",
  result: "jobs.stage.result",
});

/** Field names the job identity may surface when an event actually carries them. */
const EXTRA_FIELDS = ["copies", "template", "paper", "sheets"];

function isStage(value) {
  return typeof value === "string" && STAGES.includes(value);
}

function validEvent(event) {
  return Boolean(event) && typeof event.seq === "number" && typeof event.traceId === "string";
}

/* Rust time serialises RFC 3339 with nanoseconds; Date.parse wants milliseconds.
 * Trim beyond three fractional digits instead of losing the whole timestamp. */
function parseAt(value) {
  if (typeof value !== "string") return null;
  const normalized = value.replace(/(\.\d{3})\d+/, "$1");
  const ms = Date.parse(normalized);
  return Number.isNaN(ms) ? null : ms;
}

function eventEnd(event) {
  const start = parseAt(event.at);
  if (start === null) return null;
  const dur = typeof event.durMs === "number" && Number.isFinite(event.durMs) ? event.durMs : 0;
  return start + dur;
}

/** First eight characters of a trace id; the full value stays in the detail. */
export function traceShortId(traceId) {
  const text = String(traceId ?? "");
  return text.length > 8 ? text.slice(0, 8) : text;
}

/** Total duration in a compact form; unknown stays an em dash, never `0`. */
export function formatMs(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "—";
  if (value < 0) return "—";
  if (value < 1000) return `${Math.round(value)} ms`;
  if (value < 10_000) return `${(value / 1000).toFixed(1)} s`;
  return `${Math.round(value / 1000)} s`;
}

/** First `limit` `key=value` pairs of one event, for the compact timeline row. */
export function stageFieldPairs(event, limit = 3) {
  if (!event || !event.fields) return [];
  return Object.entries(event.fields)
    .slice(0, limit)
    .map(([key, value]) => ({ key, value: String(value) }));
}

/**
 * Group events into one job per `traceId`, newest trace first.
 *
 * Only observed values are copied: a missing printer/pages/queuePos stays
 * undefined so the card renders a dash instead of a fabricated zero.
 */
export function aggregateJobs(events, limit = 200) {
  const sorted = (Array.isArray(events) ? events : [])
    .filter(validEvent)
    .slice()
    .sort((left, right) => left.seq - right.seq);

  const byTrace = new Map();
  for (const event of sorted) {
    let job = byTrace.get(event.traceId);
    if (!job) {
      job = {
        traceId: event.traceId,
        shortId: traceShortId(event.traceId),
        events: [],
        byStage: {},
        extras: {},
        firstSeq: event.seq,
        lastSeq: event.seq,
      };
      byTrace.set(event.traceId, job);
    }
    job.events.push(event);
    job.lastSeq = event.seq;

    if (isStage(event.stage)) {
      const previous = job.byStage[event.stage];
      // A trace normally has one event per stage; keep the latest if repeated.
      if (!previous || event.seq >= previous.seq) job.byStage[event.stage] = event;
    }

    if (job.origin === undefined && event.origin !== undefined) job.origin = event.origin;
    if (job.printer === undefined && event.printer !== undefined) job.printer = event.printer;
    if (job.pages === undefined && event.pages !== undefined) job.pages = event.pages;
    if (job.queuePos === undefined && event.queuePos !== undefined) job.queuePos = event.queuePos;

    for (const key of EXTRA_FIELDS) {
      if (job.extras[key] === undefined && event.fields && event.fields[key] !== undefined) {
        job.extras[key] = event.fields[key];
      }
    }
  }

  const jobs = [];
  for (const job of byTrace.values()) {
    const eventsOfJob = job.events;
    const last = eventsOfJob[eventsOfJob.length - 1];
    const result = job.byStage.result;
    const failedEvent = eventsOfJob.find((event) => event.ok === false);

    // A failure is terminal even when the closing `result` event is missing.
    job.state = failedEvent ? "failed" : result ? "ok" : "running";
    job.currentStage = isStage(last.stage) ? last.stage : null;
    job.startedAt = eventsOfJob[0].at;
    job.lastEventAt = last.at;
    job.lastAtMs = parseAt(last.at);
    job.endedAt = job.state === "running" ? undefined : last.at;
    job.code = failedEvent?.code ?? result?.code;
    job.detail = failedEvent?.detail ?? result?.detail;

    if (result && typeof result.durMs === "number") {
      job.totalMs = result.durMs;
    } else {
      const starts = eventsOfJob.map((event) => parseAt(event.at)).filter((value) => value !== null);
      const ends = eventsOfJob.map(eventEnd).filter((value) => value !== null);
      job.totalMs =
        starts.length && ends.length
          ? Math.max(0, Math.max(...ends) - Math.min(...starts))
          : undefined;
    }
    jobs.push(job);
  }

  jobs.sort((left, right) => right.lastSeq - left.lastSeq);
  return limit > 0 ? jobs.slice(0, limit) : jobs;
}

/** Seven slots in wire order; `event` is `null` when that stage was not recorded. */
export function stageTimeline(job) {
  if (!job) return [];
  return STAGES.map((stage) => ({ stage, event: job.byStage[stage] ?? null }));
}

/**
 * Proportional stage segments for the total-duration bar.
 *
 * `result` is the closing event and its `durMs` is the whole trace duration,
 * not a stage duration, so it is the bar total rather than a segment. `other`
 * absorbs the part no stage duration explains (transport, waiting), so the bar
 * never claims more time than the trace reported.
 */
export function waterfall(job) {
  if (!job) return [];
  const segments = [];
  let measured = 0;
  for (const stage of STAGES) {
    if (stage === "result") continue;
    const event = job.byStage[stage];
    const ms = event && typeof event.durMs === "number" && event.durMs >= 0 ? event.durMs : 0;
    if (!event || ms <= 0) continue;
    measured += ms;
    segments.push({
      stage,
      ms,
      ok: event.ok !== false,
      queuePos: stage === "queue" ? job.queuePos : undefined,
    });
  }

  const total = typeof job.totalMs === "number" ? job.totalMs : measured;
  const span = Math.max(total, measured);
  if (span > measured) segments.push({ stage: "other", ms: span - measured, ok: true });

  let startMs = 0;
  return segments.map((segment) => {
    const share = span > 0 ? segment.ms / span : 0;
    const entry = { ...segment, share, startMs };
    startMs += segment.ms;
    return entry;
  });
}

function dedupe(rows) {
  const seen = new Set();
  const result = [];
  for (const row of rows) {
    if (!validEvent(row) || seen.has(row.seq)) continue;
    seen.add(row.seq);
    result.push(row);
  }
  return result;
}

/**
 * Bound memory by dropping whole oldest traces, never half a timeline: a
 * partial trace would render missing stages without saying why.
 */
export function applyCaps(events, options = {}) {
  const maxRows = Number.isFinite(options.maxRows) && options.maxRows > 0 ? options.maxRows : 0;
  const maxTraces = Number.isFinite(options.maxTraces) && options.maxTraces > 0 ? options.maxTraces : 0;
  let rows = dedupe(Array.isArray(events) ? events.slice().sort((left, right) => left.seq - right.seq) : []);
  if (!rows.length) return rows;

  const lastSeqByTrace = new Map();
  for (const row of rows) lastSeqByTrace.set(row.traceId, row.seq);

  while (
    lastSeqByTrace.size &&
    ((maxRows && rows.length > maxRows) || (maxTraces && lastSeqByTrace.size > maxTraces))
  ) {
    let oldestId;
    let oldestSeq = Infinity;
    for (const [id, seq] of lastSeqByTrace) {
      if (seq < oldestSeq) {
        oldestSeq = seq;
        oldestId = id;
      }
    }
    if (oldestId === undefined) break;
    rows = rows.filter((row) => row.traceId !== oldestId);
    lastSeqByTrace.delete(oldestId);
  }

  // A single oversized trace cannot be dropped without losing the only job.
  if (maxRows && rows.length > maxRows) rows = rows.slice(-maxRows);
  return rows;
}

/**
 * Incremental merge for the live tail: drops events at or below the cursor and
 * duplicate `seq` values from overlapping pages, then applies the caps.
 */
export function mergeEvents(current, incoming, cursor = 0, options = {}) {
  const known = []; // rows already retained
  const seen = new Set();
  for (const row of Array.isArray(current) ? current : []) {
    if (!validEvent(row) || seen.has(row.seq)) continue;
    seen.add(row.seq);
    known.push(row);
  }
  for (const row of Array.isArray(incoming) ? incoming : []) {
    if (!validEvent(row)) continue;
    if (cursor && row.seq <= cursor) continue;
    if (seen.has(row.seq)) continue;
    seen.add(row.seq);
    known.push(row);
  }
  const events = applyCaps(known.sort((left, right) => left.seq - right.seq), options);
  const lastSeq = events.reduce((max, row) => Math.max(max, row.seq), Number(cursor) || 0);
  return { events, lastSeq };
}

/** In-memory facets. Values the backend may not filter on are marked in the UI. */
export function filterJobs(jobs, filters = {}) {
  const list = Array.isArray(jobs) ? jobs : [];
  const stages = Array.isArray(filters.stages) ? filters.stages.filter(isStage) : [];
  const outcome = filters.outcome ?? null;
  const printer = filters.printer ?? "";
  const origin = filters.origin ?? "";
  const keyword = String(filters.keyword ?? "").trim().toLowerCase();
  const fromMs = typeof filters.fromMs === "number" && Number.isFinite(filters.fromMs) ? filters.fromMs : null;

  return list.filter((job) => {
    if (stages.length > 0 && !stages.some((stage) => job.byStage[stage])) return false;
    if (outcome && job.state !== outcome) return false;
    if (printer && job.printer !== printer) return false;
    if (origin && job.origin !== origin) return false;
    if (fromMs !== null && !(typeof job.lastAtMs === "number" && job.lastAtMs >= fromMs)) return false;
    if (keyword) {
      const haystack = [job.traceId, job.origin, job.printer, job.code]
        .filter((value) => value !== undefined && value !== null && value !== "")
        .join(" ")
        .toLowerCase();
      if (!haystack.includes(keyword)) return false;
    }
    return true;
  });
}

/** Distinct printer/origin values for the facet selects, sorted for stability. */
export function facetValues(jobs) {
  const printers = new Set();
  const origins = new Set();
  for (const job of Array.isArray(jobs) ? jobs : []) {
    if (job.printer) printers.add(job.printer);
    if (job.origin) origins.add(job.origin);
  }
  return {
    printers: [...printers].sort((left, right) => left.localeCompare(right)),
    origins: [...origins].sort((left, right) => left.localeCompare(right)),
  };
}

/** Previous/next trace id in newest-first order; `null` at either end. */
export function traceNeighbors(jobs, traceId) {
  const list = Array.isArray(jobs) ? jobs : [];
  const index = list.findIndex((job) => job.traceId === traceId);
  if (index < 0) return { prev: null, next: null };
  return {
    prev: index > 0 ? list[index - 1].traceId : null,
    next: index < list.length - 1 ? list[index + 1].traceId : null,
  };
}

/** Serialise one trace's events, newest last, as a JSON array or JSONL lines. */
export function serializeTrace(job, format = "json") {
  const events = job && Array.isArray(job.events) ? job.events : [];
  if (format === "jsonl") return events.map((event) => JSON.stringify(event)).join("\n");
  return JSON.stringify(events, null, 2);
}

/**
 * Oldest trace first, ordered by the first event ever seen for that trace.
 * Later events update a card in place; they never move it or remount its
 * detail panel. New traces are appended at the end.
 */
export function orderJobs(jobs) {
  return (Array.isArray(jobs) ? jobs : []).slice().sort((left, right) => {
    const first = (left.firstSeq ?? 0) - (right.firstSeq ?? 0);
    if (first !== 0) return first;
    return String(left.traceId ?? "").localeCompare(String(right.traceId ?? ""));
  });
}

/** Fixed windows offered by the time-range facet. */
export const RANGE_PRESETS = Object.freeze({ "15": 15, "60": 60, "1440": 1440 });

/**
 * Minutes the backend window should cover. Custom ranges start at a local
 * `datetime-local` value; an empty or unparseable start returns `null` so the
 * page can keep the previous window instead of silently widening it.
 */
export function rangeMinutes(range, customFrom, now = Date.now()) {
  if (range === "custom") {
    if (typeof customFrom !== "string" || !customFrom.trim()) return null;
    const start = Date.parse(customFrom);
    if (Number.isNaN(start)) return null;
    const minutes = Math.ceil((now - start) / 60_000);
    if (!Number.isFinite(minutes) || minutes <= 0) return null;
    return Math.min(525_600, minutes);
  }
  const minutes = RANGE_PRESETS[range];
  return typeof minutes === "number" ? minutes : null;
}

/** True when the viewport sits within `margin` of the scroll bottom. */
export function nearBottom(scrollTop, clientHeight, scrollHeight, margin = 24) {
  if (!Number.isFinite(clientHeight) || !Number.isFinite(scrollHeight)) return false;
  return scrollHeight - scrollTop - clientHeight <= margin;
}

/**
 * Follow decision for the list. While following, scrolling up to one screen is
 * tolerated; beyond that the user owns the viewport and follow stops. It only
 * re-arms once the list is actually back at the bottom.
 */
export function followAfterScroll({ scrollTop, clientHeight, scrollHeight, wasFollowing }) {
  if (!Number.isFinite(scrollTop) || !Number.isFinite(clientHeight) || !Number.isFinite(scrollHeight)) {
    return false;
  }
  const distance = scrollHeight - scrollTop - clientHeight;
  if (wasFollowing) return distance <= clientHeight;
  return distance <= 24;
}
