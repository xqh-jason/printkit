export type NavPreference = "auto" | "sidebar" | "tabs";
export type NavLayout = "sidebar" | "tabs";

export const SIDE_AT: number;
export const NARROW_AT: number;
export const SIDEBAR_DEFAULT: number;
export const SIDEBAR_NARROW: number;
export const SIDEBAR_MIN: number;
export const SIDEBAR_MAX: number;
export const SIDEBAR_MAX_RATIO: number;
export const LEGACY_RAIL_MAX: number;
export const BRAND_WORDMARK_MIN: number;
export const WIDTH_STEP: number;
export const WIDTH_STEP_LARGE: number;
export const NAV_SIDEBAR: NavLayout;
export const NAV_TABS: NavLayout;

export function viewportWidth(innerWidth: number): number;
export function resolveNav(preference: NavPreference | string | null | undefined, viewport: number): NavLayout;
export function maxSidebarWidth(viewport: number): number;
export function clampSidebarWidth(width: number | string, viewport: number): number;
export function defaultSidebarWidth(viewport: number): number;
export function sidebarWidth(
  preferred: number | null | undefined,
  viewport: number,
): number;
export function migrateSidebarWidth(width: number | null | undefined): number | null;
export function stepSidebarWidth(
  width: number,
  direction: 1 | -1,
  large: boolean,
  viewport: number,
): number;

export function brandWordmarkVisible(width: number): boolean;
