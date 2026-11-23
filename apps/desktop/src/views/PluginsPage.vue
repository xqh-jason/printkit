<script setup lang="ts">
import { computed, onMounted, ref, toRef, watch } from "vue";
import { ElMessageBox } from "element-plus";
import { ArrowRight, Check, Refresh, Setting } from "@element-plus/icons-vue";

import { pluginList, pluginSet, type PluginSetRequest } from "../api";
import { describeError, i18n } from "../i18n";
import { usePlugins } from "../composables/usePlugins";
import type { PluginConfigProperty, PluginView, PluginsReply, Snapshot } from "../types";

import { pagesForPlugin } from "../plugins/pluginPages";
import { bridgePermissions } from "../plugins/pluginBridge.js";
import EmptyState from "../components/EmptyState.vue";
import Skeleton from "../components/Skeleton.vue";
import StateDot from "../components/StateDot.vue";
import type { StateKind } from "../components/state";

const props = defineProps<{ snapshot: Snapshot | null }>();
const emit = defineEmits<{ refresh: []; "open-page": [id: string] }>();
const { t, errorText } = i18n;
const snapshot = toRef(props, "snapshot");
const plugins = usePlugins(snapshot);

const items = ref<PluginView[]>([]);
const preset = ref<string | null>(null);
const filter = ref("");
const busy = ref("");
const pageError = ref("");
const listError = ref("");
const initialized = ref(false);
const rowError = ref<Record<string, string>>({});
const configId = ref<string | null>(null);
const configDraft = ref<Record<string, unknown>>({});
const configBusy = ref(false);

const external = computed(() => props.snapshot?.status.state === "runningExternal");
const stale = computed(() => Boolean(props.snapshot?.pluginsStale));
const unreachable = computed(() => Boolean(props.snapshot && !props.snapshot.pluginsReachable));
const broken = computed(() => props.snapshot?.pluginsBroken ?? null);
const locked = computed(
  () =>
    external.value ||
    stale.value ||
    unreachable.value ||
    !plugins.known.value ||
    Boolean(broken.value),
);
const filtered = computed(() => {
  const needle = filter.value.trim().toLowerCase();
  if (!needle) return items.value;
  return items.value.filter((plugin) => {
    const title = plugins.label(plugin).toLowerCase();
    return (
      plugin.id.toLowerCase().includes(needle) ||
      plugin.name.toLowerCase().includes(needle) ||
      title.includes(needle) ||
      stateLabel(plugin.state).toLowerCase().includes(needle)
    );
  });
});
const configPlugin = computed(() =>
  configId.value ? items.value.find((plugin) => plugin.id === configId.value) ?? null : null,
);
const configFields = computed(() => {
  const properties = configPlugin.value?.schema?.properties;
  return properties ? Object.entries(properties).sort(([left], [right]) => left.localeCompare(right)) : [];
});

function stateKind(state: PluginView["state"]): StateKind {
  if (state === "on") return "success";
  if (state === "pending") return "warning";
  if (state === "incompatible" || state === "invalid" || state === "broken") return "failed";
  return "idle";
}

function stateLabel(state: PluginView["state"]): string {
  if (state === "on") return t("plugins.state.on");
  if (state === "off") return t("plugins.state.off");
  if (state === "pending") return t("plugins.state.pending");
  if (state === "incompatible") return t("plugins.state.incompatible");
  if (state === "broken") return t("plugins.state.broken");
  return t("plugins.state.invalid");
}
function sourceLabel(source: string): string {
  return source === "user" ? t("plugins.source.user") : t("plugins.source.preset");
}

function kindLabel(kind: string): string {
  return kind === "external" ? t("plugins.kind.external") : t("plugins.kind.builtin");
}

function pageChips(plugin: PluginView): Array<{ id: string; label: string }> {
  return pagesForPlugin(plugin.id).map((page) => ({
    id: page.id,
    label: t(page.titleKey),
  }));
}

function openPage(plugin: PluginView, pageId: string): void {
  if (plugin.state !== "on") return;
  emit("open-page", pageId);
}

function bridgeText(plugin: PluginView): string {
  if (plugin.kind !== "external") return t("plugins.bridge.builtin");
  if (!plugin.permissions?.bridge) return t("plugins.bridge.unknown");
  const bridge = bridgePermissions(plugin.permissions.bridge);
  const value = (state: "allowed" | "denied" | "unknown") => t(`plugins.bridge.${state}`);
  return t("plugins.bridge.external", {
    selfConfig: value(bridge.selfConfig),
    printers: value(bridge.printers),
    records: value(bridge.records),
  });
}

function textOf(value: Record<string, string> | string | undefined, fallback: string): string {
  if (!value) return fallback;
  if (typeof value === "string") return value;
  const locale = i18n.locale.value;
  return value[locale] ?? value.en ?? fallback;
}

function fieldLabel(key: string, property: PluginConfigProperty): string {
  return textOf(property.title, key);
}
function applyReply(reply: PluginsReply): void {
  items.value = reply.plugins ?? [];
  preset.value = reply.preset;
  listError.value = reply.error ?? "";
  initialized.value = true;
  emit("refresh");
}

async function reload(): Promise<void> {
  busy.value = "list";
  pageError.value = "";
  try {
    applyReply(await pluginList());
  } catch (reason) {
    listError.value = errorText(describeError(reason));
    initialized.value = true;
  } finally {
    busy.value = "";
  }
}

async function send(request: PluginSetRequest, id: string): Promise<void> {
  busy.value = id;
  rowError.value = { ...rowError.value, [id]: "" };
  try {
    applyReply(await pluginSet(request));
  } catch (reason) {
    rowError.value = { ...rowError.value, [id]: errorText(describeError(reason)) };
  } finally {
    busy.value = "";
  }
}

async function onToggle(plugin: PluginView, enabled: boolean): Promise<void> {
  if (external.value) {
    rowError.value = { ...rowError.value, [plugin.id]: t("plugins.error.readOnly") };
    return;
  }
  if (!enabled && (plugin.inUse ?? 0) > 0) {
    const detail = plugin.inUseDetail;
    rowError.value = {
      ...rowError.value,
      [plugin.id]: t("plugins.error.inUse", {
        count: plugin.inUse ?? 0,
        ws: detail?.ws ?? 0,
        leases: detail?.leases ?? 0,
      }),
    };
    return;
  }
  let confirmDataCount = false;
  if (!enabled && (plugin.dataCount ?? 0) > 0) {
    try {
      await ElMessageBox.confirm(
        t("plugins.confirmDataCount", { count: plugin.dataCount ?? 0 }),
        t("plugins.disable"),
        { type: "warning" },
      );
      confirmDataCount = true;
    } catch {
      return;
    }
  }
  await send(
    { id: plugin.id, enabled, confirmDataCount },
    plugin.id,
  );
}

async function onPreset(next: "all" | "core"): Promise<void> {
  if (locked.value || preset.value === next) return;
  const affected = items.value.filter((plugin) =>
    next === "core" ? plugin.state === "on" : plugin.state !== "on",
  );
  const conflicts = affected.filter((plugin) => (plugin.inUse ?? 0) > 0);
  if (conflicts.length > 0) {
    pageError.value = t("plugins.preset.conflicts", {
      ids: conflicts.map((plugin) => plugin.id).join(", "),
    });
    return;
  }
  const withData = affected.filter((plugin) => (plugin.dataCount ?? 0) > 0);
  if (withData.length > 0) {
    try {
      await ElMessageBox.confirm(
        t("plugins.preset.dataCount", {
          ids: withData
            .map((plugin) => `${plugin.id} (${plugin.dataCount ?? 0})`)
            .join(", "),
        }),
        t("plugins.preset.title"),
        { type: "warning" },
      );
    } catch {
      return;
    }
  }
  pageError.value = "";
  await send({ preset: next, confirmDataCount: true }, `preset:${next}`);
}
function openConfig(plugin: PluginView): void {
  if (locked.value || !plugin.schema?.properties) return;
  configId.value = plugin.id;
  configDraft.value = { ...(plugin.config ?? {}) };
  for (const [key, property] of Object.entries(plugin.schema.properties)) {
    if (configDraft.value[key] === undefined && property.default !== undefined) {
      configDraft.value[key] = property.default;
    }
  }
}

function closeConfig(): void {
  configId.value = null;
  configDraft.value = {};
}

async function saveConfig(): Promise<void> {
  if (locked.value || !configId.value) return;
  configBusy.value = true;
  try {
    applyReply(await pluginSet({ id: configId.value, config: configDraft.value }));
    closeConfig();
    emit("refresh");
  } catch (reason) {
    rowError.value = {
      ...rowError.value,
      [configId.value]: errorText(describeError(reason)),
    };
  } finally {
    configBusy.value = false;
  }
}

watch(
  () => props.snapshot?.plugins,
  (value) => {
    if (Array.isArray(value)) items.value = value;
  },
);

onMounted(() => void reload());
</script>

<template>
  <section class="page plugins-page">
    <el-alert
      v-if="pageError"
      :title="pageError"
      type="error"
      :closable="false"
      show-icon
      class="page-alert"
    />
    <el-alert
      v-if="listError"
      :title="listError"
      type="warning"
      :closable="false"
      show-icon
      class="page-alert"
    />
    <el-alert
      v-if="stale || unreachable"
      :title="t('plugins.error.stale')"
      type="warning"
      :closable="false"
      show-icon
      class="page-alert"
    />
    <el-alert
      v-if="broken"
      :title="t('plugins.error.broken', { reason: broken })"
      type="error"
      :closable="false"
      show-icon
      class="page-alert"
    />
    <el-alert
      v-if="external"
      :title="t('plugins.error.readOnly')"
      type="info"
      :closable="false"
      show-icon
      class="page-alert"
    />

    <div class="plugin-toolbar">
      <el-input
        v-model="filter"
        :placeholder="t('plugins.filter')"
        clearable
        class="filter-search"
      />
      <div class="plugin-preset">
        <el-button
          :type="preset === 'all' ? 'primary' : 'default'"
          :icon="Check"
          :loading="busy === 'preset:all'"
          :aria-busy="busy === 'preset:all' ? 'true' : 'false'"
          :disabled="locked || busy !== ''"
          @click="onPreset('all')"
        >
          {{ t("plugins.preset.all") }}
        </el-button>
        <el-button
          :type="preset === 'core' ? 'primary' : 'default'"
          :icon="Check"
          :loading="busy === 'preset:core'"
          :aria-busy="busy === 'preset:core' ? 'true' : 'false'"
          :disabled="locked || busy !== ''"
          @click="onPreset('core')"
        >
          {{ t("plugins.preset.core") }}
        </el-button>
      </div>
      <el-button :icon="Refresh" :loading="busy === 'list'"
        :aria-busy="busy === 'list' ? 'true' : 'false'" @click="reload">
        {{ t("shell.refresh") }}
      </el-button>
    </div>

    <Skeleton
      v-if="!initialized || (busy === 'list' && items.length === 0 && !listError)"
      :loading="true"
      variant="list"
      :rows="5"
      :label="t('common.loading')"
    />

    <EmptyState
      v-else-if="initialized && filtered.length === 0 && !busy && !listError"
      :title="t('plugins.empty')"
      :description="t('plugins.emptyHint')"
    />

    <div v-else class="plugin-list">
      <article
        v-for="plugin in filtered"
        :key="plugin.id"
        class="plugin-row"
        :data-feature="plugin.id"
      >
        <div class="plugin-row-main">
          <div class="plugin-row-title">
            <h3>{{ plugins.label(plugin) }}</h3>
            <span v-if="pageChips(plugin).length" class="plugin-page-chips">
              <button
                v-for="page in pageChips(plugin)"
                :key="page.id"
                type="button"
                class="plugin-page-chip"
                :disabled="plugin.state !== 'on'"
                :title="plugin.state !== 'on' ? t('plugins.pages.enableFirst') : t('plugins.openPage')"
                @click="openPage(plugin, page.id)"
              >
                <span>{{ page.label }}</span>
                <el-icon class="plugin-page-chip-arrow"><ArrowRight /></el-icon>
              </button>
            </span>
            <span v-else class="plugin-page-chip plugin-page-chip--empty">
              {{ t("plugins.pages.none") }}
            </span>
          </div>
          <div class="plugin-row-meta">
            <StateDot :kind="stateKind(plugin.state)" :label="stateLabel(plugin.state)" :size="9" />
            <span v-if="plugin.dataCount">{{ t("plugins.dataCount", { count: plugin.dataCount }) }}</span>
          </div>
          <details class="plugin-tech">
            <summary>{{ t("plugins.techDetails") }}</summary>
            <div class="plugin-tech-body">
              <span>{{ t("plugins.techId") }}: {{ plugin.id }}</span>
              <span>{{ t("plugins.techVersion") }}: {{ plugin.version }}</span>
              <span>{{ t("plugins.techKind") }}: {{ kindLabel(plugin.kind) }}</span>
              <span>{{ t("plugins.techSource") }}: {{ sourceLabel(plugin.source) }}</span>
              <span v-if="plugin.inUse">{{ t("plugins.inUse", { count: plugin.inUse }) }}</span>
              <span v-if="plugin.inUseDetail">{{ t("plugins.inUseDetail", { ws: plugin.inUseDetail.ws ?? 0, leases: plugin.inUseDetail.leases ?? 0 }) }}</span>
              <span v-if="plugin.requires.length">{{ t("plugins.requires", { ids: plugin.requires.join(", ") }) }}</span>
              <span v-if="plugin.requiredBy.length">{{ t("plugins.requiredBy", { ids: plugin.requiredBy.join(", ") }) }}</span>
              <span class="plugin-bridge">{{ bridgeText(plugin) }}</span>
            </div>
          </details>
          <p v-if="plugin.reason" class="plugin-reason">{{ plugin.reason }}</p>
          <p v-if="rowError[plugin.id]" class="plugin-error">{{ rowError[plugin.id] }}</p>
        </div>
        <div class="plugin-row-actions">
          <el-switch
            :model-value="plugin.state === 'on'"
            :loading="busy === plugin.id"
            :disabled="locked || busy !== ''"
            :active-text="t('plugins.state.on')"
            :inactive-text="t('plugins.state.off')"
            @update:model-value="onToggle(plugin, $event as boolean)"
          />
          <el-button
            v-if="plugin.schema?.properties"
            text
            :icon="Setting"
            :disabled="locked"
            :title="locked ? t('plugins.configureLocked') : undefined"
            @click="openConfig(plugin)"
          >
            {{ t("plugins.config.title") }}
          </el-button>
        </div>

        <div v-if="configId === plugin.id" class="plugin-config">
          <h4>{{ t("plugins.config.title") }}</h4>
          <div v-for="[key, property] in configFields" :key="key" class="setting-row">
            <label class="setting-label" :for="`plugin-${plugin.id}-${key}`">
              {{ fieldLabel(key, property) }}
            </label>
            <div class="setting-control">
              <el-switch
                v-if="property.type === 'boolean'"
                :id="`plugin-${plugin.id}-${key}`"
                v-model="configDraft[key] as boolean"
              />
              <el-select
                v-else-if="property.enum"
                :id="`plugin-${plugin.id}-${key}`"
                v-model="configDraft[key]"
                class="filter-select"
              >
                <el-option
                  v-for="choice in property.enum"
                  :key="String(choice)"
                  :label="String(choice)"
                  :value="choice"
                />
              </el-select>
              <el-input-number
                v-else-if="property.type === 'number' || property.type === 'integer'"
                :id="`plugin-${plugin.id}-${key}`"
                v-model="configDraft[key] as number"
                :min="property.minimum"
                :max="property.maximum"
              />
              <el-input
                v-else
                :id="`plugin-${plugin.id}-${key}`"
                v-model="configDraft[key] as string"
              />
              <p v-if="textOf(property.description, '')" class="setting-hint">
                {{ textOf(property.description, "") }}
              </p>
            </div>
          </div>
          <div class="plugin-config-actions">
            <el-button type="primary" :icon="Check" :loading="configBusy"
            :aria-busy="configBusy ? 'true' : 'false'" :disabled="locked" @click="saveConfig">
              {{ t("plugins.config.save") }}
            </el-button>
            <el-button @click="closeConfig">{{ t("plugins.config.cancel") }}</el-button>
          </div>
        </div>
      </article>
    </div>
  </section>
</template>
