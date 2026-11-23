export interface RecordColumns {
  narrow: boolean;
  tiny: boolean;
  printer: boolean;
  copies: boolean;
  pages: boolean;
  error: boolean;
  template: boolean;
  trace: boolean;
  duration: boolean;
}

export const NARROW_AT: number;
export const TINY_AT: number;

export function visibleRecordColumns(width: number): RecordColumns;

export type EmptyStateKind = "loading" | "rows" | "empty" | "noMatch";

export function resolveEmptyState(input: {
  loading: boolean;
  count: number;
  hasFilters: boolean;
}): EmptyStateKind;

export interface LogColumns {
  seq: boolean;
  stage: boolean;
  time: boolean;
  trace: boolean;
  result: boolean;
  duration: boolean;
}

export function visibleLogColumns(width: number): LogColumns;

export function logResultKey(ok: boolean): "logs.result.ok" | "logs.result.failed";
