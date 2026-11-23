/** Local calendar day as `YYYY-MM-DD`; never UTC, so the day does not shift. */
export function localDay(offsetDays = 0, now = new Date()) {
  const date = new Date(now);
  date.setDate(date.getDate() + offsetDays);
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

/**
 * Local calendar midnight as RFC 3339 with an explicit offset. Records treat
 * `from` as inclusive and `to` as exclusive, so `to` must be the next local
 * midnight; a bare `YYYY-MM-DD` is parsed as UTC midnight and drops every
 * record after 00:00 UTC on the selected day.
 */
export function localIsoDay(offsetDays = 0, now = new Date()) {
  const date = new Date(now);
  date.setHours(0, 0, 0, 0);
  date.setDate(date.getDate() + offsetDays);
  const offset = -date.getTimezoneOffset();
  const sign = offset >= 0 ? "+" : "-";
  const hours = String(Math.floor(Math.abs(offset) / 60)).padStart(2, "0");
  const minutes = String(Math.abs(offset) % 60).padStart(2, "0");
  return `${localDay(0, date)}T00:00:00${sign}${hours}:${minutes}`;
}

/** Trailing-edge debounce for filter inputs. */
export function debounce(fn, waitMs = 300) {
  let timer;
  return (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      timer = undefined;
      fn(...args);
    }, waitMs);
  };
}
