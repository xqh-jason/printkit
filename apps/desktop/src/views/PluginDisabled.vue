<script setup lang="ts">
import { computed, ref, toRef } from "vue";
import { ElMessage } from "element-plus";
import { Warning } from "@element-plus/icons-vue";

import { pluginEnableOwned } from "../api";
import { describeError, i18n } from "../i18n";
import { pluginDisabledKind } from "../plugins/availability.js";
import type { Snapshot } from "../types";
import { usePlugins } from "../composables/usePlugins";

const props = defineProps<{
  snapshot: Snapshot | null;
  pluginId: "records" | "observability";
  canRecover: boolean;
}>();
const emit = defineEmits<{ recovered: []; refresh: []; openPage: [id: string] }>();

const { t, errorText } = i18n;
const snapshot = toRef(props, "snapshot");
const { plugins, label } = usePlugins(snapshot);
const busy = ref(false);
const error = ref("");

const kind = computed(() => pluginDisabledKind(props.snapshot, props.pluginId) ?? "disabled");
const plugin = computed(() => plugins.value.find((item) => item.id === props.pluginId) ?? null);
const pluginState = computed(() => plugin.value?.state ?? null);
const title = computed(() =>
  plugin.value ? label(plugin.value) : t(`plugins.${props.pluginId}.name`),
);
const reason = computed(() => plugin.value?.reason ?? "");
const dataDir = computed(() => props.snapshot?.config?.dataDir ?? "");
const cli = computed(() => props.snapshot?.cliPath ?? "printkit");
const command = computed(() => {
  const dir = dataDir.value ? ` --data-dir "${dataDir.value}"` : "";
  return `${cli.value} plugin enable ${props.pluginId}${dir}`;
});
const external = computed(() => props.snapshot?.status.state === "runningExternal");

async function enable(): Promise<void> {
  busy.value = true;
  error.value = "";
  try {
    await pluginEnableOwned(props.pluginId);
    emit("recovered");
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    busy.value = false;
  }
}

async function copy(): Promise<void> {
  try {
    await navigator.clipboard.writeText(command.value);
    ElMessage.success(t("shell.copied"));
  } catch {
    ElMessage.warning(t("shell.copyFailed"));
  }
}
</script>

<template>
  <section
    class="page plugin-disabled"
    :data-plugin-state="kind"
    :data-feature="pluginId"
  >
    <el-alert
      v-if="error"
      :title="error"
      type="error"
      :closable="false"
      show-icon
      class="page-alert"
    />
    <div class="plugin-disabled-card">
      <div class="plugin-disabled-icon" aria-hidden="true">
        <el-icon><Warning /></el-icon>
      </div>
      <h2 class="plugin-disabled-title">{{ title }}</h2>
      <p
        v-if="kind === 'disabled' && pluginState && pluginState !== 'off'"
        class="plugin-disabled-text"
      >
        {{ t(`plugins.disabled.state.${pluginState}`) }}
      </p>
      <p v-else-if="kind === 'disabled'" class="plugin-disabled-text">
        {{ t("plugins.disabled.hint", { plugin: title }) }}
      </p>
      <p v-else-if="kind === 'unreachable'" class="plugin-disabled-text">
        {{ t("plugins.disabled.unreachable") }}
      </p>
      <p v-else class="plugin-disabled-text">{{ t("plugins.disabled.unknown") }}</p>
      <p v-if="reason" class="plugin-disabled-reason">{{ reason }}</p>
      <p v-if="kind === 'disabled' && external" class="plugin-disabled-text">
        {{ t("plugins.disabled.external") }}
      </p>
      <p v-if="kind === 'disabled' && pluginState === 'off'" class="plugin-disabled-text">
        {{ t("plugins.disabled.keepData") }}
      </p>

      <div class="plugin-disabled-actions">
        <el-button
          v-if="canRecover && kind === 'disabled' && !external"
          type="primary"
          :loading="busy"
          @click="enable"
        >
          {{ t("plugins.enable", { plugin: title }) }}
        </el-button>
        <el-button v-if="!external" @click="emit('openPage', 'plugins')">
          {{ t("plugins.openPage") }}
        </el-button>
        <el-button v-if="kind !== 'disabled'" @click="emit('refresh')">
          {{ t("plugins.refreshState") }}
        </el-button>
      </div>

      <div class="plugin-cli">
        <p class="plugin-cli-label">{{ t("plugins.disabled.cliHint") }}</p>
        <div class="plugin-cli-row">
          <code>{{ command }}</code>
          <el-button text size="small" @click="copy">{{ t("shell.copy") }}</el-button>
        </div>
      </div>
    </div>
  </section>
</template>
