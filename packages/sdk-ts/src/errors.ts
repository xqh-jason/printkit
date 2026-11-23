import type { ErrorCode } from "./types.js";

/**
 * One failed call. `code` is the wire error code, so callers branch on the same
 * vocabulary the server documents instead of on message text.
 */
export class EngineError extends Error {
  readonly code: ErrorCode;
  readonly detail: unknown;

  constructor(code: ErrorCode, message: string, detail?: unknown) {
    super(message);
    this.name = "EngineError";
    this.code = code;
    this.detail = detail;
  }
}
