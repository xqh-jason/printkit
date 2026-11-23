<script setup lang="ts">
import { nextTick, onMounted, onUnmounted, ref, watch } from "vue";

import { i18n } from "../../i18n";
import BrandMascot from "./BrandMascot.vue";
import type { ShellPage } from "./types";

const props = defineProps<{ pages: ShellPage[]; current: string }>();

const emit = defineEmits<{ select: [id: string] }>();
const { t } = i18n;

const root = ref<HTMLElement | null>(null);
const list = ref<HTMLElement | null>(null);
const scrollable = ref(false);
const fadeLeft = ref(false);
const fadeRight = ref(false);
let observer: ResizeObserver | undefined;

/**
 * The narrow header, in the same order: normal -> tight (tighter gaps, brand
 * stays) -> cramped (less padding) -> crowded (brand/icons drop, smaller type)
 * -> packed (smallest type, list scrolls). Each step is measured, never guessed.
 */
function measure(): void {
  const shell = root.value;
  const tabs = list.value;
  if (!shell || !tabs) return;
  const overflowing = () => tabs.scrollWidth > tabs.clientWidth + 1;
  for (const state of ["", "tight", "cramped", "crowded", "packed"]) {
    shell.dataset.fit = state || "normal";
    // Force layout before the next measurement; the CSS class is the input.
    void shell.offsetWidth;
    if (!overflowing()) break;
  }
  scrollable.value = overflowing();
  updateFade();
}

function updateFade(): void {
  const tabs = list.value;
  if (!tabs) return;
  fadeLeft.value = tabs.scrollLeft > 4;
  fadeRight.value = tabs.scrollLeft + tabs.clientWidth < tabs.scrollWidth - 4;
}

function scrollActive(): void {
  const tabs = list.value;
  if (!tabs) return;
  const active = tabs.querySelector<HTMLElement>(".top-tab.active");
  active?.scrollIntoView({ block: "nearest", inline: "nearest" });
  requestAnimationFrame(updateFade);
}

function select(id: string): void {
  emit("select", id);
  // The parent updates `current`; the watcher scrolls it into view.
}

watch(
  () => props.current,
  () => void nextTick(scrollActive),
);
watch(
  () => props.pages,
  () => void nextTick(() => {
    measure();
    scrollActive();
  }),
  { deep: true },
);

onMounted(() => {
  measure();
  scrollActive();
  observer = new ResizeObserver(measure);
  if (root.value) observer.observe(root.value);
  if (list.value) observer.observe(list.value);
  window.addEventListener("resize", measure);
});

onUnmounted(() => {
  observer?.disconnect();
  window.removeEventListener("resize", measure);
});
</script>

<template>
  <div class="top-tabs" ref="root" data-fit="normal">
    <span class="brand-mark"><BrandMascot /></span>
    <nav
      ref="list"
      class="top-tab-list"
      :class="{ scrollable, 'fade-left': fadeLeft, 'fade-right': fadeRight }"
      :aria-label="t('shell.nav.label')"
      @scroll="updateFade"
    >
      <button
        v-for="page in pages"
        :key="page.id"
        class="top-tab"
        :class="{ active: page.id === current }"
        type="button"
        :aria-current="page.id === current ? 'page' : undefined"
        :data-page="page.id"
        :data-feature="page.feature"
        :title="page.label"
        @click="select(page.id)"
      >
        <el-icon v-if="page.icon" class="nav-icon"><component :is="page.icon" /></el-icon>
        <span class="top-tab-label">{{ page.label }}</span>
      </button>
    </nav>
  </div>
</template>
