import assert from "node:assert/strict";
import test from "node:test";

import { localDay, localIsoDay } from "../src/format.js";

test("record filter dates follow the local calendar day", () => {
  // 00:30 in UTC+8 is still the previous day in UTC; the filter must not shift.
  const early = new Date(2026, 10, 11, 0, 30, 0);
  assert.equal(localDay(0, early), "2026-11-11");
  assert.equal(localDay(-7, early), "2026-11-04");
  const late = new Date(2026, 10, 11, 23, 45, 0);
  assert.equal(localDay(0, late), "2026-11-11");
});

test("records bounds are local midnight instants, not bare UTC dates", () => {
  const afternoon = new Date(2026, 10, 12, 15, 0, 0);
  const from = new Date(localIsoDay(-7, afternoon));
  const to = new Date(localIsoDay(1, afternoon));
  assert.match(localIsoDay(0, afternoon), /^2026-11-12T00:00:00[+-]\d{2}:\d{2}$/u);
  assert.ok(from <= afternoon && afternoon < to, "today afternoon is inside the window");
  // A bare date is UTC midnight, which excluded this afternoon record.
  assert.ok(new Date("2026-11-12T00:00:00Z") < new Date("2026-11-12T15:00:00+08:00"));
});
