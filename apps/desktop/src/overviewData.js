import { localDay, localIsoDay } from "./format.js";

/** The rolling window shown by Overview: six days ago through today, with
 * `to` at tomorrow's local midnight so the service's exclusive upper bound
 * includes all of today. */
export function overviewWindow(now = new Date()) {
  const days = Array.from({ length: 7 }, (_, index) => localDay(index - 6, now));
  return {
    days,
    from: localIsoDay(-6, now),
    to: localIsoDay(1, now),
  };
}

/** Sort day buckets by date and fill each missing date with a real zero. */
export function fillDaySeries(buckets, days) {
  const byDay = new Map();
  for (const bucket of buckets ?? []) {
    if (!bucket || typeof bucket.key !== "string") continue;
    const previous = byDay.get(bucket.key);
    if (previous) {
      const count = previous.count + Number(bucket.count ?? 0);
      const avgDurMs = count === 0
        ? 0
        : (previous.avgDurMs * previous.count + Number(bucket.avgDurMs ?? 0) * Number(bucket.count ?? 0)) / count;
      byDay.set(bucket.key, {
        key: bucket.key,
        count,
        sheets: previous.sheets + Number(bucket.sheets ?? 0),
        avgDurMs,
      });
    } else {
      byDay.set(bucket.key, {
        key: bucket.key,
        count: Number(bucket.count ?? 0),
        sheets: Number(bucket.sheets ?? 0),
        avgDurMs: Number(bucket.avgDurMs ?? 0),
      });
    }
  }
  return (days ?? []).map((key) => {
    const value = byDay.get(key);
    return {
      key,
      count: value?.count ?? 0,
      sheets: value?.sheets ?? 0,
      avgDurMs: value?.avgDurMs ?? 0,
    };
  });
}

/**
 * Summary totals are aggregate records, not a day×status matrix.
 * `done = total - failed - cancelled - pending`; pending is not returned in the
 * visual split. A missing/null cancelled count is not turned into zero.
 */
export function statusDistribution(summary) {
  const total = Math.max(0, Number(summary?.total ?? 0));
  const failed = Math.min(total, Math.max(0, Number(summary?.failed ?? 0)));
  const cancelledKnown = summary?.cancelled !== null && summary?.cancelled !== undefined;
  const cancelled = cancelledKnown
    ? Math.min(total - failed, Math.max(0, Number(summary.cancelled)))
    : null;
  const terminal = Math.max(0, total - failed - (cancelled ?? 0));
  const pending = Math.min(terminal, Math.max(0, Number(summary?.pending ?? 0)));
  const done = Math.max(0, terminal - pending);
  return { total, done, failed, cancelled, cancelledKnown };
}

/** Failure rate over the query window; null when cancelled is missing or the
 *  explicit terminal denominator is zero. Pending and cancelled rows are not
 *  part of done + failed. */
export function failureRate(summary) {
  const parts = statusDistribution(summary);
  if (!parts.cancelledKnown) return null;
  const denominator = parts.done + parts.failed;
  return denominator === 0 ? null : parts.failed / denominator;
}

/** Success rate over done + failed; pending and cancelled are excluded. */
export function successRate(summary) {
  const parts = statusDistribution(summary);
  if (!parts.cancelledKnown) return null;
  const denominator = parts.done + parts.failed;
  return denominator === 0 ? null : Math.round((parts.done / denominator) * 100);
}

/** Five printers ranked by task count, then sheets and key. */
export function topPrinters(buckets, limit = 5) {
  return (buckets ?? [])
    .map((bucket) => ({
      key: String(bucket?.key ?? ""),
      count: Number(bucket?.count ?? 0),
      sheets: Number(bucket?.sheets ?? 0),
      avgDurMs: Number(bucket?.avgDurMs ?? 0),
    }))
    .filter((bucket) => bucket.key !== "")
    .sort(
      (left, right) =>
        right.count - left.count ||
        right.sheets - left.sheets ||
        left.key.localeCompare(right.key),
    )
    .slice(0, Math.max(0, limit));
}

/** Records endpoints must never run while the service is down or records off. */
export function recordsRequestable(serviceDown, recordsOn) {
  return !serviceDown && recordsOn;
}

export function localDayFromKey(key) {
  return new Date(`${key}T12:00:00`);
}

/** Today KPI values with explicit zero fallbacks for empty/offline summaries. */
export function todayMetrics(summary) {
  return {
    todayDone: Math.max(0, Number(summary?.todayDone ?? 0)),
    todayFailed: Math.max(0, Number(summary?.todayFailed ?? 0)),
    todayCancelled: Math.max(0, Number(summary?.todayCancelled ?? 0)),
  };
}
