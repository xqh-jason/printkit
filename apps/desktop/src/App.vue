<script setup lang="ts">
import { computed, defineAsyncComponent, onBeforeMount, onMounted, onUnmounted, ref } from "vue";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { listen } from "@tauri-apps/api/event";
import en from "element-plus/es/locale/lang/en";
import zhCn from "element-plus/es/locale/lang/zh-cn";
import { ElMessage } from "element-plus";

import { fetchSnapshot } from "./api";
import { applyDocumentLocale, i18n } from "./i18n";
import type { Snapshot } from "./types";

const BusinessShell = defineAsyncComponent(() => import("./views/BusinessShell.vue"));


// Drives the macOS overlay inset; guarded for SSR/browser-preview imports.
function applyPlatform(): void {
  if (typeof document === "undefined" || typeof navigator === "undefined") return;
  document.documentElement.dataset.platform = /Mac/i.test(navigator.userAgent)
    ? "macos"
    : "other";
}
const { t } = i18n;
const locale = i18n.locale;
const snapshot = ref<Snapshot | null>(null);
const revision = ref(0);
let timer: number | undefined;
let unlisten: Array<() => void> = [];

const elLocale = computed(() => (locale.value === "zh-CN" ? zhCn : en));

function updateTitle(): void {
  const title = t("shell.windowTitle");
  document.title = title;
  try {
    void getCurrentWebviewWindow().setTitle(title);
  } catch {
    // browser preview
  }
}

async function refresh(): Promise<void> {
  try {
    snapshot.value = await fetchSnapshot();
  } catch {
    // page-level alerts own the detail
  }
}

function onStorage(event: StorageEvent): void {
  if (event.key !== "printkit.locale" || !event.newValue) return;
  i18n.setLocale(event.newValue);
  applyDocumentLocale();
  updateTitle();
}
onBeforeMount(applyPlatform);

onMounted(async () => {
  applyDocumentLocale();
  updateTitle();
  unlisten = await Promise.all([
    listen("service-changed", () => void refresh()),
    listen("data-changed", () => {
      revision.value += 1;
      void refresh();
    }),
    listen("plugins-changed", () => {
      revision.value += 1;
      void refresh();
    }),
    listen<string>("desktop-notice", (event) => {
      ElMessage.warning(String(event.payload));
    }),
  ]);
  window.addEventListener("storage", onStorage);
  await refresh();
  timer = window.setInterval(() => void refresh(), 2000);
});

onUnmounted(() => {
  if (timer !== undefined) window.clearInterval(timer);
  window.removeEventListener("storage", onStorage);
  for (const stop of unlisten) stop();
});
</script>
<template>
  <el-config-provider :locale="elLocale">
    <div class="app-root">
      <BusinessShell :snapshot="snapshot" :revision="revision" @refresh="refresh" />
    </div>
  </el-config-provider>
</template>
