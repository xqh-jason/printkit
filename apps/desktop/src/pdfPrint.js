/** One nonce per new Print PDF attempt; timeout retries reuse the stored key. */
export function pdfAttemptNonce() {
  const crypto = globalThis.crypto;
  if (crypto?.randomUUID) return crypto.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/** FNV-1a hash for a printable, bounded request id.
 *
 * The attempt nonce is random (`pdfAttemptNonce`), so a new click is a new job
 * even for the same file/printer/copies; only a timeout replay reuses the
 * stored request id.
 */
function shortHash(value) {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(16);
}

export function pdfRequestId(file, printer, copies, attempt) {
  const name = String(file?.name ?? "");
  const size = Number(file?.size ?? 0);
  const modified = Number(file?.lastModified ?? 0);
  const target = String(printer || "default");
  const count = Number(copies) || 1;
  // The local file name may be Unicode or longer than the service accepts.
  // Hash it so the header is printable ASCII and bounded.
  return [
    "pdf", shortHash(name), name.length, size, modified,
    shortHash(target), target.length, count, `attempt-${attempt ?? ""}`,
  ].join(":");
}

export function isJobActive(status) {
  return status === "queued" || status === "printing";
}

export function isJobTerminal(status) {
  return status === "done" || status === "failed" || status === "cancelled";
}

/** Terminal outcome of one poll, independent of DOM/timers. */
export function jobOutcome(reply, deadline, now = Date.now()) {
  const status = reply?.status;
  if (status === "done") return "done";
  if (status === "failed") return "failed";
  if (status === "cancelled") return "cancelled";
  if (Number(now) >= Number(deadline)) return "timeout";
  return "active";
}

/** Double-click / in-flight guard shared by the button and the submit path. */
export function canRunPdf(file, busy, active) {
  return Boolean(file) && !busy && !active;
}
