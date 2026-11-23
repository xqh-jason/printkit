<script setup lang="ts">
import StateDot from "./StateDot.vue";
import type { StateKind } from "./state";

withDefaults(
  defineProps<{
    title: string;
    subtitle?: string;
    meta?: string;
    active?: boolean;
    state?: StateKind;
    stateLabel?: string;
  }>(),
  { subtitle: "", meta: "", active: false, stateLabel: "" },
);

const emit = defineEmits<{ select: [] }>();
</script>

<template>
  <button type="button" class="pk-list-row" :class="{ active }" @click="emit('select')">
    <StateDot v-if="state" :kind="state" :label="stateLabel" />
    <span class="pk-list-row-text">
      <strong>{{ title }}</strong>
      <small v-if="subtitle">{{ subtitle }}</small>
    </span>
    <span v-if="meta" class="pk-list-row-meta">{{ meta }}</span>
    <span v-if="$slots.trail" class="pk-list-row-trail">
      <slot name="trail" />
    </span>
  </button>
</template>
