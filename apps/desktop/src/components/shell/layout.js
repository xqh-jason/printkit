/**
 * Pure navigation geometry for the desktop shell.
 *
 * The window decides between a sidebar and a top tab bar at 900px, but the
 * user can pin either layout per window label. Sidebar width is bounded by
 * 168–320px and never more than 36% of the viewport. Everything here is a
 * pure function so `node --test` can cover the breakpoints without a browser.
 */

export const SIDE_AT = 900;
export const NARROW_AT = 860;

export const SIDEBAR_DEFAULT = 212;
export const SIDEBAR_NARROW = 184;
export const SIDEBAR_MIN = 168;
export const SIDEBAR_MAX = 320;
export const SIDEBAR_MAX_RATIO = 0.36;

/** Legacy 64px rail widths are migrated back to the default sidebar. */
export const LEGACY_RAIL_MAX = 64;

/** The first width where p + the full wordmark fit without truncation. */
export const BRAND_WORDMARK_MIN = 192;

export const WIDTH_STEP = 8;
export const WIDTH_STEP_LARGE = 32;

export const NAV_SIDEBAR = "sidebar";
export const NAV_TABS = "tabs";

/**
 * Window width for the sidebar/tabs decision. `window.innerWidth` does not
 * subtract the Windows scrollbar; at a 900px window it is still 900, so the
 * sidebar is selected. A zero/unavailable measurement falls back to `SIDE_AT`.
 */
export function viewportWidth(innerWidth) {
  const value = Number(innerWidth);
  return Number.isFinite(value) && value > 0 ? value : SIDE_AT;
}

/** "auto" follows the breakpoint; "sidebar"/"tabs" are explicit user choices. */
export function resolveNav(preference, viewport) {
  if (preference === NAV_SIDEBAR || preference === NAV_TABS) return preference;
  return viewport >= SIDE_AT ? NAV_SIDEBAR : NAV_TABS;
}

/** Largest width the sidebar may take at this viewport. */
export function maxSidebarWidth(viewport) {
  const ratio = Math.floor(viewport * SIDEBAR_MAX_RATIO);
  return Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, ratio));
}

export function clampSidebarWidth(width, viewport) {
  const max = maxSidebarWidth(viewport);
  const value = Number(width);
  if (!Number.isFinite(value)) return defaultSidebarWidth(viewport);
  return Math.min(Math.max(Math.round(value), SIDEBAR_MIN), max);
}

export function defaultSidebarWidth(viewport) {
  return viewport < NARROW_AT ? SIDEBAR_NARROW : SIDEBAR_DEFAULT;
}

/**
 * Resolved sidebar width: the user's saved width, then the breakpoint default.
 * Always clamped to the viewport.
 */
export function sidebarWidth(preferred, viewport) {
  return clampSidebarWidth(preferred ?? defaultSidebarWidth(viewport), viewport);
}

/** Treat any legacy rail-sized persisted width as absent. */
export function migrateSidebarWidth(width) {
  const value = Number(width);
  return Number.isFinite(value) && value > LEGACY_RAIL_MAX ? value : null;
}

export function stepSidebarWidth(width, direction, large, viewport) {
  const step = large ? WIDTH_STEP_LARGE : WIDTH_STEP;
  return clampSidebarWidth((Number(width) || defaultSidebarWidth(viewport)) + direction * step, viewport);
}

export function brandWordmarkVisible(width) {
  return width >= BRAND_WORDMARK_MIN;
}
