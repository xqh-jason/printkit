// WebSocket transport: one connection, one `hello`, then request/response
// envelopes plus pushed `job` events.
//
// A socket is never reconnected in the background: a session carries a token,
// an origin binding and granted scopes, so a replacement session is an
// explicit `engine.reconnect()`. Callers get `E_CONNECT` for a closed socket,
// which is also what a stopped service looks like.

import { EngineError } from "./errors.js";
import type { MethodName } from "./methods.js";
import type { ErrorBody, JobEvent, TraceEventView, WarningBody, Welcome } from "./types.js";

const SDK_VERSION = "0.2.5";

/** Fallback when neither the call nor `createEngine` sets a timeout. */
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
const PROTOCOL_VERSION = 1;

export interface ConnectionOptions {
  url: string;
  token?: string;
  /** Self-reported origin; must equal the browser's origin header when set. */
  origin?: string;
  connectTimeoutMs: number;
  /** Fallback for methods without a dedicated default; omitted means the
   * SDK's per-method default. */
  requestTimeoutMs?: number;
  /** `sys.info` keepalive interval; 0 disables it. Defaults to 240 000 ms. */
  heartbeatMs?: number;
}

interface ResponseBody {
  result: unknown;
  warnings: WarningBody[];
}

interface ResponseFrame {
  id: string;
  ok: boolean;
  result?: unknown;
  warnings?: WarningBody[];
  error?: ErrorBody;
}

type JobHandler = (event: JobEvent) => void;
type TraceHandler = (event: TraceEventView) => void;
type WarningHandler = (warnings: WarningBody[]) => void;

export class Connection {
  readonly #socket: WebSocket;
  readonly #requestTimeoutMs: number | undefined;
  readonly #pending = new Map<
    string,
    {
      resolve: (value: ResponseBody) => void;
      reject: (error: Error) => void;
      timer: ReturnType<typeof setTimeout>;
    }
  >();
  readonly #jobHandlers = new Set<JobHandler>();
  readonly #traceHandlers = new Set<TraceHandler>();
  readonly #warningHandlers = new Set<WarningHandler>();
  #handshake: {
    resolve: (welcome: Welcome) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  } | null = null;
  #nextId = 1;
  #closed = false;

  constructor(socket: WebSocket, requestTimeoutMs: number | undefined) {
    this.#socket = socket;
    this.#requestTimeoutMs = requestTimeoutMs;
    socket.addEventListener("message", (event) => {
      if (typeof event.data === "string") {
        this.#receive(event.data);
      }
    });
    socket.addEventListener("close", () => {
      this.#closed = true;
      this.#rejectAll(new EngineError("E_CONNECT", "the service closed the connection"));
    });
    socket.addEventListener("error", () => {
      this.#rejectAll(new EngineError("E_CONNECT", "the WebSocket reported an error"));
    });
  }

  /** Resolves with the server's `welcome`; rejects with the refusal or timeout. */
  handshake(timeoutMs: number): Promise<Welcome> {
    return new Promise<Welcome>((resolve, reject) => {
      this.#handshake = {
        resolve,
        reject,
        timer: setTimeout(() => {
          this.#handshake = null;
          this.close();
          reject(new EngineError("E_CONNECT", "the service did not answer the hello frame"));
        }, timeoutMs),
      };
    });
  }

  request(
    method: MethodName,
    params: unknown,
    timeoutMs?: number,
  ): Promise<ResponseBody> {
    if (this.#closed) {
      return Promise.reject(new EngineError("E_CONNECT", "the connection is closed"));
    }
    const timeout = timeoutMs ?? this.#requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    const id = String(this.#nextId++);
    return new Promise<ResponseBody>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new EngineError("E_TIMEOUT", `${method} timed out after ${timeout} ms`));
      }, timeout);
      this.#pending.set(id, { resolve, reject, timer });
      try {
        this.#socket.send(JSON.stringify({ id, method, params }));
      } catch (cause) {
        clearTimeout(timer);
        this.#pending.delete(id);
        reject(new EngineError("E_CONNECT", `cannot send ${method}`, cause));
      }
    });
  }

  onJob(handler: JobHandler): () => void {
    this.#jobHandlers.add(handler);
    return () => this.#jobHandlers.delete(handler);
  }

  onTrace(handler: TraceHandler): () => void {
    this.#traceHandlers.add(handler);
    return () => this.#traceHandlers.delete(handler);
  }

  onWarning(handler: WarningHandler): () => void {
    this.#warningHandlers.add(handler);
    return () => this.#warningHandlers.delete(handler);
  }

  close(): void {
    if (!this.#closed) {
      this.#closed = true;
      this.#rejectAll(new EngineError("E_CONNECT", "the connection is closed"));
      this.#socket.close();
    }
  }

  #receive(text: string): void {
    let frame: unknown;
    try {
      frame = JSON.parse(text);
    } catch {
      return;
    }
    if (typeof frame !== "object" || frame === null) {
      return;
    }
    const record = frame as Record<string, unknown>;
    switch (record["type"]) {
      case "welcome": {
        this.#settleHandshake(() => {
          const handshake = this.#handshake;
          this.#handshake = null;
          if (handshake !== null) {
            clearTimeout(handshake.timer);
            handshake.resolve(record as unknown as Welcome);
          }
        });
        return;
      }
      case "rejected": {
        const body = record["error"] as ErrorBody | undefined;
        this.#settleHandshake(() => {
          const handshake = this.#handshake;
          this.#handshake = null;
          if (handshake !== null) {
            clearTimeout(handshake.timer);
            handshake.reject(
              new EngineError(
                body?.code ?? "E_PROTOCOL",
                body?.message ?? "the service refused the handshake",
                body?.detail,
              ),
            );
          }
        });
        return;
      }
      case "event": {
        this.#dispatchEvent(record);
        return;
      }
      default: {
        this.#settleResponse(record as unknown as ResponseFrame);
      }
    }
  }

  #settleHandshake(settle: () => void): void {
    if (this.#handshake !== null) {
      settle();
    }
  }

  #dispatchEvent(record: Record<string, unknown>): void {
    // Unknown event kinds are ignored: a newer engine may push an event this
    // client was built before, and dropping it must not break request handling.
    if (record["event"] === "job") {
      for (const handler of this.#jobHandlers) {
        handler(record as unknown as JobEvent);
      }
    } else if (record["event"] === "trace") {
      for (const handler of this.#traceHandlers) {
        handler(record as unknown as TraceEventView);
      }
    }
  }

  #settleResponse(frame: ResponseFrame): void {
    if (typeof frame.id !== "string") {
      return;
    }
    const pending = this.#pending.get(frame.id);
    if (pending === undefined) {
      return;
    }
    this.#pending.delete(frame.id);
    clearTimeout(pending.timer);
    if (frame.ok) {
      const warnings = frame.warnings ?? [];
      if (warnings.length > 0) {
        for (const handler of this.#warningHandlers) {
          handler(warnings);
        }
      }
      pending.resolve({ result: frame.result, warnings });
      return;
    }
    const error = frame.error;
    pending.reject(
      new EngineError(
        error?.code ?? "E_PROTOCOL",
        error?.message ?? "the response carried no error body",
        error?.detail,
      ),
    );
  }

  #rejectAll(error: EngineError): void {
    const handshake = this.#handshake;
    this.#handshake = null;
    if (handshake !== null) {
      clearTimeout(handshake.timer);
      handshake.reject(error);
    }
    for (const [id, pending] of this.#pending) {
      this.#pending.delete(id);
      clearTimeout(pending.timer);
      pending.reject(error);
    }
  }
}

/**
 * Opens the socket and completes the handshake. The call resolves only after the
 * service accepted the token, so a returned `Connection` never exposes a socket
 * that would fail on its first request.
 */
export async function openConnection(options: ConnectionOptions): Promise<{
  connection: Connection;
  welcome: Welcome;
}> {
  const socket = new WebSocket(options.url);
  const connection = new Connection(socket, options.requestTimeoutMs);
  const welcome = connection.handshake(options.connectTimeoutMs);
  const hello = {
    type: "hello",
    protocol: PROTOCOL_VERSION,
    sdk: SDK_VERSION,
    ...(options.token === undefined ? {} : { token: options.token }),
    ...(options.origin === undefined ? {} : { origin: options.origin }),
  };
  const send = () => socket.send(JSON.stringify(hello));
  if (socket.readyState === WebSocket.OPEN) {
    send();
  } else {
    socket.addEventListener("open", send, { once: true });
  }
  try {
    return { connection, welcome: await welcome };
  } catch (error) {
    connection.close();
    throw error;
  }
}
