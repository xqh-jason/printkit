<script setup lang="ts">
import { computed } from "vue";

import { i18n } from "../../i18n";

const props = defineProps<{
  paused: boolean;
  hidden: boolean;
  loading: boolean;
  following: boolean;
  lastSeq: number;
  rowCount: number;
  traceCount: number;
}>();
const emit = defineEmits<{ togglePause: []; jumpLatest: [] }>();
const { t, formatNumber } = i18n;

const stateText = computed(() => {
  if (props.hidden) return t("jobs.live.hidden");
  if (props.paused) return t("jobs.live.paused");
  return t("jobs.live.tailing");
});

const counts = computed(() =>
  t("jobs.live.counts", {
    rows: formatNumber(props.rowCount),
    traces: formatNumber(props.traceCount),
  }),
);
</script>

<template>
  <div class="obs-live" role="status" aria-live="polite">
    <span class="obs-live-dot" :class="{ 'is-off': paused || hidden }" aria-hidden="true" />
    <span class="obs-live-text">{{ stateText }}</span>
    <span class="obs-cursor">#{{ lastSeq }}</span>
    <span class="obs-muted">{{ counts }}</span>
    <span v-if="loading && rowCount === 0" class="obs-muted">{{ t("jobs.live.loading") }}</span>

    <span class="obs-live-actions">
      <button
        v-if="!following"
        type="button"
        class="obs-jump-btn"
        @click="emit('jumpLatest')"
      >
        {{ t("jobs.follow.jump") }}
      </button>
      <el-button size="small" :disabled="hidden" @click="emit('togglePause')">
        {{ paused ? t("jobs.live.resume") : t("jobs.live.pause") }}
      </el-button>
    </span>
  </div>
</template>
