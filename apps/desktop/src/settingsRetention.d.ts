export const RETENTION_MIN_DAYS: number;
export const RETENTION_MAX_DAYS: number;
export const RETENTION_SOURCE_KEYS: Readonly<Record<string, string>>;
export function retentionDaysValid(value: unknown): value is number;
export function retentionSourceKey(source: unknown): string;
