import { invoke } from "@tauri-apps/api/core";

import type {
  ConfigView,
  Paper,
  PluginsReply,
  PluginView,
  Printer,
  RecordStatsBucket,
  RecordsPage,
  RecordsSummary,
  RetentionView,
  OriginsReply,
  ServiceStatus,
  Snapshot,
  TraceEventView,
} from "./types";


export function fetchSnapshot(): Promise<Snapshot> {
  return invoke<Snapshot>("snapshot");
}

export function startService(): Promise<ServiceStatus> {
  return invoke<ServiceStatus>("start_service");
}

export function stopService(): Promise<ServiceStatus> {
  return invoke<ServiceStatus>("stop_service");
}

export function retryStart(): Promise<ServiceStatus> {
  return invoke<ServiceStatus>("retry_start");
}

/** Read the unified retention setting and the legacy log/records components. */
export function getRetentionDays(): Promise<RetentionView> {
  return invoke<RetentionView>("get_retention_days");
}

/** Persist one retention value for logs and print records. */
export function setRetentionDays(days: number): Promise<RetentionView> {
  return invoke<RetentionView>("set_retention_days", { days });
}

export function refreshConfig(): Promise<ConfigView> {
  return invoke<ConfigView>("refresh_config");
}

export interface RecordsWindow {
  from?: string;
  to?: string;
  status?: string;
  printer?: string;
  tags?: string[];
}

export async function recordsSummary(query: RecordsWindow = {}): Promise<RecordsSummary> {
  return invoke<RecordsSummary>("records_summary", { ...query } as Record<string, unknown>);
}

export interface RecordsStatsQuery extends RecordsWindow {
  groupBy: "day" | "printer";
}

export async function recordsStats(query: RecordsStatsQuery): Promise<RecordStatsBucket[]> {
  return invoke<RecordStatsBucket[]>("records_stats", { ...query } as Record<string, unknown>);
}

export interface RecordsQuery {
  from?: string;
  to?: string;
  tags?: string[];
  printer?: string;
  status?: string;
  cursor?: number;
  limit?: number;
}

export async function recordsList(query: RecordsQuery): Promise<RecordsPage> {
  return invoke<RecordsPage>("records_list", { ...query } as Record<string, unknown>);
}
export function exportRecords(query: Omit<RecordsQuery, "cursor" | "limit">): Promise<string> {
  return invoke<string>("export_records", { ...query } as Record<string, unknown>);
}

export interface LogsEventsQuery {
  traceId?: string;
  stage?: string;
  field?: string;
  /** Replay events strictly after this sequence number. */
  since?: number;
  minutes?: number;
  /** The service clamps this to 1..2000. */
  limit?: number;
}

export async function logsEvents(query: LogsEventsQuery): Promise<TraceEventView[]> {
  return invoke<TraceEventView[]>("logs_events", query as Record<string, unknown>);
}

export function exportLogs(): Promise<string> {
  return invoke<string>("export_logs");
}

export function listPrinters(): Promise<Printer[]> {
  return invoke<Printer[]>("list_printers");
}

export function listPapers(printer: string): Promise<Paper[]> {
  return invoke<Paper[]>("list_papers", { printer });
}

export function printTest(
  document: string,
  printer?: string,
  copies?: number,
): Promise<unknown> {
  return invoke("print_test", { document, printer, copies });
}

/** Print PDF bytes (base64) through the local service's PDF passthrough. */
export function printPdfBytes(
  bytes: string,
  fileName: string,
  printer?: string,
  copies?: number,
  requestId?: string,
): Promise<unknown> {
  return invoke("print_pdf_bytes", { bytes, fileName, printer, copies, requestId });
}

export interface JobWarning {
  code: string;
  message: string;
}

export interface JobStatusReply {
  jobId: string;
  status: "queued" | "printing" | "done" | "failed" | "cancelled";
  queuePos?: number;
  pages?: number;
  warnings?: JobWarning[];
  error?: { code: string; message: string };
}

export function jobStatus(jobId: string): Promise<JobStatusReply> {
  return invoke<JobStatusReply>("job_status", { jobId });
}

export function jobCancel(jobId: string): Promise<unknown> {
  return invoke("job_cancel", { jobId });
}

export function listOrigins(): Promise<OriginsReply> {
  return invoke<OriginsReply>("origins_list");
}

export function addOrigin(origin: string): Promise<OriginsReply> {
  return invoke<OriginsReply>("origin_add", { origin });
}

export function removeOrigin(origin: string): Promise<OriginsReply> {
  return invoke<OriginsReply>("origin_remove", { origin });
}

export function revealPath(path: string): Promise<void> {
  return invoke<void>("reveal_path", { path });
}

export function openDataDir(): Promise<void> {
  return invoke<void>("open_data_dir");
}

export interface PluginSetRequest {
  id?: string;
  preset?: "all" | "core";
  enabled?: boolean;
  confirmDataCount?: boolean;
  config?: Record<string, unknown>;
}

export function pluginList(): Promise<PluginsReply> {
  return invoke<PluginsReply>("plugin_list");
}

export function pluginSet(request: PluginSetRequest): Promise<PluginsReply> {
  return invoke<PluginsReply>("plugin_set", { request });
}

/** Recovery-only: enable records/observability from the business shell.
 * Disable and config remain plugin-page commands. */
export function pluginEnableOwned(
  id: "records" | "observability",
): Promise<PluginsReply> {
  return invoke<PluginsReply>("plugin_enable_owned", { id });
}

export type PluginLeaseOp = "acquire" | "heartbeat" | "release";

export function pluginLease(
  id: string,
  op: PluginLeaseOp,
  leaseId: string,
): Promise<{ ok: boolean; inUse: number }> {
  return invoke("plugin_lease", { request: { id, op, leaseId } });
}

export type { PluginView };
