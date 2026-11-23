import { computed, onMounted, onUnmounted, ref } from "vue";

import { visibleRecordColumns } from "../components/businessColumns.js";

function readWidth(): number {
  const width = document.documentElement.clientWidth || window.innerWidth;
  return width > 0 ? width : 900;
}

/** Reactive viewport width; the business pages hide columns with v-if. */
export function useBreakpoint() {
  const width = ref(readWidth());
  const columns = computed(() => visibleRecordColumns(width.value));
  const narrow = computed(() => columns.value.narrow);
  const tiny = computed(() => columns.value.tiny);

  function measure(): void {
    width.value = readWidth();
  }

  let observer: ResizeObserver | undefined;

  onMounted(() => {
    window.addEventListener("resize", measure);
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(measure);
      observer.observe(document.documentElement);
    }
  });

  onUnmounted(() => {
    window.removeEventListener("resize", measure);
    observer?.disconnect();
  });

  return { width, columns, narrow, tiny };
}
