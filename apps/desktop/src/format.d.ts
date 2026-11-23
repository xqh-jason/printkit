export function localDay(offsetDays?: number, now?: Date): string;
export function localIsoDay(offsetDays?: number, now?: Date): string;
export function debounce<T extends (...args: never[]) => void>(fn: T, waitMs?: number): (...args: Parameters<T>) => void;
