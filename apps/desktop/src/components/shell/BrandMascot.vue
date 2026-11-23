<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";

const root = ref<HTMLElement | null>(null);
let timer: number | undefined;
let frame: number | undefined;

function motionReduced(): boolean {
  if (typeof document !== "undefined" && document.documentElement.dataset.motion === "reduce") return true;
  return typeof window !== "undefined" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** One 0.7s ears/tail twitch on first paint and on hover; no animation when motion is reduced. */
function play(): void {
  if (motionReduced()) return;
  const element = root.value;
  if (!element) return;
  element.classList.remove("is-playing");
  void element.offsetWidth;
  element.classList.add("is-playing");
  if (timer !== undefined) clearTimeout(timer);
  timer = window.setTimeout(() => {
    element.classList.remove("is-playing");
    timer = undefined;
  }, 750);
}

onMounted(() => {
  frame = window.requestAnimationFrame(play);
});

onUnmounted(() => {
  if (timer !== undefined) window.clearTimeout(timer);
  if (frame !== undefined) window.cancelAnimationFrame(frame);
});
</script>

<template>
  <span ref="root" class="mascot" aria-hidden="true" @mouseenter="play">
    <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
      <g class="mascot-ears">
        <path class="mascot-fur" d="M3.8 10.2 5.6 2.5 10.8 7Z" />
        <path class="mascot-soft" d="m5.9 8.3.7-3.8 2.8 2.7Z" />
        <path class="mascot-fur" d="M20.2 10.2 18.4 2.5 13.2 7Z" />
        <path class="mascot-soft" d="m18.1 8.3-.7-3.8-2.8 2.7Z" />
      </g>
      <path class="mascot-fur" d="M2.8 10.3c0-2.4 2-4 4.8-4h6.5c2.8 0 4.8 1.6 4.8 4v3.2c0 4.8-3.2 7.7-8.9 7.7S2.8 18.3 2.8 13.5Z" />
      <path class="mascot-soft" d="M8 11.2 16 11.2 12 17.8Z" />
      <circle class="mascot-ink" cx="8.4" cy="10.6" r="1" />
      <circle class="mascot-ink" cx="15.5" cy="10.6" r="1" />
      <circle class="mascot-ink" cx="12" cy="15.8" r="1.1" />
      <g class="mascot-tail">
        <path class="mascot-fur" d="M18.6 17.6C21.5 17.4 23.8 14.6 22.6 9.8 21.9 13.2 20.1 14.3 18.6 15.4Z" />
        <path class="mascot-soft" d="M22.6 9.8C23.2 11.2 23.1 12.6 22.2 13.5 22.4 12 22.1 11 21.5 10.2Z" />
      </g>
    </svg>
  </span>
</template>

<style scoped>
.mascot {
  display: inline-flex;
  width: 100%;
  height: 100%;
  align-items: center;
  justify-content: center;
  color: var(--pk-accent);
}

.mascot svg {
  display: block;
  width: 100%;
  height: 100%;
  overflow: visible;
}

.mascot-fur {
  fill: var(--pk-accent);
}

.mascot-soft {
  fill: var(--pk-accent-soft);
}

.mascot-ink {
  fill: var(--pk-text);
}

.mascot-ears,
.mascot-tail {
  transform-box: fill-box;
}

.mascot-ears {
  transform-origin: 50% 100%;
}

.mascot-tail {
  transform-origin: 10% 80%;
}

.mascot.is-playing .mascot-ears {
  animation: mascot-ears 0.7s var(--pk-ease-control);
}

.mascot.is-playing .mascot-tail {
  animation: mascot-tail 0.7s var(--pk-ease-control);
}

@keyframes mascot-ears {
  0%,
  100% {
    transform: rotate(0deg);
  }
  40% {
    transform: rotate(-4deg);
  }
  70% {
    transform: rotate(2deg);
  }
}

@keyframes mascot-tail {
  0%,
  100% {
    transform: rotate(0deg);
  }
  40% {
    transform: rotate(-7deg);
  }
  70% {
    transform: rotate(4deg);
  }
}

@media (prefers-reduced-motion: reduce) {
  .mascot.is-playing .mascot-ears,
  .mascot.is-playing .mascot-tail {
    animation: none;
  }
}

:global(html[data-motion="reduce"]) .mascot.is-playing .mascot-ears,
:global(html[data-motion="reduce"]) .mascot.is-playing .mascot-tail {
  animation: none;
}
</style>
