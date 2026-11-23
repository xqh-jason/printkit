<script setup lang="ts">
import { computed, onMounted, ref, toRef, watch } from "vue";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

import { applyDocumentLocale, i18n } from "../i18n";
import { buildShellPages } from "../plugins/pluginPages";
import type { Snapshot } from "../types";
import AppShell from "../components/shell/AppShell.vue";

const props = defineProps<{ snapshot: Snapshot | null; revision: number }>();
const emit = defineEmits<{ refresh: [] }>();
const { t } = i18n;
const locale = i18n.locale;
const snapshot = toRef(props, "snapshot");
const current = ref("overview");

const pages = computed(() => buildShellPages(snapshot.value, t, locale.value));
const pageItems = computed(() =>
  pages.value.map((page) => ({
    id: page.id,
    label: page.label,
    group: page.group,
    icon: page.icon,
    feature: page.feature,
  })),
);
const currentPage = computed(() => pages.value.find((page) => page.id === current.value) ?? null);
const currentComponent = computed(() => currentPage.value?.component ?? pages.value[0]?.component);
const optional = computed(() => Boolean(currentPage.value?.feature));

function selectView(id: string): void {
  if (!pages.value.some((page) => page.id === id)) return;
  current.value = id;
}

function updateTitle(): void {
  const value = t("shell.windowTitle");
  document.title = value;
  try {
    void getCurrentWebviewWindow().setTitle(value);
  } catch {
    // browser preview
  }
}

// A plugin can turn off while its page is open. Drop the page and return to
// Overview instead of keeping stale data or a dead navigation entry.
watch(pages, (list) => {
  if (list.some((page) => page.id === current.value)) return;
  current.value = "overview";
});

watch(locale, () => {
  applyDocumentLocale();
  updateTitle();
});

onMounted(updateTitle);
</script>

<template>
  <div class="business-shell">
  <AppShell
    label="business"
    :pages="pageItems"
    :snapshot="snapshot"
    :current="current"
    :title="currentPage?.label ?? t('shell.windowTitle')"
    @select="selectView"
  >
    <Transition name="pk-page" mode="out-in">
      <KeepAlive v-if="!optional" :max="4">
        <component
          :is="currentComponent"
          :key="current"
          :snapshot="snapshot"
          :revision="props.revision"
          @refresh="emit('refresh')"
          @open-page="selectView"
        />
      </KeepAlive>
      <component
        :is="currentComponent"
        v-else
        :key="current"
        :snapshot="snapshot"
        :revision="props.revision"
        @refresh="emit('refresh')"
        @open-page="selectView"
      />
    </Transition>
  </AppShell>
  </div>
</template>
