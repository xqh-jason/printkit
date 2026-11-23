<script setup lang="ts">
import { computed } from "vue";

import { i18n } from "../../i18n";
import JobStateDot from "./JobStateDot.vue";
import { STAGE_KEYS, formatMs } from "./jobTraces";
import type { JobTrace } from "./jobTraces";

const props = defineProps<{ job: JobTrace; selected: boolean }>();
const emit = defineEmits<{ select: [traceId: string] }>();
const { t } = i18n;

const stateLabel = computed(() => {
  if (props.job.state === "ok") return t("jobs.card.ok");
  if (props.job.state === "failed") return t("jobs.card.failed");
  return t("jobs.card.running");
});

const stageLabel = computed(() =>
  props.job.currentStage
    ? t(STAGE_KEYS[props.job.currentStage])
    : t("jobs.timeline.missing"),
);
</script>

<template>
  <button
    type="button"
    class="obs-card"
    :class="{ 'is-selected': selected }"
    :data-trace="job.traceId"
    :aria-pressed="selected"
    @click="emit('select', job.traceId)"
  >
    <span class="obs-card-head">
      <JobStateDot :state="job.state" :label="stateLabel" />
      <span class="obs-trace">{{ job.shortId }}</span>
      <span v-if="typeof job.totalMs === 'number'" class="obs-card-duration">
        {{ formatMs(job.totalMs) }}
      </span>
    </span>

    <span class="obs-card-meta">
      <span v-if="job.printer" class="obs-meta-item">{{ job.printer }}</span>
      <span v-if="job.origin" class="obs-meta-item">{{ job.origin }}</span>
      <span v-if="job.pages !== undefined" class="obs-meta-item">
        {{ t("jobs.card.pages", { count: job.pages }) }}
      </span>
      <span v-if="job.extras.copies !== undefined" class="obs-meta-item">
        {{ t("jobs.detail.copies") }} {{ job.extras.copies }}
      </span>
      <span v-if="job.extras.paper !== undefined" class="obs-meta-item">
        {{ t("jobs.detail.paper") }} {{ job.extras.paper }}
      </span>
      <span v-if="job.extras.template !== undefined" class="obs-meta-item">
        {{ t("jobs.detail.template") }} {{ job.extras.template }}
      </span>
    </span>

    <span class="obs-card-foot">
      <span class="obs-meta-label">{{ t("jobs.card.current") }}</span>
      <span class="obs-stage-chip" :class="job.currentStage ? `obs-stage-${job.currentStage}` : ''">
        {{ stageLabel }}
      </span>
      <span v-if="job.queuePos !== undefined" class="obs-queue">
        {{ t("jobs.card.queue", { pos: job.queuePos }) }}
      </span>
      <span v-if="job.code" class="obs-code">{{ job.code }}</span>
    </span>
  </button>
</template>
