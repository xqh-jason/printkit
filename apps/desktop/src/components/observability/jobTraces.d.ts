import type { TraceEventView } from "../../types";

export type KnownStage = "recv" | "auth" | "validate" | "queue" | "render" | "submit" | "result";
export type JobState = "running" | "ok" | "failed";
export type Extras = Record<string, string | number | boolean>;

export interface JobTrace {
  traceId: string;
  shortId: string;
  events: TraceEventView[];
  byStage: Partial<Record<KnownStage, TraceEventView>>;
  extras: Extras;
  firstSeq: number;
  lastSeq: number;
  currentStage: KnownStage | null;
  state: JobState;
  startedAt: string;
  lastEventAt: string;
  lastAtMs: number | null;
  endedAt?: string;
  totalMs?: number;
  origin?: string;
  printer?: string;
  pages?: number;
  queuePos?: number;
  code?: string;
  detail?: string;
}

export interface StageSlot {
  stage: KnownStage;
  event: TraceEventView | null;
}
export type RangePreset = "15" | "60" | "1440" | "custom";

export interface WaterfallSegment {
  stage: KnownStage | "other";
  ms: number;
  ok: boolean;
  share: number;
  startMs: number;
  queuePos?: number;
}

export interface JobFilters {
  stages?: string[] | null;
  outcome?: JobState | null;
  printer?: string | null;
  origin?: string | null;
  keyword?: string | null;
  fromMs?: number | null;
}

export interface CapOptions {
  maxRows?: number;
  maxTraces?: number;
}

export interface MergeResult {
  events: TraceEventView[];
  lastSeq: number;
}

export interface FollowInput {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
  wasFollowing: boolean;
}

export const STAGES: readonly KnownStage[];
export const STAGE_KEYS: Record<KnownStage, string>;

export function traceShortId(traceId: string): string;
export function formatMs(value?: number | null): string;
export function stageFieldPairs(
  event: TraceEventView | null | undefined,
  limit?: number,
): Array<{ key: string; value: string }>;
export function aggregateJobs(events: readonly TraceEventView[], limit?: number): JobTrace[];
export function stageTimeline(job: JobTrace | null | undefined): StageSlot[];
export function waterfall(job: JobTrace | null | undefined): WaterfallSegment[];
export function applyCaps(events: readonly TraceEventView[], options?: CapOptions): TraceEventView[];
export function mergeEvents(
  current: readonly TraceEventView[],
  incoming: readonly TraceEventView[],
  cursor?: number,
  options?: CapOptions,
): MergeResult;
export function filterJobs(jobs: readonly JobTrace[], filters?: JobFilters): JobTrace[];
export function orderJobs(jobs: readonly JobTrace[]): JobTrace[];
export function facetValues(jobs: readonly JobTrace[]): { printers: string[]; origins: string[] };
export function traceNeighbors(
  jobs: readonly JobTrace[],
  traceId: string,
): { prev: string | null; next: string | null };
export function serializeTrace(job: JobTrace | null | undefined, format?: "json" | "jsonl"): string;
export const RANGE_PRESETS: Readonly<Record<"15" | "60" | "1440", number>>;
export function rangeMinutes(
  range: RangePreset | string,
  customFrom: string,
  now?: number,
): number | null;
export function nearBottom(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number,
  margin?: number,
): boolean;
export function followAfterScroll(input: FollowInput): boolean;
