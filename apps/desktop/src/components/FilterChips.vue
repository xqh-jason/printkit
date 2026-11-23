<script setup lang="ts">
interface FilterChip {
  id: string;
  label: string;
}

defineProps<{ chips: FilterChip[]; clearLabel?: string }>();
const emit = defineEmits<{ remove: [id: string]; clear: [] }>();
</script>

<template>
  <div v-if="chips.length" class="pk-filter-chips">
    <button
      v-for="chip in chips"
      :key="chip.id"
      type="button"
      class="pk-filter-chip"
      :aria-label="chip.label"
      @click="emit('remove', chip.id)"
    >
      <span>{{ chip.label }}</span>
      <span class="pk-filter-chip-x" aria-hidden="true">×</span>
    </button>
    <el-button v-if="clearLabel" text size="small" @click="emit('clear')">{{ clearLabel }}</el-button>
  </div>
</template>
