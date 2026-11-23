<script setup lang="ts">
import { Document } from "@element-plus/icons-vue";

defineProps<{ title: string; description?: string; actionLabel?: string }>();
const emit = defineEmits<{ action: [] }>();
</script>

<template>
  <div class="pk-empty">
    <span class="pk-empty-mark" aria-hidden="true">
      <el-icon><Document /></el-icon>
    </span>
    <h3>{{ title }}</h3>
    <p v-if="description">{{ description }}</p>
    <el-button v-if="actionLabel" type="primary" plain @click="emit('action')">
      {{ actionLabel }}
    </el-button>
  </div>
</template>
