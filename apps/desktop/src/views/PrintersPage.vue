<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { ElMessage, ElMessageBox } from "element-plus";
import { CircleClose, Delete, Document, Printer as PrinterIcon, Refresh, VideoPlay } from "@element-plus/icons-vue";

import {
  jobCancel,
  jobStatus,
  listPapers,
  listPrinters,
  printPdfBytes,
  printTest,
  type JobStatusReply,
  type JobWarning,
} from "../api";
import { describeError, i18n } from "../i18n";
import { canRunPdf, isJobTerminal, jobOutcome, pdfAttemptNonce } from "../pdfPrint";
import {
  beginPdfAttempt,
  markPdfTerminal,
  markPdfTimeout,
  pdfLocked as pdfActionsLocked,
  refreshKey,
  settlePdfAttempt,
  type PdfAttempt,
} from "../pdfAttempt";
import type { Paper, Printer, Snapshot } from "../types";
import ServiceBanner from "../components/ServiceBanner.vue";
import EmptyState from "../components/EmptyState.vue";
import ListRow from "../components/ListRow.vue";
import Skeleton from "../components/Skeleton.vue";
import StateDot from "../components/StateDot.vue";

const { t, errorText } = i18n;
const props = defineProps<{ snapshot: Snapshot | null }>();
const emit = defineEmits<{ refresh: [] }>();
const printers = ref<Printer[]>([]);
const papers = ref<Paper[]>([]);
const selected = ref("");
const loading = ref(false);
const papersLoading = ref(false);
const error = ref("");
const papersError = ref("");
const copies = ref(1);
const busy = ref(false);
const result = ref("");
const pdfInput = ref<HTMLInputElement | null>(null);
const pdfFile = ref<{ name: string; size: number; bytes: string } | null>(null);
const pdfBusy = ref(false);
const pdfError = ref("");
const pdfResult = ref<{ pages?: number; jobId?: string } | null>(null);
const pdfJob = ref<JobStatusReply | null>(null);
const pdfJobError = ref("");
const pdfWarnings = ref<JobWarning[]>([]);
const pdfAttempt = ref<PdfAttempt | null>(null);
const pdfTimedOut = computed(() => Boolean(pdfAttempt.value?.timedOut));
let pdfTimer: number | undefined;
const MAX_PDF_BYTES = 64 * 1024 * 1024;
const document = ref(`{
  "version": 1,
  "pages": [
    {
      "spec": { "size": "A4" },
      "items": [
        {
          "id": "test",
          "type": "text",
          "frame": { "x": 10, "y": 10, "width": 150, "height": 12 },
          "style": { "fontSize": 6, "lineHeight": 1.4, "color": "#000000", "align": "left" },
          "data": { "text": "printkit 测试打印" }
        }
      ]
    }
  ]
}`);

const current = computed(() => printers.value.find((printer) => printer.name === selected.value) ?? null);
const owned = computed(
  () =>
    props.snapshot?.status.state === "runningOwned" &&
    props.snapshot?.status.reachable === true,
);
const writeReason = computed(() => {
  const service = props.snapshot?.status;
  if (!service || service.state !== "runningOwned" || !service.reachable) {
    return t(
      service?.state === "runningExternal"
        ? "business.printPdf.external"
        : "business.printPdf.stopped",
    );
  }
  return current.value ? "" : t("business.printTest.noPrinter");
});
const canTestPrint = computed(() => writeReason.value === "");
const canPdf = computed(() => owned.value && Boolean(current.value) && Boolean(pdfFile.value));
const pdfLocked = computed(() =>
  pdfActionsLocked(pdfBusy.value, pdfJob.value?.status, pdfAttempt.value),
);
const capabilities = computed(() => {
  const printer = current.value;
  return [
    { id: "raw", label: t("business.printers.raw"), on: Boolean(printer?.raw) },
    { id: "duplex", label: t("business.printers.duplex"), on: Boolean(printer?.duplex) },
    { id: "paperSource", label: t("business.printers.paperSource"), on: Boolean(printer?.paperSource) },
  ];
});

async function loadPrinters(): Promise<void> {
  loading.value = true;
  error.value = "";
  try {
    const next = await listPrinters();
    printers.value = next;
    const keep = next.some((printer) => printer.name === selected.value) ? selected.value : "";
    selected.value = keep || (next.find((printer) => printer.isDefault)?.name ?? next[0]?.name ?? "");
    await loadPapers();
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    loading.value = false;
  }
}

async function loadPapers(): Promise<void> {
  papersError.value = "";
  if (!selected.value) {
    papers.value = [];
    return;
  }
  papersLoading.value = true;
  papers.value = [];
  try {
    papers.value = await listPapers(selected.value);
  } catch (reason) {
    papersError.value = errorText(describeError(reason));
  } finally {
    papersLoading.value = false;
  }
}

function select(name: string): void {
  selected.value = name;
  void loadPapers();
}

function pickPdf(): void {
  pdfInput.value?.click();
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let index = 0; index < bytes.length; index += chunk) {
    binary += String.fromCharCode(...bytes.subarray(index, index + chunk));
  }
  return btoa(binary);
}

async function onPdfChange(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = "";
  pdfError.value = "";
  pdfResult.value = null;
  pdfAttempt.value = settlePdfAttempt();
  pdfWarnings.value = [];
  if (!file) return;
  if (!file.name.toLowerCase().endsWith(".pdf")) {
    pdfError.value = t("business.printPdf.notPdf");
    pdfFile.value = null;
    return;
  }
  if (file.size > MAX_PDF_BYTES) {
    pdfError.value = t("business.printPdf.tooLarge", { limit: 64 });
    pdfFile.value = null;
    return;
  }
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    pdfFile.value = { name: file.name, size: file.size, bytes: bytesToBase64(bytes) };
  } catch (reason) {
    pdfError.value = errorText(describeError(reason));
    pdfFile.value = null;
  }
}

function stopPolling(): void {
  if (pdfTimer !== undefined) window.clearTimeout(pdfTimer);
  pdfTimer = undefined;
}

function clearPdf(): void {
  stopPolling();
  pdfFile.value = null;
  pdfResult.value = null;
  pdfJob.value = null;
  pdfJobError.value = "";
  pdfWarnings.value = [];
  pdfAttempt.value = settlePdfAttempt();
  pdfError.value = "";
}

function jobStatusLabel(status: JobStatusReply["status"]): string {
  return t(`business.printPdf.status.${status}`);
}

function pollJob(jobId: string, deadline: number): void {
  stopPolling();
  pdfTimer = window.setTimeout(async () => {
    try {
      const reply = await jobStatus(jobId);
      pdfJob.value = reply;
      pdfWarnings.value = reply.warnings ?? [];
      if (reply.pages !== undefined) {
        pdfResult.value = { jobId, pages: reply.pages };
      }
      const outcome = jobOutcome(reply, deadline);
      if (outcome === "done") {
        stopPolling();
        pdfAttempt.value = markPdfTerminal(pdfAttempt.value, outcome);
        ElMessage.success(t("business.printPdf.done"));
        emit("refresh");
        return;
      }
      if (outcome === "failed") {
        stopPolling();
        pdfAttempt.value = markPdfTerminal(pdfAttempt.value, outcome);
        pdfJobError.value = reply.error
          ? `${reply.error.code}: ${reply.error.message}`
          : t("business.printPdf.failedUnknown");
        ElMessage.error(pdfJobError.value);
        return;
      }
      if (outcome === "cancelled") {
        stopPolling();
        pdfAttempt.value = markPdfTerminal(pdfAttempt.value, outcome);
        ElMessage.warning(t("business.printPdf.cancelled"));
        return;
      }
      if (outcome === "timeout") {
        stopPolling();
        pdfAttempt.value = markPdfTimeout(pdfAttempt.value);
        pdfJobError.value = t("business.printPdf.unknown");
        return;
      }
      pollJob(jobId, deadline);
    } catch (reason) {
      stopPolling();
      pdfJobError.value = errorText(describeError(reason));
    }
  }, 500);
}

async function cancelPdf(): Promise<void> {
  const jobId = pdfJob.value?.jobId;
  if (!jobId || !owned.value) return;
  try {
    await jobCancel(jobId);
    const reply = await jobStatus(jobId);
    pdfJob.value = reply;
    if (reply.status === "cancelled") {
      stopPolling();
      pdfAttempt.value = settlePdfAttempt();
      ElMessage.info(t("business.printPdf.cancelled"));
    }
  } catch (reason) {
    pdfJobError.value = errorText(describeError(reason));
  }
}

async function runPdf(skipConfirm = false): Promise<void> {
  const file = pdfFile.value;
  if (!owned.value) {
    pdfError.value = writeReason.value || t("business.printTest.unavailable");
    return;
  }
  if (!canRunPdf(file, pdfBusy.value, pdfLocked.value) || !file) return;
  if (!skipConfirm) {
    try {
      await ElMessageBox.confirm(
        t("business.printPdf.confirm", { file: file.name }),
        t("business.printPdf.run"),
        { type: "warning" },
      );
    } catch {
      return;
    }
  }
  pdfWarnings.value = [];
  pdfAttempt.value = beginPdfAttempt(
    file,
    selected.value,
    copies.value,
    pdfAttemptNonce(),
  );
  const key = pdfAttempt.value.key;
  pdfBusy.value = true;
  pdfError.value = "";
  try {
    const reply = (await printPdfBytes(
      file.bytes,
      file.name,
      selected.value || undefined,
      copies.value,
      key,
    )) as { jobId?: string; pageCount?: number };
    if (!reply?.jobId) {
      throw new Error("E_PROTOCOL: the service returned no job id");
    }
    pdfJobError.value = "";
    pdfWarnings.value = (reply as { warnings?: JobWarning[] }).warnings ?? [];
    pdfJob.value = { jobId: reply.jobId, status: "queued", pages: reply.pageCount };
    pdfResult.value = { jobId: reply.jobId, pages: reply.pageCount };
    ElMessage.info(t("business.printPdf.submitted"));
    emit("refresh");
    pollJob(reply.jobId, Date.now() + 60_000);
  } catch (reason) {
    pdfAttempt.value = settlePdfAttempt();
    pdfError.value = errorText(describeError(reason));
    ElMessage.error(pdfError.value);
  } finally {
    pdfBusy.value = false;
  }
}

async function refreshPdfStatus(): Promise<void> {
  const jobId = pdfJob.value?.jobId;
  const key = refreshKey(pdfAttempt.value);
  if (!jobId || !key) return;
  try {
    const reply = await jobStatus(jobId);
    pdfJob.value = reply;
    pdfWarnings.value = reply.warnings ?? [];
    if (isJobTerminal(reply.status)) {
      pdfAttempt.value = markPdfTerminal(pdfAttempt.value, reply.status);
    }
    if (reply.status === "done") ElMessage.success(t("business.printPdf.done"));
    if (reply.status === "failed") {
      pdfJobError.value = reply.error
        ? `${reply.error.code}: ${reply.error.message}`
        : t("business.printPdf.failedUnknown");
    }
  } catch (reason) {
    pdfJobError.value = errorText(describeError(reason));
  }
}

async function reprintPdf(): Promise<void> {
  try {
    await ElMessageBox.confirm(
      t("business.printPdf.reprintConfirm"),
      t("business.printPdf.reprint"),
      { type: "warning" },
    );
  } catch {
    return;
  }
  // A new attempt nonce means this is a new job, never the unknown old one.
  await runPdf(true);
}

function warningText(warning: JobWarning): string {
  if (warning.code === "W_PDF_MERGE_METADATA_LOST") {
    return t("business.printPdf.warning.metadataLost");
  }
  return warning.message;
}

async function runTest(): Promise<void> {
  if (!canTestPrint.value) {
    error.value = t("business.printTest.unavailable");
    return;
  }
  try {
    await ElMessageBox.confirm(t("business.printTest.confirm"), t("business.printTest.run"), {
      type: "warning",
    });
  } catch {
    return;
  }
  busy.value = true;
  result.value = "";
  try {
    JSON.parse(document.value);
    result.value = JSON.stringify(
      await printTest(document.value, selected.value || undefined, copies.value),
      null,
      2,
    );
    ElMessage.success(t("business.printTest.sent"));
  } catch (reason) {
    error.value = errorText(describeError(reason));
  } finally {
    busy.value = false;
  }
}
function paperKind(kind: Paper["kind"]): string {
  return t(`business.printers.kind.${kind}`);
}

onMounted(() => void loadPrinters());
onUnmounted(stopPolling);
</script>

<template>
  <section class="page printers-page">
    <ServiceBanner :snapshot="snapshot" @refresh="emit('refresh')" />
    <el-alert v-if="error" :title="error" type="error" :closable="false" show-icon class="page-alert" />

    <div class="split business-split">
      <section class="panel list-panel">
        <header class="panel-head">
          <h2>{{ t("business.printers.title") }}</h2>
          <el-button text @click="loadPrinters">{{ t("shell.refresh") }}</el-button>
        </header>
        <Skeleton v-if="loading && printers.length === 0" :loading="true" variant="list" :rows="5" :label="t('common.loading')" />
        <EmptyState
          v-else-if="printers.length === 0"
          :title="t('business.printers.empty')"
          :description="t('business.printers.emptyHint')"
          :action-label="t('shell.refresh')"
          @action="loadPrinters"
        />
        <div v-else class="printer-list">
          <ListRow
            v-for="printer in printers"
            :key="printer.name"
            :title="printer.name"
            :subtitle="printer.mimeTypes.length ? printer.mimeTypes.join(' · ') : undefined"
            :active="selected === printer.name"
            :state="printer.isDefault ? 'success' : undefined"
            :state-label="printer.isDefault ? t('common.default') : undefined"
            @select="select(printer.name)"
          />
        </div>
      </section>

      <section class="panel detail-panel">
        <header class="panel-head">
          <h2>{{ t("business.printers.detail") }}</h2>
        </header>
        <EmptyState v-if="!current" :title="t('business.printers.select')" />
        <div v-else class="printer-detail">
          <div class="detail-head">
            <h3>{{ current.name }}</h3>
            <StateDot v-if="current.isDefault" kind="success" :label="t('business.printers.default')" />
          </div>
          <div class="detail-block">
            <p class="detail-label">{{ t("business.printers.capabilities") }}</p>
            <div class="capability-row">
              <span
                v-for="capability in capabilities"
                :key="capability.id"
                class="capability"
                :class="{ 'is-off': !capability.on }"
              >
                <span class="capability-mark" aria-hidden="true">{{ capability.on ? "✓" : "✕" }}</span>
                {{ capability.label }}
              </span>
            </div>
            <p v-if="current.mimeTypes.length" class="section-sub">
              {{ t("business.printers.formats") }}: {{ current.mimeTypes.join(" · ") }}
            </p>
            <p class="muted">{{ t("business.printers.v1CapabilityHint") }}</p>
          </div>
          <div class="detail-block">
            <p class="detail-label">{{ t("business.printers.papers") }}</p>
            <Skeleton v-if="papersLoading" :loading="true" variant="list" :rows="3" :label="t('common.loading')" />
            <p v-else-if="papersError" class="inline-error">{{ papersError }}</p>
            <EmptyState v-else-if="papers.length === 0" :title="t('business.printers.noPapers')" />
            <ul v-else class="paper-list">
              <li v-for="paper in papers" :key="paper.name" class="paper-row">
                <span class="paper-name">{{ paper.name }}</span>
                <span class="paper-size">{{ paper.widthMm }} × {{ paper.heightMm }} mm</span>
                <span class="paper-kind">{{ paperKind(paper.kind) }}</span>
              </li>
            </ul>
          </div>
        </div>
      </section>
    </div>

    <el-card shadow="never" class="pdf-print-card">
      <template #header>
        <div class="section-head">
          <span>{{ t("business.printPdf.title") }}</span>
          <div class="inline-actions">
            <input
              ref="pdfInput"
              type="file"
              accept="application/pdf,.pdf"
              class="pdf-input"
              @change="onPdfChange"
            />
            <el-button :icon="Document" :disabled="!owned || pdfLocked" @click="pickPdf">{{ t("business.printPdf.choose") }}</el-button>
            <el-button
              type="primary"
              :icon="PrinterIcon"
              :loading="pdfBusy"
              :aria-busy="pdfBusy ? 'true' : 'false'"
              :disabled="!canPdf || pdfLocked"
              @click="runPdf()"
            >
              {{ t("business.printPdf.run") }}
            </el-button>
            <el-button v-if="pdfFile" :icon="Delete" :disabled="pdfBusy" @click="clearPdf">
              {{ t("business.printPdf.clear") }}
            </el-button>
            <el-button
              v-if="pdfJob && (pdfJob.status === 'queued' || pdfJob.status === 'printing')"
              :icon="CircleClose"
              :disabled="pdfBusy || !owned"
              @click="cancelPdf"
            >
              {{ t("business.printPdf.cancel") }}
            </el-button>
          </div>
        </div>
      </template>
      <p v-if="writeReason" class="muted">{{ writeReason }}</p>
      <p v-if="!pdfFile" class="muted">{{ t("business.printPdf.hint") }}</p>
      <div v-else class="pdf-summary">
        <span class="pdf-name">{{ pdfFile.name }}</span>
        <span class="muted">{{ Math.ceil(pdfFile.size / 1024) }} KB</span>
        <span v-if="pdfResult?.pages !== undefined" class="muted">
          {{ t("business.printPdf.pages", { count: pdfResult.pages }) }}
        </span>
        <span v-if="pdfResult?.jobId" class="obs-trace">{{ pdfResult.jobId }}</span>
        <span v-if="pdfJob" class="pdf-job-status" :class="`is-${pdfJob.status}`">
          {{ jobStatusLabel(pdfJob.status) }}
          <template v-if="pdfJob.queuePos !== undefined"> · {{ t("business.printPdf.queue", { pos: pdfJob.queuePos }) }}</template>
        </span>
      </div>
      <p v-if="pdfJobError" class="inline-error">{{ pdfJobError }}</p>
      <el-alert
        v-for="warning in pdfWarnings"
        :key="warning.code"
        :title="warning.code"
        :description="warningText(warning)"
        type="warning"
        :closable="false"
        show-icon
        class="pdf-warning"
      />
      <div v-if="pdfTimedOut" class="inline-actions">
        <el-button :icon="Refresh" :loading="pdfBusy"
              :aria-busy="pdfBusy ? 'true' : 'false'" @click="refreshPdfStatus">
          {{ t("business.printPdf.refresh") }}
        </el-button>
        <el-button type="primary" :icon="Refresh" :loading="pdfBusy"
              :aria-busy="pdfBusy ? 'true' : 'false'" @click="reprintPdf">
          {{ t("business.printPdf.reprint") }}
        </el-button>
      </div>
      <el-alert
        v-if="pdfError"
        :title="pdfError"
        type="error"
        :closable="false"
        show-icon
        class="page-alert"
      />
      <p class="muted">{{ t("business.printPdf.printerHint") }}</p>
    </el-card>

    <el-card shadow="never" class="test-print-card">
      <template #header>
        <div class="section-head">
          <span>{{ t("business.printTest.title") }}</span>
          <el-button type="primary" :icon="VideoPlay" :loading="busy"
          :aria-busy="busy ? 'true' : 'false'" :disabled="!canTestPrint" @click="runTest">
            {{ t("business.printTest.run") }}
          </el-button>
        </div>
      </template>
      <div class="filter-row">
        <el-select v-model="selected" class="filter-select" :placeholder="t('business.printers.filter')">
          <el-option v-for="item in printers" :key="item.name" :label="item.name" :value="item.name" />
        </el-select>
        <el-input-number v-model="copies" :min="1" :max="999" controls-position="right" />
      </div>
      <p class="muted">{{ t("business.overview.testPrintHint") }}</p>
      <p v-if="writeReason" class="muted">{{ writeReason }}</p>
      <p class="muted">{{ t("business.printTest.knownLimit") }}</p>
      <el-input v-model="document" type="textarea" :rows="14" class="json-editor" />
    </el-card>
    <el-card v-if="result" shadow="never" class="table-card">
      <template #header>{{ t("business.printTest.result") }}</template>
      <pre class="code-block">{{ result }}</pre>
    </el-card>
  </section>
</template>

<style scoped>
.pdf-input {
  display: none;
}
.pdf-summary {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 8px;
  margin-bottom: 6px;
}
.pdf-name {
  font-weight: var(--pk-font-weight-medium);
  overflow-wrap: anywhere;
}
.pdf-job-status {
  color: var(--pk-text-muted);
  font-size: var(--pk-font-size-caption);
}
.pdf-job-status.is-failed {
  color: var(--pk-danger);
}
.pdf-job-status.is-done {
  color: var(--pk-success);
}
</style>
