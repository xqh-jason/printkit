/** Retention validation shared by the Settings form and its tests. */
export const RETENTION_MIN_DAYS = 1;
export const RETENTION_MAX_DAYS = 3650;

export function retentionDaysValid(value) {
  return (
    typeof value === "number" &&
    Number.isInteger(value) &&
    value >= RETENTION_MIN_DAYS &&
    value <= RETENTION_MAX_DAYS
  );
}

/** Backend source strings (`cli|env|file|db|default`) -> catalogue keys. */
export const RETENTION_SOURCE_KEYS = Object.freeze({
  cli: "business.settings.retentionSource.cli",
  env: "business.settings.retentionSource.env",
  file: "business.settings.retentionSource.file",
  db: "business.settings.retentionSource.db",
  default: "business.settings.retentionSource.default",
  unknown: "business.settings.retentionSource.unknown",
});

export function retentionSourceKey(source) {
  return RETENTION_SOURCE_KEYS[source] ?? RETENTION_SOURCE_KEYS.unknown;
}
