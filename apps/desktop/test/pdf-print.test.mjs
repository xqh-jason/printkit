import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { canRunPdf, isJobActive, isJobTerminal, jobOutcome, pdfAttemptNonce, pdfRequestId } from "../src/pdfPrint.js";
import {
  beginPdfAttempt,
  markPdfTerminal,
  markPdfTimeout,
  pdfLocked,
  refreshKey,
} from "../src/pdfAttempt.js";
import { visibleRecordColumns } from "../src/components/businessColumns.js";

const here = dirname(fileURLToPath(import.meta.url));
const source = (path) => readFileSync(join(here, "..", path), "utf8");

test("one attempt nonce makes independent clicks distinct jobs", () => {
  const file = { name: "label.pdf", size: 1024, lastModified: 1700000000000 };
  const attempt = pdfAttemptNonce();
  const first = pdfRequestId(file, "Office", 2, attempt);
  assert.equal(pdfRequestId(file, "Office", 2, attempt), first, "same attempt is stable");
  assert.notEqual(pdfRequestId(file, "Office", 2, pdfAttemptNonce()), first);
  assert.notEqual(pdfRequestId(file, "Office", 3, attempt), first);
  assert.notEqual(pdfRequestId(file, "HP", 2, attempt), first);
  assert.notEqual(pdfRequestId({ ...file, name: "other.pdf" }, "Office", 2, attempt), first);
  const fallback = pdfRequestId(file, "", 0, "");
  assert.match(fallback, /^pdf:[0-9a-f]+:9:1024:1700000000000:[0-9a-f]+:7:1:attempt-$/u);
  const unicode = pdfRequestId(
    { ...file, name: "\u6253\u5370 label.pdf" },
    "\u524d\u53f0",
    1,
    pdfAttemptNonce(),
  );
  assert.match(unicode, /^[\x20-\x7e]{1,128}$/u);
});

test("a timeout keeps the old key for status, while reprint is a new job", () => {
  const file = { name: "label.pdf", size: 1024 };
  const attempt = pdfAttemptNonce();
  const submitted = pdfRequestId(file, "Office", 1, attempt);
  assert.equal(pdfRequestId(file, "Office", 1, attempt), submitted, "refresh keeps the old job key");
  const afterTimeout = pdfRequestId(file, "Office", 1, pdfAttemptNonce());
  assert.notEqual(afterTimeout, submitted, "new attempt after timeout: new job");
  assert.match(pdfAttemptNonce(), /^[0-9a-f-]{8,}$/iu);
});

test("job state helpers split active and terminal states", () => {
  assert.equal(isJobActive("queued"), true);
  assert.equal(isJobActive("printing"), true);
  assert.equal(isJobActive("done"), false);
  assert.equal(isJobTerminal("done"), true);
  assert.equal(isJobTerminal("failed"), true);
  assert.equal(isJobTerminal("cancelled"), true);
  assert.equal(isJobTerminal("printing"), false);
});

test("success, failure, cancel and timeout resolve once; active keeps polling", () => {
  assert.equal(jobOutcome({ status: "done" }, 1000, 5000), "done");
  assert.equal(jobOutcome({ status: "failed" }, 1000, 5000), "failed");
  assert.equal(jobOutcome({ status: "cancelled" }, 1000, 5000), "cancelled");
  assert.equal(jobOutcome({ status: "queued" }, 1000, 5000), "timeout");
  assert.equal(jobOutcome({ status: "printing" }, 1000, 500), "active");
  assert.equal(jobOutcome({ status: "done" }, 0, 999999), "done", "a late reply still wins");
});

test("double clicks cannot submit while the first job is in flight", () => {
  const file = { name: "a.pdf" };
  assert.equal(canRunPdf(file, false, false), true);
  assert.equal(canRunPdf(file, true, false), false);
  assert.equal(canRunPdf(file, false, true), false);
  assert.equal(canRunPdf(null, false, false), false);
});

test("60 s timeout keeps the key, Refresh reuses it and Reprint starts a new job", () => {
  const file = { name: "label.pdf", size: 1024, lastModified: 1700000000000 };
  const submitted = beginPdfAttempt(file, "Office", 2, "attempt-1");
  const key = submitted.key;
  assert.equal(pdfLocked(false, "printing", submitted), true, "an active job locks writes");

  const timedOut = markPdfTimeout(submitted);
  assert.equal(timedOut.key, key, "the timeout keeps the key");
  assert.equal(timedOut.timedOut, true);
  assert.equal(refreshKey(timedOut), key, "Refresh reads the same job");
  assert.equal(pdfLocked(false, "printing", timedOut), false, "timeout unlocks writes");

  const reprint = beginPdfAttempt(file, "Office", 2, "attempt-2");
  assert.notEqual(reprint.key, key, "Reprint is a new attempt and a new job");
  assert.equal(pdfLocked(false, "printing", reprint), true);

  assert.equal(markPdfTerminal(timedOut, "done"), null, "terminal drops the attempt and key");
  assert.equal(pdfLocked(true, null, null), true, "a submit in flight still locks");

  const page = source("src/views/PrintersPage.vue");
  assert.match(page, /markPdfTimeout\(pdfAttempt\.value\)/u);
  assert.match(page, /markPdfTerminal\(pdfAttempt\.value/u);
  assert.match(page, /refreshKey\(pdfAttempt\.value\)/u);
  assert.match(page, /pdfActionsLocked\(pdfBusy\.value, pdfJob\.value\?\.status, pdfAttempt\.value\)/u);
});

test("PDF and test print are disabled for stopped or external services", () => {
  const page = source("src/views/PrintersPage.vue");
  assert.match(page, /const writeReason = computed/u);
  assert.match(page, /business\.printPdf\.external/u);
  assert.match(page, /business\.printPdf\.stopped/u);
  assert.match(page, /const canTestPrint = computed\(\(\) => writeReason\.value === ""\)/u);
  assert.match(page, /const canPdf = computed\(/u);
  assert.match(page, /:disabled="!owned \|\| pdfLocked"/u);
  assert.match(page, /:disabled="!canPdf \|\| pdfLocked"/u);
  assert.match(page, /:disabled="!canTestPrint"/u);
  assert.match(page, /:disabled="pdfBusy \|\| !owned"/u);
});

test("PDF warnings are surfaced on completion without blocking the job", () => {
  const page = source("src/views/PrintersPage.vue");
  assert.match(page, /const pdfWarnings = ref<JobWarning\[\]>/u);
  assert.match(page, /pdfWarnings\.value = reply\.warnings \?\? \[\]/u);
  assert.match(page, /business\.printPdf\.warning\.metadataLost/u);
  assert.match(page, /W_PDF_MERGE_METADATA_LOST/u);
  assert.match(page, /v-for="warning in pdfWarnings"/u);
  assert.match(page, /pdfWarnings\.value = \[\]/u, "new attempts clear old warnings");
  const api = source("src/api.ts");
  assert.match(api, /warnings\?: JobWarning\[\]/u);
});

test("records keep error, copies, template and trace visible at 900px", () => {
  const columns = visibleRecordColumns(900);
  assert.equal(columns.printer, true);
  assert.equal(columns.copies, true);
  assert.equal(columns.error, true);
  assert.equal(columns.template, true);
  assert.equal(columns.trace, true);
  const page = source("src/views/BusinessRecords.vue");
  assert.match(page, /records\.table\.error/u);
  assert.match(page, /records\.table\.template/u);
  assert.match(page, /records\.table\.trace/u);
});

test("copies accept the documented 1-999 range and fixed A4 copy remains", () => {
  const page = source("src/views/PrintersPage.vue");
  assert.match(page, /:min="1" :max="999"/u);
  assert.match(page, /business\.printTest\.knownLimit/u);
});

test("test print is gated by printer and owned service", () => {
  const page = source("src/views/PrintersPage.vue");
  assert.match(page, /canTestPrint/u);
  assert.match(page, /runningOwned/u);
  assert.match(page, /business\.printTest\.noPrinter/u);
  assert.match(page, /business\.printPdf\.external/u);
});
