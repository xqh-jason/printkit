<script setup lang="ts">
import { computed } from "vue";

import { i18n } from "../i18n";
import type { Snapshot } from "../types";

const props = defineProps<{ snapshot: Snapshot | null }>();
const { t } = i18n;

const label = computed(() => {
  switch (props.snapshot?.status.state) {
    case "runningOwned":
      return t("shell.service.state.runningOwned");
    case "runningExternal":
      return t("shell.service.state.runningExternal");
    default:
      return t("shell.service.state.stopped");
  }
});

const kind = computed(() => {
  switch (props.snapshot?.status.state) {
    case "runningOwned":
      return "success";
    case "runningExternal":
      return "warning";
    default:
      return "info";
  }
});
</script>

<template>
  <el-tag :type="kind" effect="light" round>{{ label }}</el-tag>
</template>
