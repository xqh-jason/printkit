<script setup lang="ts">
import { computed } from "vue";
import { Expand, Fold } from "@element-plus/icons-vue";

import { useWindowLayout } from "../../composables/useWindowLayout";
import { i18n } from "../../i18n";
import type { Snapshot } from "../../types";
import AppSidebar from "./AppSidebar.vue";
import AppTopTabs from "./AppTopTabs.vue";
import type { ShellPage } from "./types";
import { SIDEBAR_MIN } from "./layout.js";

const props = defineProps<{
  label: string;
  pages: ShellPage[];
  snapshot: Snapshot | null;
  current: string;
  title: string;
}>();
const emit = defineEmits<{ select: [id: string] }>();
const { t } = i18n;

const {
  sidebarVisible,
  sidebarWidth,
  maxWidth,
  toggle,
  setWidth,
  stepWidth,
} = useWindowLayout(props.label);

const rootStyle = computed(() => ({
  "--pk-sidebar-column": sidebarVisible.value ? `${sidebarWidth.value}px` : "0px",
}));

function onResizeEnd(width: number): void {
  setWidth(width, true);
}
</script>

<template>
  <div class="app-shell" :class="{ 'has-sidebar': sidebarVisible }" :style="rootStyle">
    <AppSidebar
      v-if="sidebarVisible"
      :pages="pages"
      :snapshot="snapshot"
      :current="current"
      :width="sidebarWidth"
      :min="SIDEBAR_MIN"
      :max="maxWidth"
      @select="emit('select', $event)"
      @resize="setWidth"
      @resize-end="onResizeEnd"
      @step="stepWidth"
    >
    </AppSidebar>

    <header class="app-header" data-tauri-drag-region>
      <div v-if="sidebarVisible" class="header-title">
        <span class="breadcrumb">{{ t("window.breadcrumb.root") }}</span>
        <span class="breadcrumb-separator">/</span>
        <h1>{{ title }}</h1>
        <slot name="title-extra" />
      </div>
      <div v-else class="header-tabs">
        <AppTopTabs :pages="pages" :current="current" @select="emit('select', $event)" />
      </div>

      <div class="header-actions">
        <button
          class="nav-toggle"
          type="button"
          :aria-label="sidebarVisible ? t('shell.nav.toTabs') : t('shell.nav.toSidebar')"
          :aria-pressed="sidebarVisible"
          :title="t('shell.nav.shortcut')"
          @click="toggle"
        >
          <el-icon><Fold v-if="sidebarVisible" /><Expand v-else /></el-icon>
        </button>
        <slot name="actions" />
      </div>
    </header>


    <main class="app-main">
      <slot />
    </main>
  </div>
</template>
