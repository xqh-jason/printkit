<script setup lang="ts">
import { computed, ref } from "vue";
import { i18n } from "../../i18n";
import { brandWordmarkVisible } from "./layout.js";
import BrandMascot from "./BrandMascot.vue";
import SidebarAddress from "./SidebarAddress.vue";
import type { ShellPage } from "./types";
import type { Snapshot } from "../../types";
const props = defineProps<{
  pages: ShellPage[];
  snapshot: Snapshot | null;
  current: string;
  width: number;
  min: number;
  max: number;
}>();
const emit = defineEmits<{
  select: [id: string];
  resize: [width: number];
  resizeEnd: [width: number];
  step: [direction: 1 | -1, large: boolean];
}>();

const sections = computed(() => {
  const byGroup = {
    core: [] as ShellPage[],
    plugins: [] as ShellPage[],
  };
  for (const page of props.pages) byGroup[page.group].push(page);
  return (["core", "plugins"] as const)
    .map((id) => ({ id, pages: byGroup[id] }))
    .filter((section) => section.pages.length > 0);
});

const { t } = i18n;
const handle = ref<HTMLElement | null>(null);
const dragging = ref(false);
let startX = 0;
let startWidth = 0;
let lastWidth = 0;

const shellStyle = computed(() => ({ "--pk-sidebar-width": `${props.width}px` }));

function begin(event: PointerEvent): void {
  if (event.button !== 0) return;
  dragging.value = true;
  startX = event.clientX;
  startWidth = props.width;
  lastWidth = props.width;
  handle.value?.setPointerCapture(event.pointerId);
  event.preventDefault();
}

function move(event: PointerEvent): void {
  if (!dragging.value) return;
  lastWidth = Math.round(startWidth + event.clientX - startX);
  emit("resize", lastWidth);
}

function end(event: PointerEvent): void {
  if (!dragging.value) return;
  dragging.value = false;
  handle.value?.releasePointerCapture(event.pointerId);
  emit("resizeEnd", lastWidth);
}

function onKey(event: KeyboardEvent): void {
  if (event.key === "ArrowLeft") {
    event.preventDefault();
    emit("step", -1, event.shiftKey);
  } else if (event.key === "ArrowRight") {
    event.preventDefault();
    emit("step", 1, event.shiftKey);
  }
}
</script>

<template>
  <aside class="app-sidebar" :class="{ narrow: !brandWordmarkVisible(width) }" :style="shellStyle" :aria-label="t('shell.nav.label')">
    <div class="brand">
      <span class="brand-mark"><BrandMascot /></span>
      <span class="brand-name">printkit</span>
    </div>
    <nav class="nav">
      <template v-for="section in sections" :key="section.id">
        <p class="nav-group">{{ t(`window.group.${section.id}`) }}</p>
        <button
          v-for="page in section.pages"
          :key="page.id"
          class="nav-item"
          :class="{ active: page.id === current }"
          type="button"
          :aria-current="page.id === current ? 'page' : undefined"
          :data-page="page.id"
          :data-feature="page.feature"
          @click="emit('select', page.id)"
        >
          <el-icon v-if="page.icon" class="nav-icon"><component :is="page.icon" /></el-icon>
          <span class="nav-label">{{ page.label }}</span>
        </button>
      </template>
    </nav>
    <div class="sidebar-foot">
      <SidebarAddress :snapshot="snapshot" @open-settings="emit('select', 'settings')" />
    </div>
    <div
      ref="handle"
      class="sidebar-resizer"
      :class="{ dragging }"
      role="separator"
      aria-orientation="vertical"
      :aria-label="t('shell.nav.resize')"
      :aria-valuemin="min"
      :aria-valuemax="max"
      :aria-valuenow="width"
      tabindex="0"
      @pointerdown="begin"
      @pointermove="move"
      @pointerup="end"
      @pointercancel="end"
      @keydown="onKey"
    />
  </aside>
</template>
