/**
 * The one motion clock. Motion is resampling, not easing: every animated
 * character surface reads this tick and re-renders on it. Steps only.
 *
 * Reduced motion freezes the clock on frame 0.
 */

import { computed, onScopeDispose, ref, type Ref } from "vue";

export const TICK_MS = 90;

const tick = ref(0);
const reducedQuery = window.matchMedia("(prefers-reduced-motion: reduce)");

/** True while the OS asks for reduced motion. Surfaces may render a complete frame instead. */
export const motionReduced = ref(reducedQuery.matches);
function savedPause(): boolean {
  try { return localStorage.getItem("oppen.decorative-motion") === "paused"; } catch { return false; }
}
export const motionPaused = ref(savedPause());
export const motionStill = computed(() => motionReduced.value || motionPaused.value);
export function setMotionPaused(paused: boolean): void {
  motionPaused.value = paused;
  try { localStorage.setItem("oppen.decorative-motion", paused ? "paused" : "playing"); } catch { /* Session setting still applies. */ }
  stop();
  if (subscribers > 0) start();
}
document.addEventListener("visibilitychange", () => {
  stop();
  if (subscribers > 0) start();
});

let timer: ReturnType<typeof setInterval> | null = null;
let subscribers = 0;

function start(): void {
  if (timer !== null || motionStill.value || document.hidden) return;
  timer = setInterval(() => {
    tick.value += 1;
  }, TICK_MS);
}

function stop(): void {
  if (timer === null) return;
  clearInterval(timer);
  timer = null;
}

reducedQuery.addEventListener("change", (event) => {
  motionReduced.value = event.matches;
  if (event.matches) {
    stop();
    tick.value = 0;
  } else if (subscribers > 0) {
    start();
  }
});

/** Subscribe the calling component to the shared 90ms tick. */
export function useClock(): Readonly<Ref<number>> {
  subscribers += 1;
  start();
  onScopeDispose(() => {
    subscribers -= 1;
    if (subscribers === 0) stop();
  });
  return tick;
}
