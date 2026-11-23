<script setup lang="ts">
import { computed } from "vue";
import { ElMessage } from "element-plus";

import { i18n } from "../../i18n";
import { serviceAddress } from "../../sidebarAddress.js";
import StateDot from "../StateDot.vue";
import type { StateKind } from "../state";
import type { Snapshot } from "../../types";

const props = defineProps<{ snapshot: Snapshot | null }>();
const emit = defineEmits<{ openSettings: [] }>();
const { t } = i18n;

const view = computed(() => serviceAddress(props.snapshot?.status, props.snapshot?.config));

const kind = computed<StateKind>(() => {
  switch (view.value.state) {
    case "owned":
      return "success";
    case "external":
    case "no-port":
      return "warning";
    case "unreachable":
      return "failed";
    default:
      return "idle";
  }
});

const mainText = computed(() => {
  switch (view.value.state) {
    case "owned":
    case "external":
      return view.value.state === "external"
        ? `${view.value.address} ${t("shell.footer.externalShort")}`
        : view.value.address;
    case "unreachable":
      return t("shell.footer.unreachable");
    case "no-port":
      return view.value.configuredPort
        ? t("shell.footer.configuredPort", { port: view.value.configuredPort })
        : t("shell.footer.noPort");
    default:
      return t("shell.footer.stopped");
  }
});

const title = computed(() => {
  if (view.value.state === "external" && view.value.address) {
    return `${t("shell.footer.external")} · ${view.value.address} · ${t("shell.footer.copyShort")}`;
  }
  if (view.value.address) {
    return `${view.value.address} · ${t("shell.footer.copyAddress")}`;
  }
  if (view.value.state === "unreachable") {
    return `${t("shell.footer.unreachable")} · ${t("shell.footer.openSettings")}`;
  }
  if (view.value.configuredPort) {
    return `${t("shell.footer.configuredPort", { port: view.value.configuredPort })} · ${t("shell.footer.openSettings")}`;
  }
  return `${t("shell.footer.stopped")} · ${t("shell.footer.openSettings")}`;
});

async function activate(): Promise<void> {
  const address = view.value.address;
  if (!address) {
    emit("openSettings");
    return;
  }
  try {
    await navigator.clipboard.writeText(address);
    ElMessage.success(t("shell.footer.copied"));
  } catch {
    ElMessage.warning(t("shell.copyFailed"));
  }
}
</script>

<template>
  <button
    type="button"
    class="side-gw"
    :class="{ 'side-gw--down': !view.running }"
    :title="title"
    :aria-label="title"
    @click="activate"
  >
    <StateDot :kind="kind" :size="8" />
    <span class="side-gw-copy">{{ mainText }}</span>
    <span class="side-gw-action" aria-hidden="true">{{ view.address ? "⧉" : "›" }}</span>
  </button>
</template>

<style scoped>
.side-gw {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  min-width: 0;
  padding: 5px 6px;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--pk-text);
  text-align: left;
  cursor: pointer;
}

.side-gw:hover {
  background: var(--pk-surface-sunken);
}

.side-gw:focus-visible {
  outline: 2px solid var(--pk-focus);
  outline-offset: 1px;
}

.side-gw-copy {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
  font-weight: var(--pk-font-weight-medium);
  font-variant-numeric: tabular-nums;
}

.side-gw-action {
  flex: none;
  margin-left: auto;
  color: var(--pk-text-muted);
  font-size: 12px;
  opacity: 0;
}

.side-gw:hover .side-gw-action,
.side-gw:focus-visible .side-gw-action {
  opacity: 1;
}

.side-gw--down .side-gw-copy {
  color: var(--pk-text-muted);
  font-weight: normal;
}
</style>
