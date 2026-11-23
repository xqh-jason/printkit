<script setup lang="ts">
import type { JobState } from "./jobTraces";

defineProps<{ state: JobState; label: string }>();
</script>

<template>
  <span class="obs-state" :class="`obs-state-${state}`">
    <!-- Failure is an x shape, not only a colour: a greyscale screenshot still
         distinguishes it from success. -->
    <svg v-if="state === 'failed'" class="obs-state-x" viewBox="0 0 16 16" aria-hidden="true">
      <path d="M4 4l8 8M12 4l-8 8" />
    </svg>
    <span v-else class="obs-state-dot" aria-hidden="true" />
    <span class="obs-state-label">{{ label }}</span>
  </span>
</template>
