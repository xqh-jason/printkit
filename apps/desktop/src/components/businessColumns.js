/** Table columns that fit at each business-page breakpoint. */
export const NARROW_AT = 760;
export const TINY_AT = 560;

export function visibleRecordColumns(width) {
  const value = Number(width);
  const viewport = Number.isFinite(value) && value > 0 ? value : 1440;
  return {
    narrow: viewport < NARROW_AT,
    tiny: viewport < TINY_AT,
    printer: viewport >= NARROW_AT,
    copies: viewport >= NARROW_AT,
    pages: viewport >= TINY_AT,
    // Key failure/identity columns stay visible at the 900px window; the table
    // pane scrolls horizontally instead of hiding them.
    error: true,
    template: viewport >= 900,
    trace: viewport >= 900,
    duration: viewport >= 1100,
  };
}

/**
 * Which empty surface (if any) a list should show. `rows` means the list
 * renders normally; `noMatch` only applies when filters/search are active.
 */
export function resolveEmptyState({ loading, count, hasFilters }) {
  if (loading) return "loading";
  if (Number(count) > 0) return "rows";
  return hasFilters ? "noMatch" : "empty";
}

/** Logs keep their key columns inside the desktop page width. */
export function visibleLogColumns(width) {
  const value = Number(width);
  const viewport = Number.isFinite(value) && value > 0 ? value : 1440;
  return {
    seq: viewport >= 1000,
    stage: viewport >= 760,
    time: true,
    trace: true,
    result: true,
    duration: true,
  };
}

/** Result column label key; keeps the table renderer and tests in lockstep. */
export function logResultKey(ok) {
  return ok ? "logs.result.ok" : "logs.result.failed";
}
