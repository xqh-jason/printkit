<script setup lang="ts">
import { computed } from "vue";

import { i18n } from "../../i18n";
import { STAGE_KEYS, formatMs, waterfall } from "./jobTraces";
import type { JobTrace, WaterfallSegment } from "./jobTraces";

const props = defineProps<{ job: JobTrace }>();
const { t } = i18n;

const segments = computed(() => waterfall(props.job));

const STAGE_LABEL_KEYS: Record<string, string> = {
  ...STAGE_KEYS,
  other: "jobs.waterfall.other",
};

function labelOf(stage: WaterfallSegment["stage"]): string {
  const key = STAGE_LABEL_KEYS[stage];
  return key ? t(key) : stage;
}

function shareOf(segment: WaterfallSegment): string {
  return `${(segment.share * 100).toFixed(2)}%`;
}

function titleOf(segment: WaterfallSegment): string {
  const base = `${labelOf(segment.stage)} · ${formatMs(segment.ms)}`;
  return segment.queuePos === undefined
    ? base
    : `${base} · ${t("jobs.waterfall.queuePos", { pos: segment.queuePos })}`;
}
</script>

<template>
  <div class="obs-waterfall">
    <div class="obs-waterfall-head">
      <span class="obs-section-title">{{ t("jobs.waterfall.title") }}</span>
      <span v-if="typeof job.totalMs === 'number'" class="obs-num obs-waterfall-total">
        {{ formatMs(job.totalMs) }}
      </span>
    </div>

    <div
      v-if="segments.length"
      class="obs-waterfall-track"
      role="img"
      :aria-label="t('jobs.waterfall.title')"
    >
      <div
        v-for="segment in segments"
        :key="segment.stage"
        class="obs-waterfall-seg"
        :class="`obs-stage-${segment.stage}`"
        :style="{ width: shareOf(segment) }"
        :title="titleOf(segment)"
      >
        <span v-if="segment.stage === 'queue' && segment.queuePos !== undefined" class="obs-waterfall-pos">
          #{{ segment.queuePos }}
        </span>
      </div>
    </div>
    <div v-else class="obs-muted">{{ t("jobs.waterfall.empty") }}</div>

    <div v-if="segments.length" class="obs-waterfall-legend">
      <span v-for="segment in segments" :key="segment.stage" class="obs-legend-item">
        <span class="obs-legend-chip" :class="`obs-stage-${segment.stage}`" aria-hidden="true" />
        <span>{{ labelOf(segment.stage) }}</span>
        <span class="obs-num">{{ formatMs(segment.ms) }}</span>
        <span v-if="segment.stage === 'queue' && segment.queuePos !== undefined" class="obs-queue">
          {{ t("jobs.card.queue", { pos: segment.queuePos }) }}
        </span>
      </span>
    </div>
  </div>
</template>
