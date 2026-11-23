import {
  computed,
  onActivated,
  onDeactivated,
  onMounted,
  onUnmounted,
  ref,
  shallowRef,
} from "vue";

import { describeError } from "@printkit/i18n";

import { mergeEvents } from "../components/observability/jobTraces";
import { i18n } from "../i18n";
import type { TraceEventView } from "../types";

/**
 * Cursor-based live tail shared by the raw log page and the jobs page.
 *
 * One composable instance belongs to one page, so pausing the jobs page never
 * silences the log page. The backend replays events after `since`, and every
 * response is merged through `mergeEvents` (de-duplicate by `seq`, drop rows
 * at or below the cursor, then cap memory by whole traces).
 */
export interface LiveTailOptions {
  /** Query the backend; `since` is omitted for the first page of a window. */
  fetch: (since: number | undefined) => Promise<TraceEventView[]>;
  intervalMs?: number;
  /** Rows retained; the default keeps 500 events. */
  maxRows?: number;
  /** Traces retained; `0` disables the trace cap. Defaults to 200. */
  maxTraces?: number;
}

export function useLiveTail(options: LiveTailOptions) {
  const events = shallowRef<TraceEventView[]>([]);
  const lastSeq = ref(0);
  const paused = ref(false);
  const loading = ref(false);
  const error = ref("");
  const updatedAt = ref<number | null>(null);
  const attempted = ref(false);
  /** True only while the first page is in flight; polling never re-arms it. */
  const initialLoading = computed(() => loading.value && !attempted.value);

  const intervalMs = options.intervalMs ?? 1000;
  const maxRows = options.maxRows ?? 500;
  const maxTraces = options.maxTraces ?? 200;

  let timer: number | undefined;
  let running = false;
  let inFlight = false;
  let generation = 0;
  let disposed = false;

  function clearTimer(): void {
    if (timer !== undefined) window.clearInterval(timer);
    timer = undefined;
  }

  async function pull(): Promise<void> {
    if (inFlight || disposed) return;
    const mine = generation;
    inFlight = true;
    loading.value = true;
    try {
      const incoming = await options.fetch(lastSeq.value > 0 ? lastSeq.value : undefined);
      if (mine !== generation || disposed) return;
      const merged = mergeEvents(events.value, incoming, lastSeq.value, { maxRows, maxTraces });
      events.value = merged.events;
      lastSeq.value = merged.lastSeq;
      updatedAt.value = Date.now();
      error.value = "";
    } catch (reason) {
      if (mine === generation) error.value = i18n.errorText(describeError(reason));
    } finally {
      inFlight = false;
      if (mine === generation) {
        attempted.value = true;
        loading.value = false;
      }
    }
  }

  /** Start the interval and fetch once; safe to call repeatedly. */
  function start(): void {
    if (disposed) return;
    if (!running) {
      running = true;
      timer = window.setInterval(() => {
        if (paused.value || document.visibilityState !== "visible") return;
        void pull();
      }, intervalMs);
    }
    void pull();
  }

  /** Stop polling but keep the retained rows and the cursor. */
  function stop(): void {
    running = false;
    clearTimer();
  }

  function setPaused(next: boolean): void {
    if (paused.value === next) return;
    paused.value = next;
    if (next) stop();
    else start();
  }

  /** Clear content and cursor, then fetch the window again. */
  function reset(): void {
    generation += 1;
    events.value = [];
    lastSeq.value = 0;
    error.value = "";
    updatedAt.value = null;
    attempted.value = false;
    loading.value = false;
    void pull();
    start();
  }

  function onVisibility(): void {
    if (document.visibilityState !== "visible") return;
    if (running && !paused.value) void pull();
  }

  onMounted(() => {
    document.addEventListener("visibilitychange", onVisibility);
    start();
  });
  // A cached page resumes from its cursor; a deactivated page stops polling
  // and releases its timer until it comes back.
  onActivated(start);
  onDeactivated(stop);
  onUnmounted(() => {
    disposed = true;
    stop();
    document.removeEventListener("visibilitychange", onVisibility);
  });

  return {
    events,
    lastSeq,
    paused,
    loading,
    error,
    initialLoading,
    updatedAt,
    start,
    stop,
    reset,
    setPaused,
    refresh: pull,
  };
}
