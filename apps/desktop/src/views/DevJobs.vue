<script setup lang="ts">
import { computed, nextTick, onActivated, onDeactivated, onMounted, onUnmounted, ref, watch } from "vue";

import "../styles/observability.css";
import { logsEvents } from "../api";
import { i18n } from "../i18n";
import type { Snapshot } from "../types";
import { useJobTraces } from "../composables/useJobTraces";
import { useLiveTail } from "../composables/useLiveTail";
import { useMediaQuery } from "../composables/useMediaQuery";
import { useMotion } from "../composables/useMotion";
import { debounce } from "../format";
import FilterFacets from "../components/observability/FilterFacets.vue";
import { usePluginLease } from "../composables/usePluginLease";
import { orderJobs } from "../components/observability/jobTraces";
import JobCard from "../components/observability/JobCard.vue";
import Skeleton from "../components/Skeleton.vue";
import LiveBar from "../components/observability/LiveBar.vue";
import TraceDetail from "../components/observability/TraceDetail.vue";
import { defaultFacets, type FacetState } from "../components/observability/facets";
import {
  facetValues,
  filterJobs,
  followAfterScroll,
  rangeMinutes,
  traceNeighbors,
} from "../components/observability/jobTraces";

// Live job viewing counts as `inUse`, so disabling observability is refused
// until this page closes or its 60 s lease expires.
usePluginLease("observability");
defineProps<{ snapshot?: Snapshot | null }>();

const { t } = i18n;
const { reduce } = useMotion();
const narrow = useMediaQuery("(max-width: 779px)");

const facets = ref<FacetState>(defaultFacets());

const windowMinutes = computed(() => rangeMinutes(facets.value.range, facets.value.customFrom));
const customInvalid = computed(
  () => facets.value.range === "custom" && windowMinutes.value === null,
);
const minutes = computed(() => windowMinutes.value ?? 60);

const tail = useLiveTail({
  fetch: (since) => logsEvents({ minutes: minutes.value, since, limit: 2000 }),
  maxRows: 500,
  maxTraces: 200,
});
const { events, paused, initialLoading, error: tailError, lastSeq } = tail;

const { jobs } = useJobTraces(events, 200);
const values = computed(() => facetValues(jobs.value));

/** Oldest first by first observation: polling updates text in place and new
 *  traces append at the bottom; a running trace never jumps the queue. */
const orderedJobs = computed(() =>
  orderJobs(
    filterJobs(jobs.value, {
      stages: facets.value.stages,
      outcome: facets.value.outcome || null,
      printer: facets.value.printer || null,
      origin: facets.value.origin || null,
      keyword: facets.value.keyword,
    }),
  ),
);

const filteredOut = computed(() => jobs.value.length > 0 && orderedJobs.value.length === 0);

const selectedId = ref("");
const detailOpen = ref(false);
const selected = computed(
  () => orderedJobs.value.find((job) => job.traceId === selectedId.value) ?? null,
);
const neighbors = computed(() => traceNeighbors(orderedJobs.value, selectedId.value));

watch(narrow, (value) => {
  if (!value) detailOpen.value = false;
});
watch(orderedJobs, (list) => {
  if (selectedId.value && !list.some((job) => job.traceId === selectedId.value)) {
    selectedId.value = "";
    detailOpen.value = false;
  }
  if (!selectedId.value && list.length > 0) selectedId.value = list[0].traceId;
});

// The window is a server-side filter: changing it restarts the tail from
// scratch. Facet values stay in memory and never refetch.
const resetForWindow = debounce(() => tail.reset(), 300);
watch(() => facets.value.range, () => tail.reset());
watch(() => facets.value.customFrom, resetForWindow);

const listEl = ref<HTMLDivElement | null>(null);
const following = ref(true);
const hidden = ref(document.visibilityState !== "visible");
const deactivated = ref(false);

function onVisibility(): void {
  hidden.value = document.visibilityState !== "visible";
}

function scrollToBottom(behavior: ScrollBehavior): void {
  const element = listEl.value;
  if (!element) return;
  element.scrollTo({ top: element.scrollHeight, behavior });
}

function onListScroll(): void {
  const element = listEl.value;
  if (!element) return;
  following.value = followAfterScroll({
    scrollTop: element.scrollTop,
    clientHeight: element.clientHeight,
    scrollHeight: element.scrollHeight,
    wasFollowing: following.value,
  });
}

function jumpLatest(): void {
  following.value = true;
  nextTick(() => scrollToBottom(reduce.value ? "auto" : "smooth"));
}

watch(lastSeq, () => {
  // Tail updates must not animate a smooth scroll every second.
  if (following.value) nextTick(() => scrollToBottom("auto"));
});

function scrollToCard(traceId: string): void {
  const element = listEl.value;
  if (!element) return;
  const card = element.querySelector<HTMLElement>(`[data-trace="${traceId}"]`);
  if (!card) return;
  const target = card.offsetTop - element.clientHeight / 2 + card.clientHeight / 2;
  element.scrollTo({ top: Math.max(0, target), behavior: reduce.value ? "auto" : "smooth" });
}

function selectTrace(traceId: string): void {
  selectedId.value = traceId;
  if (narrow.value) detailOpen.value = true;
}

/** Prev/next in the detail also bring the corresponding card into view. */
function selectFromDetail(traceId: string): void {
  selectTrace(traceId);
  nextTick(() => scrollToCard(traceId));
}

function closeDetail(): void {
  selectedId.value = "";
  detailOpen.value = false;
}

function togglePause(): void {
  tail.setPaused(!paused.value);
}

onMounted(() => document.addEventListener("visibilitychange", onVisibility));
onUnmounted(() => document.removeEventListener("visibilitychange", onVisibility));
onActivated(() => {
  deactivated.value = false;
});
onDeactivated(() => {
  deactivated.value = true;
});
</script>

<template>
  <section class="page obs-page">
    <el-alert
      v-if="tailError"
      :title="tailError"
      type="error"
      :closable="false"
      show-icon
      class="page-alert"
    />

    <LiveBar
      class="obs-toolbar"
      :paused="paused"
      :hidden="hidden || deactivated"
      :loading="initialLoading"
      :following="following"
      :last-seq="lastSeq"
      :row-count="events.length"
      :trace-count="jobs.length"
      @toggle-pause="togglePause"
      @jump-latest="jumpLatest"
    />

    <FilterFacets
      v-model="facets"
      :printers="values.printers"
      :origins="values.origins"
      :row-count="events.length"
      :custom-invalid="customInvalid"
    />

    <div class="obs-grid" :class="{ 'obs-grid-narrow': narrow }">
      <div class="obs-list-wrap">
        <div ref="listEl" class="obs-list" @scroll="onListScroll">
          <Skeleton
            v-if="initialLoading"
            :loading="true"
            variant="list"
            :rows="5"
            :label="t('common.loading')"
          />
          <div v-else-if="!orderedJobs.length" class="obs-empty">
            <strong>
              {{ filteredOut ? t("jobs.empty.filteredTitle") : t("jobs.empty.title") }}
            </strong>
            <span class="obs-muted">
              {{ filteredOut ? t("jobs.empty.filteredHint") : t("jobs.empty.hint") }}
            </span>
          </div>
          <div v-else class="obs-card-list">
            <JobCard
              v-for="job in orderedJobs"
              :key="job.traceId"
              :job="job"
              :selected="job.traceId === selectedId"
              @select="selectTrace"
            />
          </div>
        </div>
      </div>

      <aside v-if="!narrow" class="obs-detail-col">
        <TraceDetail
          v-if="selected"
          :job="selected"
          :prev="neighbors.prev"
          :next="neighbors.next"
          @close="closeDetail"
          @select="selectFromDetail"
        />
        <div v-else class="obs-detail-empty">
          <strong>{{ t("jobs.selectHint") }}</strong>
          <span class="obs-muted">{{ t("jobs.selectHintDetail") }}</span>
        </div>
      </aside>
    </div>

    <el-drawer
      v-if="narrow"
      v-model="detailOpen"
      direction="rtl"
      size="100%"
      :with-header="false"
      class="obs-drawer"
      @closed="selectedId = ''"
    >
      <TraceDetail
        v-if="selected"
        :job="selected"
        :prev="neighbors.prev"
        :next="neighbors.next"
        @close="closeDetail"
        @select="selectFromDetail"
      />
    </el-drawer>
  </section>
</template>
