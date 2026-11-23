import assert from "node:assert/strict";
import test from "node:test";

import {
  BRAND_WORDMARK_MIN,
  LEGACY_RAIL_MAX,
  NAV_SIDEBAR,
  NAV_TABS,
  NARROW_AT,
  SIDEBAR_DEFAULT,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  SIDE_AT,
  brandWordmarkVisible,
  clampSidebarWidth,
  defaultSidebarWidth,
  maxSidebarWidth,
  migrateSidebarWidth,
  resolveNav,
  sidebarWidth,
  viewportWidth,
  stepSidebarWidth,
} from "../src/components/shell/layout.js";

test("the navigation swaps sidebar for tabs at 900px", () => {
  assert.equal(SIDE_AT, 900);
  assert.equal(resolveNav("auto", 899), NAV_TABS);
  assert.equal(resolveNav("auto", 900), NAV_SIDEBAR);
  assert.equal(resolveNav("auto", 959), NAV_SIDEBAR);
  assert.equal(resolveNav(null, 1440), NAV_SIDEBAR);
  assert.equal(resolveNav(undefined, 560), NAV_TABS);

  // An explicit user choice survives any width.
  assert.equal(resolveNav("sidebar", 560), NAV_SIDEBAR);
  assert.equal(resolveNav("tabs", 1440), NAV_TABS);
});

test("sidebar width stays inside 168-320px and 36% of the window", () => {
  assert.equal(SIDEBAR_MIN, 168);
  assert.equal(SIDEBAR_MAX, 320);
  assert.equal(maxSidebarWidth(1440), 320);
  assert.equal(maxSidebarWidth(1000), 320);
  assert.equal(maxSidebarWidth(560), 201);
  assert.equal(clampSidebarWidth(100, 1440), 168);
  assert.equal(clampSidebarWidth(999, 1440), 320);
  assert.equal(clampSidebarWidth(300, 560), 201);
});

test("defaults follow the 212/184 breakpoint without a rail", () => {
  assert.equal(NARROW_AT, 860);
  assert.equal(defaultSidebarWidth(1440), SIDEBAR_DEFAULT);
  assert.equal(defaultSidebarWidth(1024), SIDEBAR_DEFAULT);
  assert.equal(defaultSidebarWidth(900), SIDEBAR_DEFAULT);
  assert.equal(defaultSidebarWidth(859), 184);
  assert.equal(sidebarWidth(null, 1440), 212);
  assert.equal(sidebarWidth(null, 800), 184);
  assert.equal(sidebarWidth(280, 1440), 280);
  assert.equal(sidebarWidth(null, 560), 184);
});

test("legacy rail widths migrate to a normal sidebar", () => {
  assert.equal(LEGACY_RAIL_MAX, 64);
  assert.equal(migrateSidebarWidth(64), null);
  assert.equal(migrateSidebarWidth(100), 100);
  assert.equal(sidebarWidth(migrateSidebarWidth(64), 1024), 212);
  assert.equal(sidebarWidth(migrateSidebarWidth(100), 1024), 168);
  assert.equal(sidebarWidth(64, 1024), 168);
});

test("keyboard steps move 8px or 32px and clamp", () => {
  assert.equal(stepSidebarWidth(200, 1, false, 1440), 208);
  assert.equal(stepSidebarWidth(200, -1, false, 1440), 192);
  assert.equal(stepSidebarWidth(200, 1, true, 1440), 232);
  assert.equal(stepSidebarWidth(168, -1, false, 1440), 168);
  assert.equal(stepSidebarWidth(310, 1, true, 1440), 320);
});

test("a 900px innerWidth selects the sidebar without scrollbar subtraction", () => {
  assert.equal(viewportWidth(900), 900);
  assert.equal(resolveNav("auto", viewportWidth(900)), NAV_SIDEBAR);
  assert.equal(resolveNav("auto", viewportWidth(899)), NAV_TABS);
  assert.equal(resolveNav("auto", viewportWidth(959)), NAV_SIDEBAR);
  assert.equal(viewportWidth(0), 900);
  assert.equal(viewportWidth(Number.NaN), 900);
  assert.equal(viewportWidth(1016), 1016);
  assert.equal(resolveNav("auto", viewportWidth(1016)), NAV_SIDEBAR);
});

test("the brand wordmark only appears when the sidebar row has room", () => {
  assert.equal(BRAND_WORDMARK_MIN, 192);
  for (const width of [168, 184]) {
    assert.equal(brandWordmarkVisible(width), false, String(width));
  }
  for (const width of [192, 200, 208, 212]) {
    assert.equal(brandWordmarkVisible(width), true, String(width));
  }
});
