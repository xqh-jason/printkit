import { Connection, openConnection, type ConnectionOptions } from "./connection.js";
import { resolveDefaultUrl } from "./discovery.js";
import { EngineError } from "./errors.js";
import { toBase64, type BinaryInput } from "./encoding.js";
import { METHOD_MAP } from "./mapping.js";
import type {
  BatchResult,
  Capabilities,
  DocPrintBatchParams,
  DocPrintPdfBatchParams,
  DocPrintPdfParams,
  Document,
  Empty,
  ExportPdfResult,
  JobEvent,
  JobRef,
  JobStatus,
  PaperSize,
  PdfBatchResult,
  PreviewPagesResult,
  Printer,
  RecordsListParams,
  RecordsListResult,
  RecordsQuery,
  RecordsStatsResult,
  RecordsSummaryView,
  StatsGroupView,
  SysInfo,
  TraceEventView,
  WarningBody,
  Welcome,
  WindowRef,
} from "./types.js";

export const SDK_VERSION = "0.2.7";

const DEFAULT_CONNECT_TIMEOUT_MS = 5_000;
/** A request that only submits work and returns a job id or a handle. */
const DEFAULT_JOB_TIMEOUT_MS = 30_000;
/**
 * A request that renders before answering. The macOS host budget is
 * `10s × (pages + 1)`, so 120s covers the first eleven pages; a larger
 * document needs an explicit `timeoutMs`, up to the full budget (about
 * 2010s for the 200-page maximum).
 */
const DEFAULT_RENDER_TIMEOUT_MS = 120_000;

export interface EngineOptions {
  /**
   * Defaults to `ws://127.0.0.1:19080/print`. Under Node the SDK reads the
   * port the service wrote to `serve.lock`, so the default also follows a
   * custom `--port`.
   */
  url?: string;
  /**
   * Omit to print without a token: a local script, or a page whose origin the
   * desktop app whitelisted. `raw` and `admin` methods still need a token.
   */
  token?: string;
  /**
   * Self-reported origin. Browsers send an `Origin` header as well; the service
   * rejects a handshake whose two origins differ, so pass the page origin only.
   */
  origin?: string;
  connectTimeoutMs?: number;
  /**
   * Fallback for every method. When set it overrides the SDK's per-method
   * default, so a small value also shortens `exportPdf`; a call-level
   * `timeoutMs` overrides both.
   */
  requestTimeoutMs?: number;
  /**
   * `sys.info` heartbeat cadence. The default (240000 ms) stays under the
   * service's five-minute idle close; set `0` to disable it.
   */
  heartbeatMs?: number;
}

/** Per-call overrides accepted by methods whose work takes longer. */
export interface CallOptions {
  /**
   * A per-call deadline. It overrides the engine-wide `requestTimeoutMs` and
   * the method default; the call fails locally with `E_TIMEOUT` without
   * cancelling the work the service already started.
   */
  timeoutMs?: number;
  /** Idempotency key; the service coalesces the same key for ten minutes. */
  requestId?: string;
}

export type BatchOptions = Partial<Omit<DocPrintBatchParams, "docs">>;
export type PdfOptions = Partial<Omit<DocPrintPdfParams, "bytes">>;
export type PdfBatchOptions = Partial<Omit<DocPrintPdfBatchParams, "pdfs">>;

/**
 * Which UI entries this build supports. Platform fields come from
 * `sys.info.capabilities`; `observability` and `records` come from
 * `welcome.pluginFamilies`. A page hides an entry when its field is false
 * instead of branching on the platform name.
 */
export interface UiEntries {
  duplex: boolean;
  paperSource: boolean;
  /** Cancelling a queued job works on every platform. */
  cancelQueued: boolean;
  /** Retracting an already submitted graphic/PDF job; macOS only in version 1. */
  cancelSubmitted: boolean;
  /** Retracting an already submitted raw job; raw jobs carry a spooler id. */
  cancelRaw: boolean;
  preview: boolean;
  rasterFallback: boolean;
  raw: boolean;
  batch: boolean;
  /** `printPdf` / embedded PDF without rendering. */
  printPdf: boolean;
  /** `trace.*` / `logs.export`, from the observability plugin family. */
  observability: boolean;
  /** `records.*`, from the records plugin family. */
  records: boolean;
  renderWorkers: number;
}

function uiEntries(info: SysInfo, welcome: Welcome): UiEntries {
  const capabilities = info.capabilities;
  return {
    duplex: capabilities.duplex,
    paperSource: capabilities.paperSource,
    cancelQueued: true,
    cancelSubmitted: capabilities.cancelSubmitted,
    cancelRaw: capabilities.cancelRaw,
    preview: capabilities.preview,
    rasterFallback: capabilities.rasterFallback,
    raw: capabilities.raw,
    batch: capabilities.batch,
    printPdf: capabilities.pdfPassthrough,
    observability: (welcome.pluginFamilies ?? []).includes("observability"),
    records: (welcome.pluginFamilies ?? []).includes("records"),
    renderWorkers: capabilities.renderWorkers,
  };
}

/**
 * A live session with a printkit service: one WebSocket, the handshake result
 * and the runtime capabilities. Each method sends one request envelope and
 * returns the wire result unchanged.
 */
export class Engine {
  #handshake: Welcome;
  #connection: Connection;
  readonly #options: ConnectionOptions;
  readonly #requestTimeoutMs: number | undefined;
  #info: SysInfo;
  #ui: UiEntries;
  readonly #warningHandlers = new Set<(warnings: WarningBody[]) => void>();
  readonly #jobHandlers = new Set<(event: JobEvent) => void>();
  readonly #traceHandlers = new Set<(event: TraceEventView) => void>();
  #subscriptions: Array<() => void> = [];
  #heartbeat: unknown;

  constructor(
    connection: Connection,
    welcome: Welcome,
    info: SysInfo,
    options: ConnectionOptions,
  ) {
    this.#connection = connection;
    this.#options = options;
    this.#requestTimeoutMs = options.requestTimeoutMs;
    this.#handshake = welcome;
    this.#info = info;
    this.#ui = uiEntries(info, welcome);
    this.#subscribe();
    const heartbeatMs = options.heartbeatMs ?? 240_000;
    if (heartbeatMs > 0) {
      this.#heartbeat = setInterval(() => {
        void this.#connection
          .request(METHOD_MAP.info, {}, this.#requestTimeoutMs ?? 10_000)
          .catch(() => undefined);
      }, Math.max(1_000, heartbeatMs));
      const timer = this.#heartbeat as unknown as { unref?: () => void };
      timer.unref?.();
    }
  }

  get handshake(): Welcome {
    return this.#handshake;
  }

  get capabilities(): Capabilities {
    return this.#info.capabilities;
  }

  get ui(): UiEntries {
    return this.#ui;
  }

  /** `sys.info`. Cached from `createEngine`; pass `true` to ask again. */
  async info(refresh = false): Promise<SysInfo> {
    if (!refresh) {
      return this.#info;
    }
    const info = await this.#call<SysInfo>(METHOD_MAP.info, {});
    this.#info = info;
    this.#ui = uiEntries(info, this.handshake);
    return info;
  }

  listPrinters(): Promise<Printer[]> {
    return this.#call<Printer[]>(METHOD_MAP.listPrinters, {});
  }

  paperSizes(printerName: string): Promise<PaperSize[]> {
    return this.#call<PaperSize[]>(METHOD_MAP.paperSizes, { name: printerName });
  }

  /**
   * `doc.print`: renders and queues the document, then returns its job id.
   * The default deadline is 30s. A call that times out may still have been
   * submitted by the service; without the response the caller has no job id
   * and must look the job up in the records view.
   */
  print(document: Document, options: CallOptions = {}): Promise<JobRef> {
    const params =
      options.requestId === undefined
        ? document
        : { ...document, requestId: options.requestId };
    return this.#call<JobRef>(METHOD_MAP.print, params, options.timeoutMs);
  }

  printBatch(documents: Document[], options: BatchOptions = {}): Promise<BatchResult> {
    return this.#call<BatchResult>(METHOD_MAP.printBatch, { docs: documents, ...options });
  }

  /**
   * Prints a complete PDF through the zero-render path. Hidden and refused when
   * the build cannot pass PDF bytes to the driver, so a page never offers a
   * button that the service answers with `E_UNSUPPORTED`.
   */
  printPdf(pdf: BinaryInput, options: PdfOptions = {}): Promise<JobRef> {
    if (!this.#ui.printPdf) {
      return Promise.reject(
        new EngineError(
          "E_UNSUPPORTED",
          "this engine has no PDF passthrough; use exportPdf and hand the file to the user",
        ),
      );
    }
    return this.#call<JobRef>(METHOD_MAP.printPdf, { bytes: toBase64(pdf), ...options });
  }

  /**
   * `doc.printPdfBatch`: one job from finished PDFs. A single PDF uses the
   * byte-for-byte passthrough; several PDFs are merged and may report
   * `W_PDF_MERGE_METADATA_LOST` on the response and done event.
   */
  printPdfBatch(pdfs: BinaryInput[], options: PdfBatchOptions = {}): Promise<PdfBatchResult> {
    if (!this.#ui.printPdf) {
      return Promise.reject(
        new EngineError(
          "E_UNSUPPORTED",
          "this engine has no PDF passthrough; use exportPdf and hand the files to the user",
        ),
      );
    }
    return this.#call<PdfBatchResult>(METHOD_MAP.printPdfBatch, {
      pdfs: pdfs.map((pdf) => ({ bytes: toBase64(pdf) })),
      ...options,
    });
  }

  preview(document: Document): Promise<WindowRef> {
    if (!this.#ui.preview) {
      return Promise.reject(
        new EngineError(
          "E_UNSUPPORTED",
          "this engine has no preview window; use exportPdf and hand the file to the user",
        ),
      );
    }
    return this.#call<WindowRef>(METHOD_MAP.preview, document);
  }

  /**
   * `doc.exportPdf`: renders the document and writes a PDF before answering.
   * The default deadline is 120s, which covers the service's
   * `10s × (pages + 1)` budget for up to eleven pages; a larger document
   * needs an explicit `timeoutMs`. A call that times out leaves the service
   * rendering and writing the file, so check the returned path before retrying.
   */
  exportPdf(document: Document, path: string, options: CallOptions = {}): Promise<ExportPdfResult> {
    return this.#call<ExportPdfResult>(METHOD_MAP.exportPdf, { document, path }, options.timeoutMs);
  }

  sendRaw(
    printerName: string,
    bytes: BinaryInput,
    options: CallOptions = {},
  ): Promise<JobRef> {
    return this.#call<JobRef>(METHOD_MAP.sendRaw, {
      printer: printerName,
      bytes: toBase64(bytes),
      ...(options.requestId === undefined ? {} : { requestId: options.requestId }),
    });
  }

  jobStatus(jobId: string): Promise<JobStatus> {
    return this.#call<JobStatus>(METHOD_MAP.jobStatus, { jobId });
  }

  /** Cancels a queued job on every platform; a submitted one only where supported. */
  async cancelJob(jobId: string): Promise<void> {
    await this.#call<Empty>(METHOD_MAP.cancelJob, { jobId });
  }

  /**
   * `preview.pages`: rasterises one page. The default deadline is 120s for the
   * same render budget as `exportPdf`; pass `timeoutMs` for a larger document.
   */
  previewPage(
    windowId: string,
    pageIndex: number,
    dpi: number,
    options: CallOptions = {},
  ): Promise<PreviewPagesResult> {
    return this.#call<PreviewPagesResult>(
      METHOD_MAP.previewPage,
      { windowId, pageIndex, dpi },
      options.timeoutMs,
    );
  }

  async closePreview(windowId: string): Promise<void> {
    await this.#call<Empty>(METHOD_MAP.closePreview, { windowId });
  }

  /** Usage rows of this token's origin; `cursor` pages backwards. */
  myRecords(query: Partial<RecordsListParams> = {}): Promise<RecordsListResult> {
    return this.#call<RecordsListResult>(METHOD_MAP.myRecords, { ...query });
  }

  myStats(groupBy: StatsGroupView, query: Partial<RecordsQuery> = {}): Promise<RecordsStatsResult> {
    return this.#call<RecordsStatsResult>(METHOD_MAP.myStats, { groupBy, ...query });
  }

  /** The four KPI cards for this token's origin. */
  mySummary(query: Partial<RecordsQuery> = {}): Promise<RecordsSummaryView> {
    return this.#call<RecordsSummaryView>(METHOD_MAP.mySummary, { ...query });
  }

  /** The origin's records as CSV bytes, with the UTF-8 BOM the service writes. */
  async exportRecords(query: Partial<RecordsQuery> = {}): Promise<Uint8Array> {
    const result = await this.#call<{ csv: string }>(METHOD_MAP.exportRecords, { ...query });
    return new TextEncoder().encode(result.csv);
  }

  /** Job progress pushed by the service; returns the unsubscribe function. */
  onJob(handler: (event: JobEvent) => void): () => void {
    this.#jobHandlers.add(handler);
    return () => {
      this.#jobHandlers.delete(handler);
    };
  }

  /** Trace events, only after an admin token subscribes with `trace.subscribe`. */
  onTrace(handler: (event: TraceEventView) => void): () => void {
    this.#traceHandlers.add(handler);
    return () => {
      this.#traceHandlers.delete(handler);
    };
  }

  /** `W_` warnings carried by successful responses. */
  onWarning(handler: (warnings: WarningBody[]) => void): () => void {
    this.#warningHandlers.add(handler);
    return () => this.#warningHandlers.delete(handler);
  }

  /** Closes the socket. Pending calls fail with `E_CONNECT`; `reconnect()` reopens. */
  close(): void {
    if (this.#heartbeat !== undefined) {
      clearInterval(this.#heartbeat as number);
      this.#heartbeat = undefined;
    }
    this.#connection.close();
  }

  /**
   * Replaces the session with a fresh `hello` handshake and `sys.info`, keeping
   * the options from `createEngine` and every `onJob`/`onTrace` handler. The
   * SDK never reconnects on its own: a new session carries a token and granted
   * scopes, so choosing when to replace it stays with the caller.
   */
  async reconnect(): Promise<void> {
    const { connection, welcome } = await openConnection(this.#options);
    let info: SysInfo;
    try {
      const response = await connection.request(
        METHOD_MAP.info,
        {},
        this.#requestTimeoutMs ?? DEFAULT_JOB_TIMEOUT_MS,
      );
      info = response.result as SysInfo;
    } catch (error) {
      connection.close();
      throw error;
    }
    const previous = this.#connection;
    this.#connection = connection;
    this.#handshake = welcome;
    this.#info = info;
    this.#ui = uiEntries(info, welcome);
    previous.close();
    this.#subscribe();
  }

  #subscribe(): void {
    for (const unsubscribe of this.#subscriptions) {
      unsubscribe();
    }
    this.#subscriptions = [
      this.#connection.onJob((event) => {
        for (const handler of this.#jobHandlers) {
          handler(event);
        }
      }),
      this.#connection.onTrace((event) => {
        for (const handler of this.#traceHandlers) {
          handler(event);
        }
      }),
    ];
  }

  #call<T>(
    method: (typeof METHOD_MAP)[keyof typeof METHOD_MAP],
    params: unknown,
    timeoutMs?: number,
  ): Promise<T> {
    const effective = timeoutMs ?? this.#requestTimeoutMs ?? defaultTimeoutFor(method);
    return this.#request<T>(method, params, effective);
  }

  async #request<T>(
    method: (typeof METHOD_MAP)[keyof typeof METHOD_MAP],
    params: unknown,
    timeoutMs: number,
  ): Promise<T> {
    const { result, warnings } = await this.#connection.request(method, params, timeoutMs);
    if (warnings.length > 0) {
      for (const handler of this.#warningHandlers) {
        handler(warnings);
      }
    }
    return result as T;
  }

}

/**
 * Opens the session: `hello` handshake, then `sys.info` so the caller can render
 * capability-gated entries immediately. `info()` later returns the cached answer.
 */
export async function createEngine(options: EngineOptions = {}): Promise<Engine> {
  const connectionOptions: ConnectionOptions = {
    url: await resolveDefaultUrl(options.url),
    ...(options.token === undefined ? {} : { token: options.token }),
    ...(options.origin === undefined ? {} : { origin: options.origin }),
    connectTimeoutMs: options.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS,
    ...(options.requestTimeoutMs === undefined
      ? {}
      : { requestTimeoutMs: options.requestTimeoutMs }),
    ...(options.heartbeatMs === undefined ? {} : { heartbeatMs: options.heartbeatMs }),
  };
  const { connection, welcome } = await openConnection(connectionOptions);
  try {
    const info = await connection.request(
      METHOD_MAP.info,
      {},
      connectionOptions.requestTimeoutMs ?? DEFAULT_JOB_TIMEOUT_MS,
    );
    return new Engine(connection, welcome, info.result as SysInfo, connectionOptions);
  } catch (error) {
    connection.close();
    throw error;
  }
}

/**
 * Deadline for one method: a render call gets the host budget, everything
 * else only has to submit the job and return its handle.
 */
function defaultTimeoutFor(method: (typeof METHOD_MAP)[keyof typeof METHOD_MAP]): number {
  switch (method) {
    case METHOD_MAP.exportPdf:
    case METHOD_MAP.previewPage:
      return DEFAULT_RENDER_TIMEOUT_MS;
    default:
      return DEFAULT_JOB_TIMEOUT_MS;
  }
}
