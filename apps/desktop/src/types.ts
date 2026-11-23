export type ServiceState = "stopped" | "runningOwned" | "runningExternal";

export interface ServiceStatus {
  state: ServiceState;
  pid: number | null;
  port: number | null;
  reachable: boolean;
  lockPath: string;
  staleLock: boolean;
  detail: string | null;
}

export interface SidecarInfo {
  version: string;
  target: string;
  compatible: boolean;
  message: string;
}

export interface ConfigView {
  dataDir: string;
  port: number;
  json: unknown;
}

/** Unified retention setting; logDays/recordsDays expose legacy divergence. */
export interface RetentionView {
  days: number;
  source: string;
  logDays: number;
  logSource: string;
  recordsDays: number;
  recordsSource: string;
  restartRequired: boolean;
}

export type AutoStartState =
  | "idle"
  | "attempting"
  | "started"
  | "failed"
  | "skippedExternal"
  | "skippedIncompatible"
  | "skippedConfig"
  | "disabled";

export interface AutoStartStatus {
  state: AutoStartState;
  error: string | null;
}

export interface Snapshot {
  sidecar: SidecarInfo;
  config: ConfigView | null;
  configError: string | null;
  status: ServiceStatus;
  autoStart: AutoStartStatus;
  plugins: PluginView[] | null;
  pluginsError: string | null;
  pluginsStale: boolean;
  pluginsReachable: boolean;
  pluginsPreset: string | null;
  pluginsBroken: string | null;
  cliPath: string | null;
}
export type PluginState = "on" | "off" | "pending" | "incompatible" | "invalid" | "broken";
export type PluginAvailability = "on" | "off" | "unknown";

export interface PluginConfigProperty {
  type?: string;
  enum?: Array<string | number | boolean>;
  default?: string | number | boolean;
  minimum?: number;
  maximum?: number;
  title?: Record<string, string> | string;
  description?: Record<string, string> | string;
}

export interface PluginConfigSchema {
  properties?: Record<string, PluginConfigProperty>;
}

export interface PluginView {
  id: string;
  name: string;
  nameKey?: string | null;
  sub?: string;
  description?: string;
  version: string;
  kind: string;
  api?: string | null;
  state: PluginState;
  active?: boolean;
  enabled?: boolean;
  chosen: boolean;
  source: string;
  reason?: string | null;
  ui?: Array<{ window: string; page: string; entry?: string | null; titles?: Record<string, string> }> | null;
  requires: string[];
  requiredBy: string[];
  inUse?: number;
  inUseDetail?: { ws?: number; leases?: number } | null;
  dataCount?: number | null;
  capabilities?: {
    methods?: string[];
    adminRoutes?: unknown[];
    ui?: Array<{ window: string; page: string; entry?: string | null; titles?: Record<string, string> }>;
  } | null;
  permissions?: {
    bridge?: { selfConfig?: boolean; printers?: boolean; recordsSummary?: boolean };
    [key: string]: unknown;
  } | null;
  config?: Record<string, unknown> | null;
  schema?: PluginConfigSchema | null;
}

export interface PluginsReply {
  /** Desktop wrapper fields; the service's POST body carries only the rest. */
  instance?: string;
  stale?: boolean;
  reachable?: boolean;
  error?: string | null;
  at?: number;
  settled?: boolean | null;
  plugins: PluginView[] | null;
  preset: string | null;
  broken: string | null;
}


export interface RecordStatsBucket {
  key: string;
  count: number;
  sheets: number;
  avgDurMs: number;
}
export interface RecordsSummary {
  todaySheets: number;
  /** Count of today's successful print requests (one per successful request). */
  todayDone: number;
  todayFailed: number;
  todayCancelled: number;
  total: number;
  failed: number;
  cancelled: number;
  /** Current schema requires it; an older service missing it is treated as 0
   *  by the UI metric helpers, which must not crash during a version skew. */
  pending: number;
  avgDurMs: number;
  slowestPrinter?: string;
  slowestAvgDurMs: number;
}

export interface RecordView {
  id: number;
  seq: number;
  at: string;
  origin: string;
  template?: string;
  tags: string[];
  printer?: string;
  pages: number;
  copies: number;
  status: string;
  durMs: number;
  errorCode?: string;
  traceId: string;
}

export interface RecordsPage {
  records: RecordView[];
  nextCursor?: number | null;
}



export interface OriginsReply {
  origins: string[];
}

export interface Printer {
  name: string;
  isDefault: boolean;
  raw: boolean;
  mimeTypes: string[];
  duplex: boolean;
  paperSource: boolean;
}

export interface Paper {
  name: string;
  widthMm: number;
  heightMm: number;
  kind: "named" | "custom";
}

export interface TraceEventView {
  seq: number;
  traceId: string;
  stage: string;
  at: string;
  durMs?: number;
  ok: boolean;
  code?: string;
  detail?: string;
  origin?: string;
  printer?: string;
  pages?: number;
  queuePos?: number;
  fields: Record<string, string | number | boolean>;
}
