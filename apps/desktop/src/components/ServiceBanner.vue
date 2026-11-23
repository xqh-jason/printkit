<script setup lang="ts">
import { computed, ref } from "vue";
import { Refresh } from "@element-plus/icons-vue";

import { startService } from "../api";
import { describeError, i18n } from "../i18n";
import type { AutoStartState, Snapshot } from "../types";

const props = defineProps<{ snapshot: Snapshot | null }>();
const emit = defineEmits<{ retry: []; refresh: [] }>();
const { t, errorText } = i18n;

const autoStart = computed(() => props.snapshot?.autoStart);
const state = computed(() => props.snapshot?.status.state ?? "stopped");
const reachable = computed(() => Boolean(props.snapshot?.status.reachable));

const visible = computed(() => {
  if (!props.snapshot) return false;
  if (state.value === "stopped" || !reachable.value) return true;
  const auto = autoStart.value?.state ?? "idle";
  return auto !== "idle" && auto !== "started";
});

const AUTO_MESSAGES: Record<AutoStartState, string> = {
  idle: "compact.serviceDown",
  attempting: "service.banner.attempting",
  started: "service.banner.running",
  failed: "service.banner.failed",
  skippedExternal: "service.banner.skippedExternal",
  skippedIncompatible: "service.banner.skippedIncompatible",
  skippedConfig: "service.banner.skippedConfig",
  disabled: "service.banner.disabled",
};

const title = computed(() => {
  const auto = autoStart.value?.state ?? "idle";
  const known = AUTO_MESSAGES[auto] ?? "compact.serviceDown";
  if (state.value === "stopped" || !reachable.value) {
    return t(state.value === "stopped" ? "compact.serviceDown" : "service.banner.unreachable");
  }
  return t(known);
});

const startError = ref("");
const detail = computed(() => [autoStart.value?.error, startError.value].filter(Boolean).join(" · "));

const type = computed(() => {
  const auto = autoStart.value?.state ?? "idle";
  if (auto === "failed" || auto === "skippedIncompatible") return "error";
  return "warning";
});

const canRetry = computed(() => autoStart.value?.state === "failed");
const canStart = computed(() => !canRetry.value && (state.value === "stopped" || !reachable.value));
const starting = ref(false);

function retry(): void {
  emit("retry");
}

async function start(): Promise<void> {
  starting.value = true;
  startError.value = "";
  try {
    await startService();
    emit("refresh");
  } catch (reason) {
    startError.value = errorText(describeError(reason));
  } finally {
    starting.value = false;
  }
}
</script>

<template>
  <div v-if="visible" class="service-banner page-alert">
    <el-alert
      :title="title"
      :description="detail"
      :type="type"
      :closable="false"
      show-icon
    />
    <el-button v-if="canRetry" type="primary" plain size="small" @click="retry">
      {{ t("business.overview.retryStart") }}
    </el-button>
    <el-button v-else-if="canStart" type="primary" plain size="small" :loading="starting" @click="start">
      {{ t("service.banner.start") }}
    </el-button>
    <el-button v-else size="small" :icon="Refresh" @click="emit('refresh')">
      {{ t("shell.refresh") }}
    </el-button>
  </div>
</template>
