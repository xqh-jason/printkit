<script setup lang="ts">
import { computed } from "vue";

import type { StateKind } from "./state";

const props = withDefaults(defineProps<{ kind: StateKind; label?: string; size?: number }>(), {
  label: "",
  size: 10,
});

const classes = computed(() => [
  "pk-state",
  `pk-state--${props.kind}`,
  { "pk-state--waiting": props.kind === "pending" },
]);
const style = computed(() => ({ "--pk-state-size": `${props.size}px` }));
</script>

<template>
  <span :class="classes" :style="style" :role="label ? undefined : 'img'" :aria-label="label ? undefined : kind">
    <svg class="pk-state-svg" :class="{ 'pk-breathe': kind === 'pending' }" viewBox="0 0 10 10" aria-hidden="true" focusable="false">
      <circle v-if="kind === 'success'" cx="5" cy="5" r="4" class="pk-state-fill" />
      <template v-else-if="kind === 'failed'">
        <circle cx="5" cy="5" r="4" class="pk-state-ring" />
        <path d="M3.35 3.35 6.65 6.65M6.65 3.35 3.35 6.65" class="pk-state-cross" />
      </template>
      <template v-else-if="kind === 'warning'">
        <path d="M5 1.2 9.2 8.4H0.8Z" class="pk-state-triangle" />
        <path d="M5 3.9v2.1" class="pk-state-cross" />
        <circle cx="5" cy="7.6" r="0.55" class="pk-state-fill" />
      </template>
      <template v-else-if="kind === 'pending'">
        <circle cx="5" cy="5" r="4" class="pk-state-ring" />
        <circle cx="5" cy="5" r="1.7" class="pk-state-fill" />
      </template>
      <circle v-else cx="5" cy="5" r="4" class="pk-state-ring" />
    </svg>
    <span v-if="label" class="pk-state-label">{{ label }}</span>
  </span>
</template>
