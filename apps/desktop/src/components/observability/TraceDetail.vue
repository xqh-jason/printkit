<script setup lang="ts">
import { computed, ref } from "vue";
import { ElMessage } from "element-plus";
import { ArrowLeft, ArrowRight, CopyDocument, Download } from "@element-plus/icons-vue";
import { ERROR_TITLES } from "@printkit/i18n";

import { i18n } from "../../i18n";
import StageTimeline from "./StageTimeline.vue";
import StageWaterfall from "./StageWaterfall.vue";
import { STAGE_KEYS, formatMs, serializeTrace } from "./jobTraces";
import type { JobTrace } from "./jobTraces";

const props = defineProps<{
  job: JobTrace;
  prev: string | null;
  next: string | null;
}>();
const emit = defineEmits<{ close: []; select: [traceId: string] }>();
const { t, formatNumber, formatDateTime } = i18n;

/**
 * Repair advice for the registered protocol codes. Unknown codes stay
 * "unregistered" on purpose; the page must not invent an explanation.
 */
const CODE_HINTS: Record<string, string> = {
  E_CONNECT: "jobs.hint.E_CONNECT",
  E_AUTH: "jobs.hint.E_AUTH",
  E_ORIGIN: "jobs.hint.E_ORIGIN",
  E_SCHEMA: "jobs.hint.E_SCHEMA",
  E_PRINTER_NOT_FOUND: "jobs.hint.E_PRINTER_NOT_FOUND",
  E_PAPER_UNSUPPORTED: "jobs.hint.E_PAPER_UNSUPPORTED",
  E_JOB_FAILED: "jobs.hint.E_JOB_FAILED",
  E_TIMEOUT: "jobs.hint.E_TIMEOUT",
  E_PROTOCOL: "jobs.hint.E_PROTOCOL",
  E_UNSUPPORTED: "jobs.hint.E_UNSUPPORTED",
  E_CANCEL_UNSUPPORTED: "jobs.hint.E_CANCEL_UNSUPPORTED",
  E_RAW_UNSUPPORTED: "jobs.hint.E_RAW_UNSUPPORTED",
  E_PDF_ENCRYPTED: "jobs.hint.E_PDF_ENCRYPTED",
  E_NOT_FOUND: "jobs.hint.E_NOT_FOUND",
  E_TOO_LARGE: "jobs.hint.E_TOO_LARGE",
  E_RENDER_CRASH: "jobs.hint.E_RENDER_CRASH",
  E_ALREADY_RUNNING: "jobs.hint.E_ALREADY_RUNNING",
};

const rawOpen = ref(false);

const statusLabel = computed(() => {
  if (props.job.state === "ok") return t("jobs.card.ok");
  if (props.job.state === "failed") return t("jobs.card.failed");
  return t("jobs.card.running");
});

const codeTitle = computed(() => {
  const code = props.job.code;
  const key = code ? ERROR_TITLES[code] : undefined;
  return key ? t(key) : "";
});

const suggestion = computed(() => {
  const code = props.job.code;
  if (!code) return "";
  const key = CODE_HINTS[code];
  return key ? t(key) : t("jobs.detail.unregisteredCode");
});

const statusValue = computed(() => {
  if (props.job.state === "ok") return "ok";
  if (props.job.state === "failed") return "failed";
  return "running";
});

const rawGroups = computed(() =>
  props.job.events
    .map((event) => ({
      stage: event.stage,
      at: event.at,
      fields: Object.entries(event.fields ?? {}).map(([key, value]) => ({ key, value: String(value) })),
    }))
    .filter((group) => group.fields.length > 0),
);

const hasFields = computed(() => rawGroups.value.length > 0);

/** RFC 3339 nanoseconds → WebKit date parsing; display in the local zone. */
function atText(at: string): string {
  return formatDateTime(at.replace(/(\.\d{3})\d+/, "$1"));
}

function fieldStage(stage: string): string {
  const key = STAGE_KEYS[stage as keyof typeof STAGE_KEYS];
  return key ? t(key) : stage;
}

function legacyCopy(text: string): boolean {
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(area);
    return ok;
  } catch {
    return false;
  }
}

async function copy(text: string, messageKey: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success(t(messageKey));
    return;
  } catch {
    // Fall through to the selection-based copy for restricted webviews.
  }
  if (legacyCopy(text)) ElMessage.success(t(messageKey));
  else ElMessage.error(t("jobs.detail.copyFailed"));
}

function copyTrace(): void {
  void copy(props.job.traceId, "jobs.detail.copiedTrace");
}

function copyJson(): void {
  void copy(serializeTrace(props.job, "json"), "jobs.detail.copiedJson");
}

function download(format: "json" | "jsonl"): void {
  const text = serializeTrace(props.job, format);
  const file = `printkit-trace-${props.job.shortId}.${format}`;
  const type = format === "json" ? "application/json" : "application/x-ndjson";
  try {
    const blob = new Blob([text], { type });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = file;
    link.rel = "noopener";
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    window.setTimeout(() => URL.revokeObjectURL(url), 1000);
    ElMessage.success(
      t("jobs.detail.exported", { count: formatNumber(props.job.events.length), file }),
    );
  } catch {
    ElMessage.error(t("jobs.detail.exportFailed"));
  }
}
</script>

<template>
  <article class="obs-detail">
    <header class="obs-detail-head">
      <div class="obs-detail-heading">
        <el-button text class="obs-back" :aria-label="t('jobs.detail.back')" @click="emit('close')">
          <el-icon><ArrowLeft /></el-icon>
        </el-button>
        <div class="obs-detail-title">
          <h3>{{ t("jobs.detail.title") }}</h3>
          <span class="obs-trace">{{ job.shortId }}</span>
        </div>
        <div class="obs-detail-nav">
          <el-button
            text
            size="small"
            :disabled="!prev"
            :aria-label="t('jobs.detail.prev')"
            :title="t('jobs.detail.prev')"
            @click="prev && emit('select', prev)"
          >
            <el-icon><ArrowLeft /></el-icon>
          </el-button>
          <el-button
            text
            size="small"
            :disabled="!next"
            :aria-label="t('jobs.detail.next')"
            :title="t('jobs.detail.next')"
            @click="next && emit('select', next)"
          >
            <el-icon><ArrowRight /></el-icon>
          </el-button>
        </div>
      </div>

      <div class="obs-detail-actions">
        <el-button text size="small" :aria-label="t('jobs.detail.copyTrace')" @click="copyTrace">
          <el-icon><CopyDocument /></el-icon>
          <span>{{ t("jobs.detail.copyTrace") }}</span>
        </el-button>
        <el-button text size="small" :aria-label="t('jobs.detail.copyJson')" @click="copyJson">
          <el-icon><CopyDocument /></el-icon>
          <span>{{ t("jobs.detail.copyJson") }}</span>
        </el-button>
        <el-button text size="small" @click="download('json')">
          <el-icon><Download /></el-icon>
          <span>{{ t("jobs.detail.exportJson") }}</span>
        </el-button>
        <el-button text size="small" @click="download('jsonl')">
          <el-icon><Download /></el-icon>
          <span>{{ t("jobs.detail.exportJsonl") }}</span>
        </el-button>
      </div>
    </header>

    <div class="obs-detail-body">
      <StageWaterfall :job="job" />

      <section class="obs-block">
        <h4 class="obs-section-title">{{ t("jobs.detail.identity") }}</h4>
        <dl class="obs-kv">
          <dt>{{ t("jobs.detail.trace") }}</dt>
          <dd>
            <button type="button" class="obs-trace-copy" @click="copyTrace">
              <span class="obs-trace-full">{{ job.traceId }}</span>
            <el-icon><CopyDocument /></el-icon>
            </button>
          </dd>
          <dt v-if="job.origin !== undefined">{{ t("jobs.detail.origin") }}</dt>
          <dd v-if="job.origin !== undefined">{{ job.origin }}</dd>
          <dt v-if="job.printer !== undefined">{{ t("jobs.detail.printer") }}</dt>
          <dd v-if="job.printer !== undefined">{{ job.printer }}</dd>
          <dt v-if="job.pages !== undefined">{{ t("jobs.detail.pages") }}</dt>
          <dd v-if="job.pages !== undefined">{{ formatNumber(job.pages) }}</dd>
          <dt v-if="job.extras.template !== undefined">{{ t("jobs.detail.template") }}</dt>
          <dd v-if="job.extras.template !== undefined">{{ job.extras.template }}</dd>
          <dt v-if="job.extras.paper !== undefined">{{ t("jobs.detail.paper") }}</dt>
          <dd v-if="job.extras.paper !== undefined">{{ job.extras.paper }}</dd>
          <dt v-if="job.extras.copies !== undefined">{{ t("jobs.detail.copies") }}</dt>
          <dd v-if="job.extras.copies !== undefined">{{ job.extras.copies }}</dd>
          <dt>{{ t("jobs.detail.started") }}</dt>
          <dd>{{ atText(job.startedAt) }}</dd>
          <dt v-if="job.endedAt !== undefined">{{ t("jobs.detail.ended") }}</dt>
          <dd v-if="job.endedAt !== undefined">{{ atText(job.endedAt) }}</dd>
          <dt v-if="typeof job.totalMs === 'number'">{{ t("jobs.detail.total") }}</dt>
          <dd v-if="typeof job.totalMs === 'number'" class="obs-num">{{ formatMs(job.totalMs) }}</dd>
          <dt v-if="job.queuePos !== undefined">{{ t("jobs.detail.queuePos") }}</dt>
          <dd v-if="job.queuePos !== undefined">{{ t("jobs.card.queue", { pos: job.queuePos }) }}</dd>
        </dl>
      </section>

      <StageTimeline :job="job" />

      <section class="obs-block obs-result" :class="`is-${statusValue}`">
        <h4 class="obs-section-title">{{ t("jobs.detail.result") }}</h4>
        <dl class="obs-kv">
          <dt>{{ t("jobs.detail.status") }}</dt>
          <dd>{{ statusLabel }}</dd>
          <dt v-if="job.code !== undefined">{{ t("jobs.detail.code") }}</dt>
          <dd v-if="job.code !== undefined" class="obs-code">
            {{ job.code }}
            <span v-if="codeTitle" class="obs-muted">· {{ codeTitle }}</span>
          </dd>
          <dt v-if="job.detail !== undefined">{{ t("jobs.detail.detail") }}</dt>
          <dd v-if="job.detail !== undefined" class="obs-detail-text">{{ job.detail }}</dd>
          <dt v-if="job.code !== undefined">{{ t("jobs.detail.suggestion") }}</dt>
          <dd v-if="job.code !== undefined" class="obs-suggestion">{{ suggestion }}</dd>
        </dl>
      </section>

      <section class="obs-block">
        <button type="button" class="obs-raw-toggle" :aria-expanded="rawOpen" @click="rawOpen = !rawOpen">
          <span>{{ t("jobs.detail.raw") }}</span>
          <span class="obs-muted">
            {{ rawOpen ? t("jobs.detail.collapse") : t("jobs.detail.expand") }}
          </span>
        </button>
        <div v-if="rawOpen" class="obs-raw">
          <p v-if="!hasFields" class="obs-muted">{{ t("jobs.detail.noFields") }}</p>
          <div v-for="group in rawGroups" :key="`${group.stage}:${group.at}`" class="obs-raw-group">
            <div class="obs-raw-head">
              <span class="obs-stage-chip" :class="`obs-stage-${group.stage}`">{{ fieldStage(group.stage) }}</span>
              <span class="obs-muted">{{ atText(group.at) }}</span>
            </div>
            <dl class="obs-raw-list">
              <template v-for="field in group.fields" :key="field.key">
                <dt>{{ field.key }}</dt>
                <dd>{{ field.value }}</dd>
              </template>
            </dl>
          </div>
        </div>
        <p class="obs-muted">{{ t("jobs.detail.rawHint") }}</p>
      </section>
    </div>
  </article>
</template>
