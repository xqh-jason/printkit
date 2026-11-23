import { isJobActive, pdfRequestId } from "./pdfPrint.js";

/**
 * Pure attempt state for one Print PDF submission.
 *
 * `key` is the idempotency key sent to the service. A timeout keeps the key so
 * Refresh can read the old job; a new click always starts a new attempt and
 * therefore a new job. Terminal outcomes or submit errors settle the attempt
 * and drop the key.
 */
export function beginPdfAttempt(file, printer, copies, nonce) {
  return Object.freeze({
    key: pdfRequestId(file, printer, copies, nonce),
    timedOut: false,
    outcome: null,
  });
}

/** Keep the key, mark the state as unknown so the page unlocks. */
export function markPdfTimeout(attempt) {
  return attempt ? Object.freeze({ ...attempt, timedOut: true }) : null;
}

/** Terminal reply: drop the attempt (and key) so Refresh cannot reuse it. */
export function markPdfTerminal() {
  return null;
}

/** Drop the attempt after a cancelled job or a submit/network error. */
export function settlePdfAttempt() {
  return null;
}

/** Key for a status refresh; `null` means there is no tracked job. */
export function refreshKey(attempt) {
  return attempt?.key ?? null;
}

/**
 * Active jobs disable the write actions until the 60 s timeout. After the
 * timeout the old job is unknown, so it must not lock the page forever.
 */
export function pdfLocked(busy, status, attempt) {
  return Boolean(busy) || (isJobActive(status) && !attempt?.timedOut);
}
