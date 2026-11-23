import { computed, ref } from "vue";

const STORAGE = "printkit.motion.reduce";

function readReduce(): boolean {
  try {
    return localStorage.getItem(STORAGE) === "1";
  } catch {
    return false;
  }
}

const appReduce = ref(readReduce());
const media = typeof window !== "undefined" ? window.matchMedia("(prefers-reduced-motion: reduce)") : null;
const systemReduce = ref(Boolean(media?.matches));
const reduce = computed(() => appReduce.value || systemReduce.value);

/**
 * Motion is token-driven: `data-motion="reduce"` zeroes the duration tokens
 * and switches off the named pk animations. Loading spinners keep spinning
 * because they are driven by the component's own animation, not by our
 * global `* { animation-duration: .001ms !important }` rule.
 */
function apply(): void {
  if (typeof document === "undefined") return;
  document.documentElement.dataset.motion = reduce.value ? "reduce" : "full";
}

export function bootMotion(): void {
  apply();
  media?.addEventListener("change", (event) => {
    systemReduce.value = event.matches;
    apply();
  });
  if (typeof window !== "undefined") window.addEventListener("storage", (event) => {
    if (event.key !== STORAGE) return;
    appReduce.value = event.newValue === "1";
    apply();
  });
}

export function useMotion() {
  function setReduce(next: boolean): void {
    appReduce.value = next;
    try {
      localStorage.setItem(STORAGE, next ? "1" : "0");
    } catch {
      // The in-memory setting still applies.
    }
    apply();
  }
  return { reduce, systemReduce, appReduce, setReduce };
}
