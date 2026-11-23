<script setup lang="ts">
import { computed, onMounted, ref, toRef, watch } from "vue";
import { ElMessage } from "element-plus";
import { ArrowDown, Download } from "@element-plus/icons-vue";

import { exportRecords, recordsList, recordsSummary, retryStart } from "../api";
import { describeError, i18n } from "../i18n";
import type { RecordView, RecordsSummary, Snapshot } from "../types";
import { debounce, localIsoDay } from "../format";
import { resolveEmptyState } from "../components/businessColumns.js";
import { successRate as successRateValue } from "../overviewData.js";
import { useBreakpoint } from "../composables/useBreakpoint";
import ServiceBanner from "../components/ServiceBanner.vue";
import EmptyState from "../components/EmptyState.vue";
import Skeleton from "../components/Skeleton.vue";
import { isPluginDisabledError } from "../plugins/availability.js";
import { usePlugins } from "../composables/usePlugins";
import PluginDisabled from "./PluginDisabled.vue";
import StateDot from "../components/StateDot.vue";
import type { StateKind } from "../components/state";

const { t, errorText, formatDateTime, formatNumber } = i18n;
const props = defineProps<{ snapshot: Snapshot | null }>();
const snapshot = toRef(props, "snapshot");
const plugins = usePlugins(snapshot);
const disabled = computed(() => !plugins.isOn("records"));
const canRecover = computed(() => plugins.canRecover("records"));
let generation = 0;

function clearRows(): void {
  rows.value = [];
  summary.value = null;
  nextCursor.value = null;
  loading.value = false;
  loadingMore.value = false;
}

const emit = defineEmits<{ refresh: []; openPage: [id: string] }>();
const { columns } = useBreakpoint();

const days = ref(7);
const status = ref("");
const printer = ref("");
const tag = ref("");
const rows = ref<RecordView[]>([]);
const summary = ref<RecordsSummary | null>(null);
const loading = ref(false);
const loadingMore = ref(false);
const nextCursor = ref<number | null>(null);
const error = ref("");
const detail = ref<RecordView | null>(null);
const detailOpen = ref(false);
const exporting = ref(false);
const retrying = ref(false);

const from = computed(() => localIsoDay(1 - days.value));
const to = computed(() => localIsoDay(1));
function filters() {
  return {
    from: from.value,
    to: to.value,
    status: status.value || undefined,
    printer: printer.value.trim() || undefined,
    tags: tag.value.trim() ? tag.value.split(",").map((value) => value.trim()).filter(Boolean) : undefined,
  };
}

const hasFilters = computed(
  () => status.value !== "" || printer.value.trim() !== "" || tag.value.trim() !== "",
);
const successRate = computed(() => successRateValue(summary.value));
const serviceDown = computed(() => {
  const service = props.snapshot?.status;
  return !service || service.state === "stopped" || !service.reachable;
});
const dataDown = computed(() => serviceDown.value);

function recordKind(value: string): StateKind {
  if (value === "failed") return "failed";
  if (value === "cancelled") return "warning";
  if (value === "done") return "success";
  return "pending";
}

function recordStatusLabel(value: string): string {
  if (value === "failed") return t("records.statusValue.failed");
  if (value === "cancelled") return t("records.statusValue.cancelled");
  if (value === "done") return t("records.statusValue.done");
  return t("records.statusValue.pending");
}


const showSkeleton = computed(() => loading.value && rows.value.length === 0);
const emptyKind = computed(() =>
  resolveEmptyState({ loading: loading.value, count: rows.value.length, hasFilters: hasFilters.value }),
);
const emptyAction = computed(() =>
  emptyKind.value === "noMatch" ? t("business.records.clearFilters") : t("shell.refresh"),
);
const filterChips = computed(() => {
  const chips: Array<{ id: string; label: string }> = [];
  if (status.value) {
    const label =
      status.value === "failed"
        ? t("records.statusValue.failed")
        : status.value === "cancelled"
          ? t("records.statusValue.cancelled")
          : t("records.statusValue.done");
    chips.push({ id: "status", label: `${t("records.table.status")}: ${label}` });
  }
  if (printer.value.trim()) {
    chips.push({ id: "printer", label: `${t("records.table.printer")}: ${printer.value.trim()}` });
  }
  if (tag.value.trim()) {
    chips.push({ id: "tag", label: `${t("records.table.tags")}: ${tag.value.trim()}` });
  }
  return chips;
});

function removeFilter(id: string): void {
  if (id === "status") status.value = "";
  else if (id === "printer") printer.value = "";
  else if (id === "tag") tag.value = "";
}

function clearFilters(): void {
  status.value = "";
  printer.value = "";
  tag.value = "";
}

function onEmptyAction(): void {
  if (emptyKind.value === "noMatch") clearFilters();
  else void load();
}

async function retry(): Promise<void> {
  retrying.value = true;
  try {
    await retryStart();
    emit("refresh");
    await load();
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    retrying.value = false;
  }
}

async function load(): Promise<void> {
  const run = ++generation;
  if (disabled.value || dataDown.value) {
    clearRows();
    error.value = "";
    return;
  }
  loading.value = true;
  error.value = "";
  try {
    const query = filters();
    const [nextSummary, list] = await Promise.all([
      recordsSummary(query),
      recordsList({ ...query, limit: 50 }),
    ]);
    if (run !== generation || disabled.value) return;
    summary.value = nextSummary;
    rows.value = list.records;
    nextCursor.value = list.nextCursor ?? null;
  } catch (reason) {
    if (run !== generation) return;
    if (isPluginDisabledError(reason)) {
      clearRows();
      error.value = "";
      emit("refresh");
      return;
    }
    error.value = errorText(describeError(reason));
  } finally {
    if (run === generation) loading.value = false;
  }
}

async function loadMore(): Promise<void> {
  if (disabled.value || dataDown.value || loadingMore.value || nextCursor.value === null) return;
  const run = generation;
  const cursor = nextCursor.value;
  loadingMore.value = true;
  try {
    const page = await recordsList({ ...filters(), cursor, limit: 50 });
    if (run !== generation || disabled.value || dataDown.value) return;
    rows.value = [...rows.value, ...page.records];
    nextCursor.value = page.nextCursor ?? null;
  } catch (reason) {
    if (run !== generation || disabled.value || dataDown.value) return;
    if (isPluginDisabledError(reason)) {
      clearRows();
      emit("refresh");
      return;
    }
    error.value = errorText(describeError(reason));
  } finally {
    loadingMore.value = false;
  }
}

function openDetail(row: RecordView): void {
  detail.value = row;
  detailOpen.value = true;
}

async function copyRecord(row: RecordView): Promise<void> {
  try {
    await navigator.clipboard.writeText(JSON.stringify(row, null, 2));
    ElMessage.success(t("business.records.copied"));
  } catch {
    ElMessage.warning(t("business.records.copyFailed"));
  }
}

function tagText(row: RecordView): string {
  return row.tags.length ? row.tags.join(", ") : t("business.records.noTags");
}

async function exportCsv(): Promise<void> {
  if (disabled.value || dataDown.value) return;
  exporting.value = true;
  try {
    const path = await exportRecords(filters());
    ElMessage.success(t("business.records.exported", { path }));
  } catch (reason) {
    if (isPluginDisabledError(reason)) {
      clearRows();
      emit("refresh");
      return;
    }
    error.value = errorText(describeError(reason));
  } finally {
    exporting.value = false;
  }
}

const reload = debounce(() => void load(), 300);
watch([days, status], () => void load());
watch(disabled, (value) => {
  if (value) {
    generation += 1;
    clearRows();
    error.value = "";
  } else {
    void load();
  }
});
watch(dataDown, (down) => {
  generation += 1;
  if (down) {
    clearRows();
    error.value = "";
  } else {
    void load();
  }
});
watch([printer, tag], reload);

onMounted(() => void load());
</script>
<template>

  <PluginDisabled
    v-if="disabled"
    :snapshot="snapshot"
    plugin-id="records"
    :can-recover="canRecover"
    @recovered="emit('refresh')"
    @refresh="emit('refresh')"
    @open-page="emit('openPage', 'plugins')"
  />
  <section v-else class="page">
    <ServiceBanner :snapshot="snapshot" @retry="retry" @refresh="emit('refresh')" />
    <el-alert
      v-if="error && !serviceDown"
      :title="error"
      type="error"
      :closable="false"
      show-icon
      class="page-alert"
    />

    <section class="kpi-strip records-kpi">
      <div class="kpi-item">
        <span class="kpi-label kpi-label--wrap">{{ t("business.records.total") }}</span>
        <strong class="kpi-value">{{ formatNumber(summary?.total ?? 0) }}</strong>
      </div>
      <div class="kpi-item">
        <span class="kpi-label kpi-label--wrap">{{ t("records.card.successfulPrintsToday") }}</span>
        <strong class="kpi-value">{{ formatNumber(summary?.todayDone ?? 0) }} <span class="kpi-unit">{{ t("records.unit.prints") }}</span></strong>
      </div>
      <div class="kpi-item">
        <span class="kpi-label kpi-label--wrap">{{ t("business.records.successRate") }}</span>
        <strong class="kpi-value">{{ successRate === null ? "—" : `${successRate}%` }}</strong>
        <span v-if="successRate === null" class="kpi-hint">
          {{ t("business.records.successRateUnavailable") }}
        </span>
      </div>
      <div class="kpi-item">
        <span class="kpi-label kpi-label--wrap">{{ t("records.card.averageDuration") }}</span>
        <strong class="kpi-value">{{ formatNumber(Math.round(summary?.avgDurMs ?? 0)) }} ms</strong>
      </div>
      <div class="kpi-item">
        <span class="kpi-label kpi-label--wrap">{{ t("business.records.failedCount") }}</span>
        <strong class="kpi-value">{{ formatNumber(summary?.failed ?? 0) }}</strong>
      </div>
    </section>



    <div class="filter-row">
      <el-radio-group v-model="days">
        <el-radio-button :value="7">7 {{ t("business.records.days") }}</el-radio-button>
        <el-radio-button :value="30">30 {{ t("business.records.days") }}</el-radio-button>
      </el-radio-group>
      <el-select v-model="status" :placeholder="t('records.table.status')" clearable class="filter-select">
        <el-option :label="t('common.all')" value="" />
        <el-option :label="t('records.statusValue.done')" value="done" />
        <el-option :label="t('records.statusValue.failed')" value="failed" />
        <el-option :label="t('records.statusValue.cancelled')" value="cancelled" />
      </el-select>
      <el-input v-model="printer" :placeholder="t('records.table.printer')" clearable class="filter-select" />
      <el-input v-model="tag" :placeholder="t('records.table.tags')" clearable class="filter-select" />

      <el-button type="primary" plain :icon="Download" :loading="exporting"
      :aria-busy="exporting ? 'true' : 'false'" :disabled="loading || dataDown" @click="exportCsv">
        {{ t("records.export") }}
      </el-button>
    </div>
    <FilterChips
      :chips="filterChips"
      :clear-label="t('business.records.clearFilters')"
      @remove="removeFilter"
      @clear="clearFilters"
    />


      <div class="table-pane">
        <Skeleton v-if="showSkeleton" :loading="true" variant="table" :rows="6" :label="t('common.loading')" />
        <el-table
          v-else-if="rows.length > 0"
          v-loading="loading"
          :data="rows"
          row-key="id"
          class="data-table"
          @row-click="openDetail"
        >
          <el-table-column :label="t('records.table.time')" width="100">
            <template #default="{ row }">{{ formatDateTime(row.at) }}</template>
          </el-table-column>
          <el-table-column
            v-if="columns.printer"
            prop="printer"
            :label="t('records.table.printer')"
            width="84"
            show-overflow-tooltip
          />
          <el-table-column v-if="columns.pages" prop="pages" :label="t('records.table.pages')" width="50" />
          <el-table-column v-if="columns.copies" prop="copies" :label="t('records.table.copies')" width="54" />
          <el-table-column :label="t('records.table.status')" width="84">
            <template #default="{ row }">
              <StateDot
                :kind="recordKind(row.status)"
                :label="recordStatusLabel(row.status)"
              />
            </template>
          </el-table-column>
          <el-table-column :label="t('records.table.error')" width="98" show-overflow-tooltip>
            <template #default="{ row }">
              <span v-if="row.errorCode" class="obs-code">{{ row.errorCode }}</span>
              <span v-else class="muted">—</span>
            </template>
          </el-table-column>
          <el-table-column v-if="columns.template" prop="template" :label="t('records.table.template')" width="82" show-overflow-tooltip />
          <el-table-column v-if="columns.trace" :label="t('records.table.trace')" width="92">
            <template #default="{ row }"><code>{{ row.traceId }}</code></template>
          </el-table-column>
          <el-table-column v-if="columns.duration" :label="t('records.table.duration')" width="90">
            <template #default="{ row }">{{ formatNumber(row.durMs) }} ms</template>
          </el-table-column>
        </el-table>
        <EmptyState
          v-else
          :title="emptyKind === 'noMatch' ? t('business.records.noMatch') : t('records.empty')"
          :description="emptyKind === 'noMatch' ? undefined : t('business.records.emptyHint')"
          :action-label="emptyAction"
          @action="onEmptyAction"
        />
        <div v-if="rows.length > 0 && nextCursor !== null" class="load-more">
          <el-button :icon="ArrowDown" :loading="loadingMore"
          :aria-busy="loadingMore ? 'true' : 'false'" @click="loadMore">{{ t("business.records.loadMore") }}</el-button>
        </div>
      </div>

    <el-drawer
      v-model="detailOpen"
      :title="t('business.records.detail')"
      size="440px"
      @closed="detail = null"
    >
      <div v-if="detail" class="record-detail">
        <dl class="detail-list">
          <dt>{{ t("records.table.time") }}</dt>
          <dd>{{ formatDateTime(detail.at) }}</dd>
          <dt>{{ t("records.table.status") }}</dt>
          <dd>{{ recordStatusLabel(detail.status) }}</dd>
          <dt v-if="detail.errorCode">{{ t("records.table.error") }}</dt>
          <dd v-if="detail.errorCode" class="obs-code">{{ detail.errorCode }}</dd>
          <dt>{{ t("records.table.trace") }}</dt>
          <dd><code>{{ detail.traceId }}</code></dd>
          <dt>{{ t("records.table.template") }}</dt>
          <dd>{{ detail.template || "—" }}</dd>
          <dt>{{ t("records.table.tags") }}</dt>
          <dd>{{ tagText(detail) }}</dd>
          <dt>{{ t("records.table.printer") }}</dt>
          <dd>{{ detail.printer || "—" }}</dd>
          <dt>{{ t("records.table.duration") }}</dt>
          <dd>{{ formatNumber(detail.durMs) }} ms</dd>
        </dl>
        <div class="inline-actions">
          <el-button type="primary" plain @click="copyRecord(detail)">
            {{ t("business.records.copyJson") }}
          </el-button>
          <el-button @click="detailOpen = false">{{ t("business.records.closeDetail") }}</el-button>
        </div>
        <p v-if="detail.status === 'failed'" class="muted">
          {{ t("business.records.reprintUnavailable") }}
        </p>
      </div>
    </el-drawer>
  </section>
</template>

<style scoped>
.records-kpi {
  grid-template-columns: repeat(5, minmax(0, 1fr));
}

.records-kpi .kpi-item:last-child:nth-child(odd) {
  grid-column: auto;
}

@media (max-width: 620px) {
  .records-kpi .kpi-item {
    padding: 10px 8px;
  }
}
</style>
