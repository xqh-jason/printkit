<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { ElMessage } from "element-plus";

import { exportLogs, logsEvents } from "../api";
import { describeError, i18n } from "../i18n";
import { debounce } from "../format";
import { useLiveTail } from "../composables/useLiveTail";
import { STAGES, STAGE_KEYS } from "../components/observability/jobTraces.js";
import { usePluginLease } from "../composables/usePluginLease";
import { useBreakpoint } from "../composables/useBreakpoint";
import { logResultKey, visibleLogColumns } from "../components/businessColumns.js";
import type { TraceEventView } from "../types";
import Skeleton from "../components/Skeleton.vue";

const { t, errorText, formatDateTime, formatNumber } = i18n;
const { width } = useBreakpoint();
function stageLabel(value: string): string {
  const key = (STAGE_KEYS as Record<string, string>)[value];
  return key ? t(key) : value;
}
const stage = ref("");
const logColumns = computed(() => visibleLogColumns(width.value));
// Live log viewing counts as `inUse`, so disabling observability is refused
// until this page closes or its 60 s lease expires.
usePluginLease("observability");
const traceId = ref("");
const field = ref("");
const minutes = ref(60);

/**
 * The raw log keeps its own tail instance: pausing the jobs page must not
 * silence this page, and the 500-row cap is shared through one implementation.
 */
const tail = useLiveTail({
  fetch: (since) =>
    logsEvents({
      stage: stage.value || undefined,
      traceId: traceId.value.trim() || undefined,
      field: field.value.trim() || undefined,
      minutes: minutes.value,
      since,
      limit: 500,
    }),
  maxRows: 500,
  maxTraces: 0,
});
const rows = tail.events;
const loading = tail.loading;
const error = tail.error;
const live = computed({
  get: () => !tail.paused.value,
  set: (value: boolean) => tail.setPaused(!value),
});

const selected = ref<TraceEventView | null>(null);
const drawer = ref(false);

async function download(): Promise<void> {
  try {
    const path = await exportLogs();
    ElMessage.success(t("business.records.exported", { path }));
  } catch (reason) {
    error.value = errorText(describeError(reason));
  }
}

function openDetail(event: TraceEventView): void {
  selected.value = event;
  drawer.value = true;
}

const reload = debounce(() => tail.reset(), 300);
watch([stage, traceId, field], reload);
watch(minutes, () => tail.reset());
</script>

<template>
  <section class="page">
    <el-alert v-if="error" :title="error" type="error" :closable="false" show-icon class="page-alert" />
    <p class="section-sub">{{ t("logs.subtitle") }}</p>
    <div class="filter-row">
      <el-select v-model="stage" clearable :placeholder="t('logs.filter.stage')" class="filter-select">
        <el-option :label="t('logs.filter.all')" value="" />
        <el-option v-for="name in STAGES" :key="name" :label="stageLabel(name)" :value="name" />
      </el-select>
      <el-input v-model="traceId" :placeholder="t('logs.filter.tracePlaceholder')" clearable class="filter-search" />
      <el-input v-model="field" :placeholder="t('logs.filter.field')" clearable class="filter-search" />
      <el-select v-model="minutes" class="filter-select">
        <el-option :label="t('logs.window.15')" :value="15" />
        <el-option :label="t('logs.window.60')" :value="60" />
        <el-option :label="t('logs.window.1440')" :value="1440" />
      </el-select>
      <el-switch v-model="live" :active-text="t('logs.liveTail')" />
      <el-button type="primary" plain @click="download">{{ t("logs.export") }}</el-button>
    </div>
    <Skeleton
      v-if="loading && rows.length === 0"
      :loading="true"
      variant="table"
      :rows="7"
      :label="t('common.loading')"
    />
    <el-table
      v-else
      v-loading="loading"
      :data="rows"
      row-key="seq"
      class="data-table"
      @row-click="openDetail"
    >
      <el-table-column v-if="logColumns.seq" prop="seq" :label="t('logs.table.seq')" width="90" />
      <el-table-column :label="t('logs.table.time')" width="170">
        <template #default="{ row }">{{ formatDateTime(row.at) }}</template>
      </el-table-column>
      <el-table-column v-if="logColumns.stage" :label="t('logs.table.stage')" width="100">
        <template #default="{ row }">{{ stageLabel(row.stage) }}</template>
      </el-table-column>
      <el-table-column prop="traceId" :label="t('logs.table.trace')" min-width="150" show-overflow-tooltip />
      <el-table-column :label="t('logs.table.result')" width="100">
        <template #default="{ row }">
          <el-tag :type="row.ok ? 'success' : 'danger'" effect="light">{{ t(logResultKey(row.ok)) }}</el-tag>
        </template>
      </el-table-column>
      <el-table-column :label="t('logs.table.duration')" width="100">
        <template #default="{ row }">{{ formatNumber(row.durMs ?? 0) }} ms</template>
      </el-table-column>
    </el-table>

    <el-drawer v-model="drawer" :title="selected ? stageLabel(selected.stage) : ''" size="420px">
      <pre v-if="selected" class="code-block">{{ JSON.stringify(selected, null, 2) }}</pre>
    </el-drawer>
  </section>
</template>
