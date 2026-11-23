import type { RecordStatsBucket, RecordsSummary } from "./types";

export interface OverviewWindow {
  days: string[];
  from: string;
  to: string;
}

export interface StatusDistribution {
  total: number;
  done: number;
  failed: number;
  cancelled: number | null;
  cancelledKnown: boolean;
}

export function overviewWindow(now?: Date): OverviewWindow;
export function fillDaySeries(
  buckets: RecordStatsBucket[] | null | undefined,
  days: string[],
): RecordStatsBucket[];
export function statusDistribution(summary: RecordsSummary | null | undefined): StatusDistribution;
export function failureRate(summary: RecordsSummary | null | undefined): number | null;
export function successRate(summary: RecordsSummary | null | undefined): number | null;
export function topPrinters(
  buckets: RecordStatsBucket[] | null | undefined,
  limit?: number,
): RecordStatsBucket[];
export function recordsRequestable(serviceDown: boolean, recordsOn: boolean): boolean;
export function localDayFromKey(key: string): Date;

export interface TodayMetrics {
  todayDone: number;
  todayFailed: number;
  todayCancelled: number;
}

export function todayMetrics(summary: RecordsSummary | null | undefined): TodayMetrics;
