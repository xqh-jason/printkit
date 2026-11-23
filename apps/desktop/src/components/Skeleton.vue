<script setup lang="ts">
import { onUnmounted, ref, watch } from "vue";

const props = withDefaults(
  defineProps<{
    loading: boolean;
    rows?: number;
    variant?: "table" | "list" | "detail";
    delay?: number;
    label?: string;
  }>(),
  { rows: 5, variant: "table", delay: 130, label: "" },
);

const visible = ref(false);
let timer: number | undefined;

function clear(): void {
  if (timer !== undefined) {
    window.clearTimeout(timer);
    timer = undefined;
  }
}

watch(
  () => props.loading,
  (value) => {
    clear();
    if (!value) {
      visible.value = false;
      return;
    }
    // A request that settles under 130ms never flashes a skeleton.
    timer = window.setTimeout(() => {
      timer = undefined;
      visible.value = true;
    }, Math.max(0, props.delay));
  },
  { immediate: true },
);

onUnmounted(clear);

const tableCells = [17, 22, 10, 12, 9, 12];
</script>

<template>
  <div
    v-if="visible"
    class="pk-skeleton"
    :class="`pk-skeleton--${variant}`"
    aria-busy="true"
    :aria-label="label || undefined"
  >
    <div v-if="variant === 'table'" class="pk-skeleton-table-head">
      <span v-for="(cell, index) in tableCells" :key="index" :style="{ width: `${cell}%` }" />
    </div>
    <div v-for="row in rows" :key="row" class="pk-skeleton-row">
      <span v-if="variant !== 'table'" class="pk-skeleton-mark" />
      <span
        v-for="(cell, index) in variant === 'table' ? tableCells : variant === 'list' ? [34, 18] : [46, 62]"
        :key="index"
        class="pk-skeleton-line"
        :style="{ width: `${cell}%` }"
      />
    </div>
  </div>
</template>
