import { onUnmounted, ref, type Ref } from "vue";

/** Reactive `matchMedia` for layout decisions that CSS alone cannot drive. */
export function useMediaQuery(query: string): Ref<boolean> {
  const media = window.matchMedia(query);
  const matches = ref(media.matches);
  const listener = (event: MediaQueryListEvent): void => {
    matches.value = event.matches;
  };
  media.addEventListener("change", listener);
  onUnmounted(() => media.removeEventListener("change", listener));
  return matches;
}
