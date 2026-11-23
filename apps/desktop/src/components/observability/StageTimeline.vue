<script setup lang="ts">
import { computed } from "vue";

import { i18n } from "../../i18n";
import JobStateDot from "./JobStateDot.vue";
import { STAGE_KEYS, formatMs, stageFieldPairs, stageTimeline } from "./jobTraces";
import type { JobTrace, StageSlot } from "./jobTraces";

const props = defineProps<{ job: JobTrace }>();
const { t, formatTime } = i18n;

const slots = computed(() => stageTimeline(props.job));
const activeStage = computed(() => (props.job.state === "running" ? props.job.currentStage : null));

function labelOf(stage: StageSlot["stage"]): string {
  const key = STAGE_KEYS[stage];
  return key ? t(key) : stage;
}

function isActive(slot: StageSlot): boolean {
  return slot.stage === activeStage.value && slot.event !== null;
}

/** RFC 3339 may carry nanoseconds; keep the first three fraction digits so
 *  WebKit's Date parser stays happy. */
function timeOf(at: string): string {
  return formatTime(at.replace(/(\.\d{3})\d+/, "$1"));
}

function queueOf(slot: StageSlot): number | undefined {
  return slot.event?.queuePos ?? (slot.stage === "queue" ? props.job.queuePos : undefined);
}
</script>

<template>
  <div class="obs-timeline">
    <div class="obs-section-head">
      <span class="obs-section-title">{{ t("jobs.timeline.title") }}</span>
      <span class="obs-num">{{ formatMs(job.totalMs) }}</span>
    </div>

    <div class="obs-stage-list">
      <div
        v-for="slot in slots"
        :key="slot.stage"
        class="obs-stage-row"
        :class="{
          'is-active': isActive(slot),
          'is-failed': slot.event !== null && !slot.event.ok,
          'is-missing': slot.event === null,
          'is-queue': slot.event !== null && slot.stage === 'queue',
        }"
      >
        <div class="obs-stage-rail" aria-hidden="true">
          <span class="obs-stage-dot" :class="`obs-stage-${slot.stage}`" />
          <span class="obs-stage-line" />
        </div>

        <div class="obs-stage-body">
          <div class="obs-stage-head">
            <span class="obs-stage-name">{{ labelOf(slot.stage) }}</span>
            <span v-if="slot.event && typeof slot.event.durMs === 'number'" class="obs-num">
              {{ formatMs(slot.event.durMs) }}
            </span>
            <span v-else-if="slot.event" class="obs-muted">{{ formatMs(undefined) }}</span>
            <span v-else class="obs-muted">{{ t("jobs.timeline.missing") }}</span>

            <JobStateDot
              v-if="slot.event"
              :state="slot.event.ok ? 'ok' : 'failed'"
              :label="slot.event.ok ? t('jobs.timeline.ok') : t('jobs.timeline.failed')"
            />
            <span v-if="slot.event?.code" class="obs-code">{{ slot.event.code }}</span>
            <span v-if="queueOf(slot) !== undefined" class="obs-queue">
              {{ t("jobs.timeline.queuePos", { pos: queueOf(slot) }) }}
            </span>
          </div>

          <div v-if="slot.event" class="obs-stage-meta">
            <span class="obs-time">{{ timeOf(slot.event.at) }}</span>
            <span v-for="pair in stageFieldPairs(slot.event)" :key="pair.key" class="obs-field"
              >{{ pair.key }}={{ pair.value }}</span
            >
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
