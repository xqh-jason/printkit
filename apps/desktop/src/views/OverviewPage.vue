<script setup lang="ts">
import { computed, onMounted, ref, toRef, watch } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { ArrowRight, Refresh, SwitchButton, VideoPlay } from "@element-plus/icons-vue";

import {
  listPrinters,
  pluginEnableOwned,
  recordsList,
  recordsStats,
  recordsSummary,
  retryStart,
  startService,
  stopService,
} from "../api";
import { describeError, i18n } from "../i18n";

import { recoveryPlugins } from "../plugins/availability.js";
import {
  failureRate,
  fillDaySeries,
  localDayFromKey,
  overviewWindow,
  recordsRequestable,
  statusDistribution,
  todayMetrics,
  topPrinters,
} from "../overviewData.js";
import type { RecordStatsBucket, RecordView, RecordsSummary, Snapshot } from "../types";
import EmptyState from "../components/EmptyState.vue";
import ServiceBanner from "../components/ServiceBanner.vue";
import Skeleton from "../components/Skeleton.vue";
import StateDot from "../components/StateDot.vue";
import type { StateKind } from "../components/state";
import { usePlugins } from "../composables/usePlugins";

const props = defineProps<{ snapshot: Snapshot | null }>();
const emit = defineEmits<{ refresh: []; "open-page": [id: string] }>();
const { t, errorText, formatDate, formatDateTime, formatNumber, formatPercent } = i18n;
const snapshot = toRef(props, "snapshot");
const plugins = usePlugins(snapshot);

const summary = ref<RecordsSummary | null>(null);
const dayBuckets = ref<RecordStatsBucket[]>([]);
const printerBuckets = ref<RecordStatsBucket[]>([]);
const recent = ref<RecordView[]>([]);
const printerCount = ref(0);
const range = ref(overviewWindow());
const loading = ref(false);
const busy = ref("");
const error = ref("");
let loadSeq = 0;

const recordsOn = computed(() => plugins.isOn("records"));
const status = computed(() => props.snapshot?.status);
const serviceState = computed(() => status.value?.state ?? "stopped");
const serviceKind = computed<StateKind>(() => {
  const value = status.value;
  if (!value) return "idle";
  if (value.state === "stopped") return "idle";
  if (!value.reachable) return "failed";
  if (value.state === "runningOwned") return "success";
  if (value.state === "runningExternal") return "warning";
  return "idle";
});
const serviceLabel = computed(() => {
  const value = status.value;
  if (value?.state === "stopped") return t("shell.service.state.stopped");
  if (value && !value.reachable) return t("service.banner.unreachable");
  if (value?.state === "runningOwned") return t("shell.service.state.runningOwned");
  if (value?.state === "runningExternal") return t("shell.service.state.runningExternal");
  return t("shell.service.state.stopped");
});
const serviceDown = computed(
  () => !status.value || status.value.state === "stopped" || !status.value.reachable,
);
const recentError = computed(() => {
  const value = props.snapshot;
  if (!value) return "";
  if (value.autoStart.state === "failed") return value.autoStart.error ?? "";
  if (value.status.state === "stopped") return value.status.detail ?? "";
  return value.configError ?? "";
});
const dataDown = computed(() => serviceDown.value);
const recoveryRows = computed(() =>
  recoveryPlugins(props.snapshot).map((id) => ({
    id,
    label: t(`plugins.recovery.${id}`),
    detail: t("plugins.recovery.detail"),
  })),
);

const daySeries = computed(() => fillDaySeries(dayBuckets.value, range.value.days));
const statusParts = computed(() => statusDistribution(summary.value));
const today = computed(() => todayMetrics(summary.value));
const failurePct = computed(() => {
  const rate = failureRate(summary.value);
  return rate === null ? null : formatPercent(rate);
});
const printersTop = computed(() => topPrinters(printerBuckets.value, 5));
const printerMax = computed(() =>
  Math.max(0, ...printersTop.value.map((printer) => printer.count)),
);
const trendMax = computed(() => Math.max(0, ...daySeries.value.map((day) => day.sheets)));
const trendTotal = computed(() => daySeries.value.reduce((sum, day) => sum + day.sheets, 0));
const trendRecords = computed(() => daySeries.value.reduce((sum, day) => sum + day.count, 0));
const hasTrend = computed(() => trendMax.value > 0);
const hasStatus = computed(() => statusParts.value.total > 0);
const hasPrinters = computed(() => printersTop.value.length > 0 && printerMax.value > 0);
const hasRecent = computed(() => recent.value.length > 0);

const chart = {
  width: 600,
  height: 180,
  left: 40,
  right: 12,
  top: 14,
  bottom: 30,
};
const trendBars = computed(() => {
  if (!hasTrend.value) return [];
  const plotWidth = chart.width - chart.left - chart.right;
  const plotHeight = chart.height - chart.top - chart.bottom;
  const step = plotWidth / Math.max(1, daySeries.value.length);
  const barWidth = Math.min(20, Math.max(8, step - 10));
  const max = trendMax.value;
  return daySeries.value.map((day, index) => {
    const height = max === 0 ? 0 : (day.sheets / max) * plotHeight;
    return {
      ...day,
      x: chart.left + index * step + (step - barWidth) / 2,
      y: chart.top + plotHeight - height,
      height,
      width: barWidth,
      center: chart.left + index * step + step / 2,
      label: dayLabel(day.key),
    };
  });
});

function dayLabel(key: string): string {
  return formatDate(localDayFromKey(key), { month: "2-digit", day: "2-digit" });
}

function printerLabel(key: string | null | undefined): string {
  if (!key || key === "default") return t("business.overview.unknownPrinter");
  return key;
}

function statusLabel(value: string): string {
  const key = `records.statusValue.${value}`;
  const label = t(key);
  return label === key ? value : label;
}

function formatMs(value: number | null | undefined): string {
  return value === null || value === undefined ? "—" : `${formatNumber(Math.round(value))} ms`;
}

function dayTooltip(day: RecordStatsBucket & { label: string }): string {
  return t("business.overview.barTooltip", {
    date: day.label,
    sheets: formatNumber(day.sheets),
    records: formatNumber(day.count),
    average: formatMs(day.avgDurMs),
  });
}

function statusWidth(value: number): string {
  return `${(value / statusParts.value.total) * 100}%`;
}

function printerWidth(value: number): string {
  return `${printerMax.value === 0 ? 0 : (value / printerMax.value) * 100}%`;
}

function openRecords(): void {
  emit("open-page", "records");
}

const hasData = computed(
  () =>
    summary.value !== null ||
    dayBuckets.value.length > 0 ||
    printerBuckets.value.length > 0 ||
    recent.value.length > 0,
);


async function load(): Promise<void> {
  const seq = ++loadSeq;
  range.value = overviewWindow();
  error.value = "";
  if (dataDown.value) {
    loading.value = false;
    return;
  }
  loading.value = true;
  try {
    if (!recordsOn.value) {
      const printers = await listPrinters();
      if (seq !== loadSeq) return;
      printerCount.value = printers.length;
      return;
    }
    const query = { from: range.value.from, to: range.value.to };
    const [printers, nextSummary, days, printersByDay, recentPage] = await Promise.all([
      listPrinters(),
      recordsSummary(query),
      recordsStats({ groupBy: "day", ...query }),
      recordsStats({ groupBy: "printer", ...query }),
      recordsList({ limit: 5 }),
    ]);
    if (seq !== loadSeq || !recordsRequestable(dataDown.value, recordsOn.value)) return;
    printerCount.value = printers.length;
    summary.value = nextSummary;
    dayBuckets.value = days;
    printerBuckets.value = printersByDay;
    recent.value = recentPage.records.slice(0, 5);
  } catch (reason) {
    if (seq !== loadSeq) return;
    // Keep the previous snapshot visible; the alert explains the refresh
    // failure instead of replacing it with an empty state first.
    error.value = errorText(describeError(reason));
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}

async function refresh(): Promise<void> {
  emit("refresh");
  await load();
}

async function serviceAction(kind: "start" | "stop"): Promise<void> {
  if (kind === "stop") {
    try {
      await ElMessageBox.confirm(t("business.overview.stopConfirm"), t("business.overview.stop"), {
        type: "warning",
      });
    } catch {
      return;
    }
  }
  busy.value = kind;
  try {
    if (kind === "start") await startService();
    else await stopService();
    emit("refresh");
    ElMessage.success(t("business.overview.done"));
    await load();
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    busy.value = "";
  }
}

async function retry(): Promise<void> {
  busy.value = "retry";
  try {
    await retryStart();
    emit("refresh");
    await load();
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    busy.value = "";
  }
}

async function recover(id: "records" | "observability"): Promise<void> {
  busy.value = id;
  try {
    await pluginEnableOwned(id);
    emit("refresh");
    await load();
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    busy.value = "";
  }
}

watch([serviceDown, recordsOn], () => void load());
onMounted(() => void load());
</script>

<template>
  <section class="page overview-page">
    <div v-if="error" class="overview-error">
      <el-alert :title="error" type="error" :closable="false" show-icon />
      <el-button type="primary" plain size="small" @click="load">
        {{ t("business.overview.retry") }}
      </el-button>
    </div>
    <ServiceBanner :snapshot="snapshot" @retry="retry" @refresh="refresh" />

    <div class="overview-head">
      <div>
        <h2>{{ t("business.overview.title") }}</h2>
        <p class="section-sub">{{ t("business.overview.subtitle") }}</p>
      </div>
      <div class="inline-actions">
        <StateDot :kind="serviceKind" :label="serviceLabel" :size="9" />
        <el-button
          v-if="serviceState === 'stopped'"
          type="primary"
          :icon="VideoPlay"
          :loading="busy === 'start'"
          :aria-busy="busy === 'start' ? 'true' : 'false'"
          @click="serviceAction('start')"
        >
          {{ t("business.overview.start") }}
        </el-button>
        <el-button
          v-else-if="serviceState === 'runningOwned'"
          :icon="SwitchButton"
          :loading="busy === 'stop'"
          :aria-busy="busy === 'stop' ? 'true' : 'false'"
          @click="serviceAction('stop')"
        >
          {{ t("business.overview.stop") }}
        </el-button>
        <el-button :icon="Refresh" :loading="busy === 'retry'"
          :aria-busy="busy === 'retry' ? 'true' : 'false'" @click="refresh">
          {{ t("shell.refresh") }}
        </el-button>
      </div>
    </div>

    <el-alert
      v-if="recentError"
      :title="recentError"
      type="warning"
      :closable="false"
      show-icon
      class="page-alert"
    />

    <section
      v-if="!dataDown && recordsOn && !error && (hasData || !loading)"
      class="kpi-strip"
    >
      <div class="kpi-item">
        <span class="kpi-label kpi-label--wrap">{{ t("compact.successfulPrintsToday") }}</span>
        <strong class="kpi-value">{{ formatNumber(summary?.todayDone ?? 0) }} <span class="kpi-unit">{{ t("records.unit.prints") }}</span></strong>
        <div class="kpi-sub">
          <span>{{ t("records.statusValue.done") }} {{ formatNumber(today.todayDone) }}</span>
          <span>{{ t("records.statusValue.failed") }} {{ formatNumber(today.todayFailed) }}</span>
          <span>{{ t("records.statusValue.cancelled") }} {{ formatNumber(today.todayCancelled) }}</span>
        </div>
        <span class="kpi-hint">{{ t("compact.successfulPrintsHint") }}</span>
      </div>
      <div class="kpi-item">
        <span class="kpi-label">{{ t("business.overview.sevenDayRecords") }}</span>
        <strong class="kpi-value">{{ formatNumber(summary?.total ?? 0) }} <span class="kpi-unit">{{ t("records.unit.prints") }}</span></strong>
      </div>
      <div class="kpi-item">
        <span class="kpi-label">{{ t("business.overview.failureRate") }}</span>
        <strong class="kpi-value">{{ failurePct ?? "—" }}</strong>
        <span class="kpi-hint">{{ t("business.overview.failureRateHint") }}</span>
      </div>
      <div class="kpi-item">
        <span class="kpi-label">{{ t("compact.printerCount") }}</span>
        <strong class="kpi-value">{{ formatNumber(printerCount) }} <span class="kpi-unit">{{ t("business.overview.printerUnit") }}</span></strong>
      </div>
    </section>
    <section
      v-else-if="!dataDown && !recordsOn && !error && (hasData || !loading)"
      class="kpi-strip"
    >
      <div class="kpi-item">
        <span class="kpi-label">{{ t("compact.printerCount") }}</span>
        <strong class="kpi-value">{{ formatNumber(printerCount) }}</strong>
      </div>
    </section>

    <section
      v-if="!dataDown && !recordsOn"
      class="panel overview-records-off"
    >
      <EmptyState
        :title="t('business.overview.recordsOff')"
        :description="t('business.overview.recordsOffHint')"
        :action-label="t('business.overview.enableRecords')"
        @action="recover('records')"
      />
    </section>
    <section v-else-if="dataDown" class="panel overview-unavailable">
      <EmptyState
        :title="t('business.overview.statsUnavailable')"
        :description="t('business.overview.statsUnavailableHint')"
      />
    </section>
    <section v-else-if="loading && !hasData" class="panel overview-loading">
      <Skeleton :loading="true" variant="detail" :rows="4" :label="t('common.loading')" />
    </section>
    <section v-else-if="!error || hasData" class="overview-charts">
      <div class="overview-grid">
        <section class="panel chart-trend">
          <header class="panel-head">
            <div>
              <h2>{{ t("business.overview.trendTitle") }}</h2>
              <p class="section-sub">{{ t("business.overview.lastSevenDays") }}</p>
            </div>
            <span class="chart-total">{{ formatNumber(trendTotal) }} {{ t("business.overview.sheets") }}</span>
          </header>
          <EmptyState
            v-if="!hasTrend"
            :title="t('business.overview.emptyRecords')"
            :description="t('business.overview.emptyHint')"
          />
          <svg
            v-else
            class="chart-svg"
            viewBox="0 0 600 180"
            role="img"
            :aria-label="t('business.overview.trendAria', { sheets: formatNumber(trendTotal), records: formatNumber(trendRecords) })"
          >
            <g v-for="gridLine in [0, 0.5, 1]" :key="gridLine">
              <line
                class="chart-grid"
                :x1="chart.left"
                :x2="chart.width - chart.right"
                :y1="chart.top + (chart.height - chart.top - chart.bottom) * (1 - gridLine)"
                :y2="chart.top + (chart.height - chart.top - chart.bottom) * (1 - gridLine)"
              />
              <text
                class="chart-axis"
                :x="chart.left - 6"
                :y="chart.top + (chart.height - chart.top - chart.bottom) * (1 - gridLine) + 4"
                text-anchor="end"
              >{{ formatNumber(trendMax * gridLine) }}</text>
            </g>
            <g
              v-for="day in trendBars"
              :key="day.key"
              role="img"
              :aria-label="dayTooltip(day)"
              tabindex="0"
            >
              <title>{{ dayTooltip(day) }}</title>
              <rect
                class="chart-bar"
                :x="day.x"
                :y="day.y"
                :width="day.width"
                :height="day.height"
                rx="2"
              />
              <text
                class="chart-x-label"
                :x="day.center"
                :y="chart.height - 8"
                text-anchor="middle"
              >{{ day.label }}</text>
            </g>
          </svg>
        </section>

        <section class="panel chart-status">
          <header class="panel-head">
            <div>
              <h2>{{ t("business.overview.statusTitle") }}</h2>
              <p class="section-sub">{{ t("business.overview.lastSevenDays") }}</p>
            </div>
            <span v-if="failurePct" class="chart-total">{{ t("business.overview.failureRate") }} {{ failurePct }}</span>
          </header>
          <EmptyState
            v-if="!hasStatus"
            :title="t('business.overview.emptyRecords')"
            :description="t('business.overview.emptyHint')"
          />
          <template v-else>
            <div
              class="status-track"
              role="img"
              :aria-label="t('business.overview.statusAria', {
                done: formatNumber(statusParts.done),
                failed: formatNumber(statusParts.failed),
                cancelled: statusParts.cancelledKnown ? formatNumber(statusParts.cancelled ?? 0) : t('business.overview.cancelledUnavailable'),
              })"
            >
              <span
                v-if="statusParts.done > 0"
                class="status-segment status-done"
                :style="{ width: statusWidth(statusParts.done) }"
              />
              <span
                v-if="statusParts.failed > 0"
                class="status-segment status-failed"
                :style="{ width: statusWidth(statusParts.failed) }"
              />
              <span
                v-if="statusParts.cancelledKnown && (statusParts.cancelled ?? 0) > 0"
                class="status-segment status-cancelled"
                :style="{ width: statusWidth(statusParts.cancelled ?? 0) }"
              />
            </div>
            <ul class="status-legend">
              <li><i class="legend-done" />{{ t("records.statusValue.done") }} <strong>{{ formatNumber(statusParts.done) }}</strong></li>
              <li><i class="legend-failed" />{{ t("records.statusValue.failed") }} <strong>{{ formatNumber(statusParts.failed) }}</strong></li>
              <li v-if="statusParts.cancelledKnown"><i class="legend-cancelled" />{{ t("records.statusValue.cancelled") }} <strong>{{ formatNumber(statusParts.cancelled ?? 0) }}</strong></li>
            </ul>
            <p v-if="!statusParts.cancelledKnown" class="chart-note">
              {{ t("business.overview.cancelledUnavailable") }}
            </p>
            <div class="status-facts">
              <span>{{ t("business.overview.averageDuration") }} <strong>{{ formatMs(summary?.avgDurMs) }}</strong></span>
              <span>{{ t("business.overview.slowestPrinter") }} <strong>{{ printerLabel(summary?.slowestPrinter) }}</strong></span>
            </div>
          </template>
        </section>
      </div>
      <div class="overview-bottom">

        <section class="panel chart-printers">
          <header class="panel-head">
            <h2>{{ t("business.overview.printersTitle") }}</h2>
          </header>
          <EmptyState
            v-if="!hasPrinters"
            :title="t('business.overview.emptyRecords')"
            :description="t('business.overview.emptyHint')"
          />
          <div v-else class="printer-bars">
            <div v-for="printer in printersTop" :key="printer.key" class="printer-row">
              <div class="printer-line">
                <span class="printer-name">{{ printerLabel(printer.key) }}</span>
                <span class="printer-value">{{ formatNumber(printer.count) }} {{ t("records.unit.prints") }}</span>
              </div>
              <div class="printer-track">
                <span class="printer-fill" :style="{ width: printerWidth(printer.count) }" />
              </div>
              <span class="printer-meta">
                <span>{{ t("business.overview.averageDuration") }} {{ formatMs(printer.avgDurMs) }}</span>
              </span>
            </div>
          </div>
        </section>

        <section class="panel chart-recent">
          <header class="panel-head">
            <h2>{{ t("business.overview.recentTitle") }}</h2>
            <el-button v-if="hasRecent" link type="primary" @click="openRecords">
              {{ t("business.overview.viewAll") }}
            </el-button>
          </header>
          <EmptyState
            v-if="!hasRecent"
            :title="t('records.empty')"
          />
          <div v-else class="recent-list">
            <button
              v-for="record in recent"
              :key="record.id"
              type="button"
              class="recent-row"
              @click="openRecords"
            >
              <span class="recent-time">{{ formatDateTime(record.at, { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" }) }}</span>
              <span class="recent-printer">{{ printerLabel(record.printer) }}</span>
              <span class="recent-pages">{{ formatNumber(record.pages) }} {{ t(record.pages === 1 ? "records.table.page" : "records.table.pages") }} / {{ formatNumber(record.copies) }} {{ t(record.copies === 1 ? "records.table.copy" : "records.table.copies") }}</span>
              <span class="recent-status" :class="`recent-status--${record.status}`">{{ statusLabel(record.status) }}</span>
              <span class="recent-duration">{{ formatMs(record.durMs) }}</span>
            </button>
          </div>
        </section>
      </div>
    </section>

    <section v-if="recoveryRows.length > 0" class="panel recovery-panel">
      <header class="panel-head">
        <h2>{{ t("business.overview.recovery") }}</h2>
      </header>
      <button
        v-for="row in recoveryRows"
        :key="row.id"
        type="button"
        class="recovery-row"
        :data-feature="row.id"
        :disabled="busy !== ''"
        @click="recover(row.id as 'records' | 'observability')"
      >
        <span class="recovery-text">
          <strong>{{ row.label }}</strong>
          <small>{{ row.detail }}</small>
        </span>
        <el-icon><ArrowRight /></el-icon>
      </button>
    </section>

    <dl class="overview-meta">
      <div>
        <dt>{{ t("business.settings.dataDir") }}</dt>
        <dd>{{ props.snapshot?.config?.dataDir ?? "—" }}</dd>
      </div>
      <div>
        <dt>{{ t("business.settings.version") }}</dt>
        <dd>{{ props.snapshot?.sidecar.version ?? "—" }}</dd>
      </div>
    </dl>
  </section>
</template>

<style scoped>
.overview-page .kpi-strip {
  grid-template-columns: repeat(2, minmax(0, 1fr));
  margin-top: 14px;
}

@media (min-width: 960px) {
  .overview-page .kpi-strip {
    grid-template-columns: repeat(4, minmax(0, 1fr));
  }
}

.overview-error {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin-bottom: 16px;
}

.overview-error :deep(.el-alert) {
  flex: 1;
  min-width: 0;
}

.inline-actions {
  flex-wrap: wrap;
}

.overview-charts {
  margin-top: 12px;
}

.overview-grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: 12px;
  align-items: start;
}

.overview-bottom {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: 12px;
  align-items: start;
  margin-top: 12px;
}

@media (min-width: 960px) {
  .overview-grid {
    grid-template-columns: minmax(0, 2fr) minmax(260px, 1fr);
  }

  .overview-bottom {
    grid-template-columns: minmax(260px, 1fr) minmax(0, 2fr);
  }
}

.chart-total,
.chart-note {
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-secondary);
  font-variant-numeric: tabular-nums;
}

.chart-svg {
  display: block;
  width: 100%;
  height: auto;
  aspect-ratio: 10 / 3;
  overflow: visible;
}

.chart-grid {
  stroke: var(--pk-border);
  stroke-width: 1;
}

.chart-axis,
.chart-x-label {
  fill: var(--pk-text-muted);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
}

.chart-bar {
  fill: var(--pk-accent);
  min-height: 0;
}

.chart-bar:focus-visible {
  outline: 2px solid var(--pk-focus);
  outline-offset: 2px;
}

.chart-svg g:focus-visible {
  outline: 2px solid var(--pk-focus);
  outline-offset: 2px;
}

.status-track {
  display: flex;
  width: 100%;
  height: 18px;
  overflow: hidden;
  border-radius: var(--pk-radius-field);
  background: var(--pk-surface-sunken);
}

.status-segment {
  display: block;
  height: 100%;
}

.status-done { background: var(--pk-success); }
.status-failed { background: var(--pk-danger); }
.status-cancelled { background: var(--pk-warning); }

.status-legend {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 6px 12px;
  margin: 0;
  padding: 0;
  list-style: none;
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-secondary);
}

.status-legend li {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
}

.status-legend i {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: var(--pk-radius-pill);
}

.legend-done { background: var(--pk-success); }
.legend-failed { background: var(--pk-danger); }
.legend-cancelled { background: var(--pk-warning); }

.status-legend strong {
  color: var(--pk-text);
  font-variant-numeric: tabular-nums;
}

.status-facts {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 14px;
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-secondary);
}

.status-facts strong {
  color: var(--pk-text);
  font-weight: var(--pk-font-weight-medium);
  font-variant-numeric: tabular-nums;
}

.printer-bars {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.printer-row {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}

.printer-line {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
  min-width: 0;
}

.printer-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--pk-text);
  font-weight: var(--pk-font-weight-medium);
}

.printer-value,
.printer-meta {
  flex: none;
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-caption);
  font-variant-numeric: tabular-nums;
}

.printer-meta {
  display: flex;
  gap: 6px;
}

.printer-track {
  height: 6px;
  overflow: hidden;
  border-radius: var(--pk-radius-pill);
  background: var(--pk-surface-sunken);
}

.printer-fill {
  display: block;
  height: 100%;
  border-radius: var(--pk-radius-pill);
  background: var(--pk-accent);
}

.recent-list {
  display: flex;
  flex-direction: column;
  border-top: 1px solid var(--pk-border);
}

.recent-row {
  display: grid;
  grid-template-columns: 100px minmax(0, 1fr) auto auto auto;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 9px 0;
  border: 0;
  border-bottom: 1px solid var(--pk-border);
  background: transparent;
  color: var(--pk-text);
  text-align: left;
  cursor: pointer;
}

.recent-row:hover {
  background: var(--pk-surface-sunken);
}

.recent-row:focus-visible {
  outline: 2px solid var(--pk-focus);
  outline-offset: -2px;
}

.recent-time,
.recent-pages,
.recent-duration {
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-caption);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

.recent-printer {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--pk-font-size-secondary);
}

.recent-status {
  font-size: var(--pk-font-size-caption);
  font-weight: var(--pk-font-weight-medium);
}

.recent-status--done { color: var(--pk-success); }
.recent-status--failed { color: var(--pk-danger); }
.recent-status--cancelled { color: var(--pk-warning); }

.overview-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 20px;
  margin: 12px 0 0;
  padding-top: 10px;
  border-top: 1px solid var(--pk-border);
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-caption);
}

.overview-meta div {
  display: flex;
  gap: 6px;
  min-width: 0;
}

.overview-meta dt {
  flex: none;
}

.overview-meta dd {
  margin: 0;
  overflow-wrap: anywhere;
  color: var(--pk-text);
}

@media (max-width: 620px) {
  .recent-row {
    grid-template-columns: minmax(0, 1fr) auto auto;
  }

  .recent-time,
  .recent-pages {
    grid-column: 1 / -1;
  }

  .recent-duration {
    justify-self: end;
  }
}
</style>
