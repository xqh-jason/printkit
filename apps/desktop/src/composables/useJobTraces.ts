import { computed, type Ref } from "vue";

import { aggregateJobs } from "../components/observability/jobTraces";
import type { JobTrace } from "../components/observability/jobTraces";
import type { TraceEventView } from "../types";

/**
 * Aggregate the live tail into at most `limit` job cards. Pure work lives in
 * `jobTraces.js`; this layer only keeps Vue's dependency tracking.
 */
export function useJobTraces(events: Ref<TraceEventView[]>, limit = 200) {
  const jobs = computed<JobTrace[]>(() => aggregateJobs(events.value, limit));
  return { jobs };
}
