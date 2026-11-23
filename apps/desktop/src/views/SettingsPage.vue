<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { Check, Plus, Refresh } from "@element-plus/icons-vue";
import { ElMessage, ElMessageBox } from "element-plus";

import {
  addOrigin,
  getRetentionDays,
  listOrigins,
  openDataDir,
  refreshConfig,
  removeOrigin,
  revealPath,
  setRetentionDays,
} from "../api";
import { setLocale } from "../i18n";
import { useMotion } from "../composables/useMotion";
import { useTheme, type ThemeSetting } from "../composables/useTheme";
import { describeError, i18n } from "../i18n";
import { localeOptions } from "../locales";
import {
  RETENTION_MAX_DAYS,
  RETENTION_MIN_DAYS,
  retentionDaysValid,
  retentionSourceKey,
} from "../settingsRetention";
import type { RetentionView, Snapshot } from "../types";
import Skeleton from "../components/Skeleton.vue";

const props = defineProps<{ snapshot: Snapshot | null }>();
const emit = defineEmits<{ refresh: [] }>();
const { t, errorText, formatNumber } = i18n;
const locale = i18n.locale;
const { theme, setTheme } = useTheme();
const { reduce, setReduce } = useMotion();
const themeModel = computed({
  get: () => theme.value,
  set: (value: ThemeSetting) => setTheme(value),
});
const motionModel = computed({
  get: () => reduce.value,
  set: (value: boolean) => setReduce(value),
});
const error = ref("");
const busy = ref("");

const dataDir = computed(() => props.snapshot?.config?.dataDir ?? "");
const logDir = computed(() => (dataDir.value ? `${dataDir.value}/logs` : ""));
const lockPath = computed(() => props.snapshot?.status.lockPath ?? "");
const languages = computed(() => localeOptions());
const origins = ref<string[]>([]);
const originDraft = ref("");
const originsLoading = ref(false);
const originsBusy = ref(false);
const originsError = ref("");
const originAddError = ref("");
const external = computed(() => props.snapshot?.status.state === "runningExternal");
const retention = ref<RetentionView | null>(null);
const retentionDraft = ref<number | undefined>(undefined);
const retentionLoading = ref(false);
const retentionBusy = ref(false);
let retentionGeneration = 0;
const retentionLoadError = ref("");
const retentionSaveError = ref("");
const retentionRestart = ref(false);
const retentionInvalid = computed(() => !retentionDaysValid(retentionDraft.value));
const retentionComponents = computed(() => {
  const view = retention.value;
  if (!view) return "";
  return t("business.settings.retentionComponents", {
    logDays: formatNumber(view.logDays),
    logSource: retentionSourceLabel(view.logSource),
    recordsDays: formatNumber(view.recordsDays),
    recordsSource: retentionSourceLabel(view.recordsSource),
  });
});

function retentionSourceLabel(source: string): string {
  return t(retentionSourceKey(source));
}

function message(reason: unknown): void {
  error.value = errorText(describeError(reason));
}

async function reread(): Promise<void> {
  if (retentionBusy.value || retentionLoading.value) return;
  busy.value = "config";
  try {
    await refreshConfig();
    emit("refresh");
    await loadRetention();
    ElMessage.success(t("business.settings.rereadDone"));
  } catch (reason) {
    message(reason);
  } finally {
    busy.value = "";
  }
}

async function openDir(): Promise<void> {
  try {
    await openDataDir();
  } catch (reason) {
    message(reason);
  }
}

async function reveal(path: string): Promise<void> {
  try {
    await revealPath(path);
  } catch (reason) {
    message(reason);
  }
}

async function loadRetention(syncRestart = false): Promise<void> {
  if (retentionBusy.value) return;
  const generation = ++retentionGeneration;
  retentionLoading.value = true;
  retentionLoadError.value = "";
  retentionSaveError.value = "";
  try {
    const view = await getRetentionDays();
    if (generation !== retentionGeneration) return;
    retention.value = view;
    retentionDraft.value = view.days;
    if (syncRestart) retentionRestart.value = view.restartRequired;
    else if (view.restartRequired) retentionRestart.value = true;
  } catch (reason) {
    if (generation !== retentionGeneration) return;
    retention.value = null;
    retentionDraft.value = undefined;
    retentionLoadError.value = t("business.settings.retentionUnavailable", {
      reason: errorText(describeError(reason)),
    });
  } finally {
    if (generation === retentionGeneration) retentionLoading.value = false;
  }
}

async function saveRetention(): Promise<void> {
  if (retentionBusy.value || retentionLoading.value) return;
  const days = retentionDraft.value;
  retentionSaveError.value = "";
  if (!retentionDaysValid(days)) {
    retentionSaveError.value = t("business.settings.retentionInvalid");
    return;
  }
  retentionBusy.value = true;
  try {
    const view = await setRetentionDays(days);
    retention.value = view;
    retentionDraft.value = view.days;
    retentionRestart.value = view.restartRequired;
    ElMessage.success(
      view.restartRequired
        ? t("business.settings.retentionSavedRestart")
        : t("business.settings.retentionSaved"),
    );
    emit("refresh");
  } catch (reason) {
    retentionSaveError.value = t("business.settings.retentionSaveFailed", {
      reason: errorText(describeError(reason)),
    });
    ElMessage.error(retentionSaveError.value);
  } finally {
    retentionBusy.value = false;
  }
}

async function loadOrigins(): Promise<void> {
  originsLoading.value = true;
  originsError.value = "";
  originAddError.value = "";
  try {
    origins.value = (await listOrigins()).origins;
  } catch (reason) {
    // Keep the last loaded list visible; the alert explains this refresh.
    originsError.value = t("business.settings.originsUnavailable", {
      reason: errorText(describeError(reason)),
    });
  } finally {
    originsLoading.value = false;
  }
}

async function addAllowedOrigin(): Promise<void> {
  const origin = originDraft.value.trim();
  originAddError.value = "";
  if (!origin) return;
  originsBusy.value = true;
  try {
    origins.value = (await addOrigin(origin)).origins;
    originDraft.value = "";
    ElMessage.success(t("business.settings.originAdded"));
  } catch (reason) {
    originAddError.value = errorText(describeError(reason));
  } finally {
    originsBusy.value = false;
  }
}

async function removeAllowedOrigin(origin: string): Promise<void> {
  try {
    await ElMessageBox.confirm(
      t("business.settings.originRemoveConfirm", { origin }),
      t("business.settings.originRemove"),
      { type: "warning" },
    );
  } catch {
    return;
  }
  originsBusy.value = true;
  originAddError.value = "";
  try {
    origins.value = (await removeOrigin(origin)).origins;
    ElMessage.success(t("business.settings.originRemoved"));
  } catch (reason) {
    originAddError.value = errorText(describeError(reason));
  } finally {
    originsBusy.value = false;
  }
}

function chooseLanguage(value: string): void {
  setLocale(value);
}

onMounted(() => {
  void loadRetention();
  void loadOrigins();
});

watch(
  () => props.snapshot?.status.state,
  (state, previous) => {
    if (!previous || state === previous || retentionBusy.value) return;
    void loadRetention(true);
  },
);
</script>

<template>
  <section class="page settings-page">
    <el-alert v-if="error" :title="error" type="error" :closable="false" show-icon class="page-alert" />
    <el-alert
      v-if="props.snapshot && !props.snapshot.sidecar.compatible"
      :title="props.snapshot.sidecar.message"
      type="error"
      :closable="false"
      show-icon
      class="page-alert"
    />

    <el-card shadow="never">
      <template #header>
        <div class="section-head">
          <span>{{ t("business.settings.paths") }}</span>
          <div class="inline-actions">
            <el-button :icon="Refresh" :loading="busy === 'config'"
            :aria-busy="busy === 'config' ? 'true' : 'false'" :disabled="retentionBusy || retentionLoading" @click="reread">{{ t("business.settings.reread") }}</el-button>
            <el-button @click="openDir">{{ t("business.settings.openDir") }}</el-button>
          </div>
        </div>
      </template>
      <details class="tech-details">
        <summary>{{ t("business.settings.technical") }}</summary>
        <dl class="detail-list">
          <dt>{{ t("business.settings.dataDir") }}</dt>
          <dd>{{ dataDir || t("common.unknown") }}</dd>
          <dt>{{ t("business.settings.logDir") }}</dt>
          <dd>{{ logDir || t("common.unknown") }}</dd>
          <dt>{{ t("business.settings.lock") }}</dt>
          <dd>{{ lockPath || t("common.unknown") }}</dd>
          <dt>{{ t("business.settings.pid") }}</dt>
          <dd>{{ props.snapshot?.status.pid ?? "—" }}</dd>
          <dt>{{ t("business.settings.version") }}</dt>
          <dd>{{ props.snapshot?.sidecar.version || "—" }} ({{ props.snapshot?.sidecar.target || "—" }})</dd>
        </dl>
        <div class="filter-row">
          <el-button :disabled="!logDir" :title="!logDir ? t('business.settings.noLogDir') : undefined" @click="reveal(logDir)">{{ t("business.settings.revealLogs") }}</el-button>
          <el-button :disabled="!lockPath" :title="!lockPath ? t('business.settings.noLockPath') : undefined" @click="reveal(lockPath)">{{ t("business.settings.revealLock") }}</el-button>
        </div>
      </details>
      <p v-if="props.snapshot?.autoStart.error" class="muted">{{ props.snapshot.autoStart.error }}</p>
    </el-card>

    <el-card shadow="never" class="origins-card">
      <template #header>
        <div class="section-head">
          <span>{{ t("business.settings.origins") }}</span>
          <el-button text :icon="Refresh" :loading="originsLoading"
            :aria-busy="originsLoading ? 'true' : 'false'" @click="loadOrigins">
            {{ t("shell.refresh") }}
          </el-button>
        </div>
      </template>
      <p class="muted">{{ t("business.settings.originsHint") }}</p>
      <el-alert
        v-if="originsError"
        :title="originsError"
        type="error"
        :closable="false"
        show-icon
        class="page-alert"
      />
      <Skeleton
        v-if="originsLoading && origins.length === 0 && !originsError"
        :loading="true"
        variant="list"
        :rows="2"
        :label="t('common.loading')"
      />
      <template v-else-if="origins.length > 0 || (!originsLoading && !originsError)">
        <p v-if="origins.length === 0" class="muted">{{ t("business.settings.originsEmpty") }}</p>
        <ul v-else class="origin-list">
          <li v-for="origin in origins" :key="origin" class="origin-row">
            <code>{{ origin }}</code>
            <el-button
              text
              type="danger"
              :disabled="originsBusy || external"
              @click="removeAllowedOrigin(origin)"
            >
              {{ t("business.settings.originRemove") }}
            </el-button>
          </li>
        </ul>
        <div class="filter-row">
          <el-input
            v-model="originDraft"
            :placeholder="t('business.settings.originPlaceholder')"
            clearable
            class="filter-search"
            @keyup.enter="addAllowedOrigin"
          />
          <el-button
            type="primary"
            :icon="Plus"
            :loading="originsBusy"
            :aria-busy="originsBusy ? 'true' : 'false'"
            :disabled="external || !originDraft.trim()"
            @click="addAllowedOrigin"
          >
            {{ t("business.settings.originAdd") }}
          </el-button>
        </div>
        <p v-if="external" class="muted">{{ t("business.settings.originsExternal") }}</p>
        <p v-if="originAddError" class="inline-error">{{ originAddError }}</p>
      </template>
    </el-card>

    <el-card shadow="never" class="retention-card">
      <template #header>
        <div class="section-head">
          <span>{{ t("business.settings.retention") }}</span>
          <span class="muted">{{ t("business.settings.retentionRange") }}</span>
        </div>
      </template>
      <el-alert
        v-if="retentionLoadError"
        :title="retentionLoadError"
        type="error"
        :closable="false"
        show-icon
        class="page-alert"
      />
      <div v-if="retentionLoadError" class="inline-actions">
        <el-button :icon="Refresh" :loading="retentionLoading"
            :aria-busy="retentionLoading ? 'true' : 'false'" :disabled="retentionLoading || retentionBusy" @click="loadRetention()">
          {{ t("business.settings.retentionRetry") }}
        </el-button>
      </div>
      <p v-else-if="retentionLoading" class="muted">
        {{ t("business.settings.retentionLoading") }}
      </p>
      <template v-else-if="retention">
        <div class="setting-row">
          <div class="setting-copy">
            <strong>{{ t("business.settings.retentionDays") }}</strong>
            <p class="muted">{{ t("business.settings.retentionHint") }}</p>
            <p class="muted">{{ t("business.settings.retentionUnified") }}</p>
          </div>
          <el-input-number
            v-model="retentionDraft"
            :min="RETENTION_MIN_DAYS"
            :max="RETENTION_MAX_DAYS"
            :step="1"
            :precision="0"
            :disabled="retentionBusy"
          />
        </div>
        <p class="muted">
          {{ t("business.settings.retentionCurrent", { days: formatNumber(retention.days) }) }}
        </p>
        <p class="muted">
          {{ t("business.settings.retentionSource", { source: retentionSourceLabel(retention.source) }) }}
        </p>
        <p class="muted">{{ retentionComponents }}</p>
        <el-alert
          v-if="retention.restartRequired || retentionRestart"
          :title="t('business.settings.retentionRestart')"
          :description="t('business.settings.retentionRestartHint')"
          type="warning"
          :closable="false"
          show-icon
          class="page-alert"
        />
        <el-alert
          v-if="retentionSaveError"
          :title="retentionSaveError"
          type="error"
          :closable="false"
          show-icon
          class="page-alert"
        />
        <p v-if="retentionInvalid" class="inline-error">
          {{ t("business.settings.retentionInvalid") }}
        </p>
        <div class="inline-actions">
          <el-button
            type="primary"
            :icon="Check"
            :loading="retentionBusy"
            :aria-busy="retentionBusy ? 'true' : 'false'"
            :disabled="retentionInvalid || retentionBusy || retentionLoading || busy === 'config'"
            @click="saveRetention"
          >
            {{ t("business.settings.retentionSave") }}
          </el-button>
        </div>
      </template>
    </el-card>

    <el-card shadow="never" class="appearance-card">
      <template #header>
        <div class="section-head">
          <span>{{ t("business.settings.appearance") }}</span>
        </div>
      </template>
      <div class="setting-row">
        <div class="setting-copy">
          <strong>{{ t("business.settings.theme") }}</strong>
          <p class="muted">{{ t("business.settings.themeHint") }}</p>
        </div>
        <el-radio-group v-model="themeModel">
          <el-radio-button value="system">{{ t("business.settings.themeSystem") }}</el-radio-button>
          <el-radio-button value="light">{{ t("business.settings.themeLight") }}</el-radio-button>
          <el-radio-button value="dark">{{ t("business.settings.themeDark") }}</el-radio-button>
        </el-radio-group>
      </div>
      <div class="setting-row">
        <div class="setting-copy">
          <strong>{{ t("business.settings.reduceMotion") }}</strong>
          <p class="muted">{{ t("business.settings.reduceMotionHint") }}</p>
        </div>
        <el-switch v-model="motionModel" />
      </div>
      <div class="setting-row">
        <div class="setting-copy">
          <strong>{{ t("business.settings.language") }}</strong>
        </div>
        <el-select :model-value="locale" class="filter-select" @update:model-value="chooseLanguage">
          <el-option v-for="language in languages" :key="language.value" :label="language.label" :value="language.value" />
        </el-select>
      </div>
    </el-card>

  </section>
</template>
