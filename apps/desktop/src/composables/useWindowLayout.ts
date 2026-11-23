import { computed, onMounted, onUnmounted, ref } from "vue";

import {
  LEGACY_RAIL_MAX,
  clampSidebarWidth,
  maxSidebarWidth,
  migrateSidebarWidth,
  resolveNav,
  sidebarWidth,
  stepSidebarWidth,
  viewportWidth,
  type NavPreference,
} from "../components/shell/layout";

const PREFIX = "printkit";
const NAV_KEY = `${PREFIX}.navLayout`;
const WIDTH_KEY = `${PREFIX}.sidebarWidth`;
const LEGACY_COLLAPSED_KEY = `${PREFIX}.sidebarCollapsed`;

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage can be unavailable; the in-memory choice still applies.
  }
}

function remove(key: string): void {
  try {
    localStorage.removeItem(key);
  } catch {
    // Storage can be unavailable; nothing else to migrate.
  }
}

function readPreference(label: string): NavPreference {
  const value = read(`${NAV_KEY}.${label}`);
  return value === "sidebar" || value === "tabs" ? value : "auto";
}

function readNumber(label: string, key: string): number | null {
  const value = Number(read(`${key}.${label}`));
  return Number.isFinite(value) && value > 0 ? value : null;
}

/** Drop the removed rail state and repair a persisted rail-sized width. */
function readStoredWidth(label: string): number | null {
  const raw = readNumber(label, WIDTH_KEY);
  if (raw !== null && raw <= LEGACY_RAIL_MAX) {
    remove(`${WIDTH_KEY}.${label}`);
  }
  return migrateSidebarWidth(raw);
}

/**
 * Per-window navigation state: 900px sidebar/tabs breakpoint, a sticky user
 * override and sidebar drag/clamp. There is no collapsed rail any more; keys
 * include the window label because every webview shares one localStorage origin.
 */
export function useWindowLayout(label: string) {
  const width = ref(viewportWidth(window.innerWidth));
  const preference = ref<NavPreference>(readPreference(label));
  remove(`${LEGACY_COLLAPSED_KEY}.${label}`);
  const storedWidth = ref<number | null>(readStoredWidth(label));

  const mode = computed(() => resolveNav(preference.value, width.value));
  const sidebarVisible = computed(() => mode.value === "sidebar");
  const resolvedWidth = computed(() => sidebarWidth(storedWidth.value, width.value));
  const maxWidth = computed(() => maxSidebarWidth(width.value));

  function persistPreference(): void {
    write(`${NAV_KEY}.${label}`, preference.value);
  }

  function persistWidth(): void {
    if (storedWidth.value !== null) write(`${WIDTH_KEY}.${label}`, String(storedWidth.value));
  }

  /** Manual switch; never returns to "auto" so resizing keeps the choice. */
  function toggle(): void {
    preference.value = sidebarVisible.value ? "tabs" : "sidebar";
    persistPreference();
  }

  function setWidth(next: number, persist = false): void {
    storedWidth.value = clampSidebarWidth(next, width.value);
    if (persist) persistWidth();
  }

  function stepWidth(direction: 1 | -1, large = false): void {
    setWidth(stepSidebarWidth(resolvedWidth.value, direction, large, width.value), true);
  }

  function onKeydown(event: KeyboardEvent): void {
    if (event.repeat) return;
    if (!(event.ctrlKey || event.metaKey) || !event.shiftKey || event.altKey) return;
    if (event.key.toLowerCase() !== "s") return;
    event.preventDefault();
    toggle();
  }

  function measure(): void {
    width.value = viewportWidth(window.innerWidth);
  }

  function onStorage(event: StorageEvent): void {
    if (event.key === `${NAV_KEY}.${label}`) preference.value = readPreference(label);
    else if (event.key === `${LEGACY_COLLAPSED_KEY}.${label}`) {
      remove(`${LEGACY_COLLAPSED_KEY}.${label}`);
    } else if (event.key === `${WIDTH_KEY}.${label}`) {
      const raw = readNumber(label, WIDTH_KEY);
      if (raw !== null && raw <= LEGACY_RAIL_MAX) remove(`${WIDTH_KEY}.${label}`);
      storedWidth.value = migrateSidebarWidth(raw);
    }
  }

  let observer: ResizeObserver | undefined;

  onMounted(() => {
    window.addEventListener("resize", measure);
    window.addEventListener("keydown", onKeydown);
    window.addEventListener("storage", onStorage);
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(measure);
      observer.observe(document.documentElement);
    }
  });

  onUnmounted(() => {
    window.removeEventListener("resize", measure);
    window.removeEventListener("keydown", onKeydown);
    window.removeEventListener("storage", onStorage);
    observer?.disconnect();
  });

  return {
    width,
    mode,
    preference,
    sidebarVisible,
    sidebarWidth: resolvedWidth,
    maxWidth,
    toggle,
    setWidth,
    stepWidth,
  };
}
