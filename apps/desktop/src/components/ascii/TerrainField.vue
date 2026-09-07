<script setup lang="ts">
import { computed, ref } from "vue";
import { reliefTerrainSample } from "../../lib/terrain";
import { motionStill, useClock } from "../../lib/clock";

// UI1: same terrain as the approved website, sampled into the app's native text grid.
const tick = useClock();
const grid = ref<HTMLElement | null>(null);
const pointer = ref<{ x: number; y: number; amp: number }>();
const tones = [" ", ".", "-", ":", "*", "+"];
const frame = computed(() => {
  const lines = [];
  for (let y = 0; y < 96; y++) {
    let row = "";
    for (let x = 0; x < 300; x++) {
      const value = reliefTerrainSample(x / 300, y / 96, tick.value, motionStill.value ? undefined : pointer.value);
      row += tones[Math.min(5, Math.floor(value * 6))];
    }
    lines.push(row);
  }
  return lines.join("\n");
});
function inspect(event: PointerEvent): void {
  if (motionStill.value) return;
  const box = grid.value?.getBoundingClientRect();
  if (!box) return;
  pointer.value = { x: (event.clientX - box.left) / box.width, y: (event.clientY - box.top) / box.height, amp: 1 };
}
</script>

<template>
  <div class="terrain" @pointermove="inspect" @pointerleave="pointer = undefined" aria-hidden="true">
    <pre ref="grid">{{ frame }}</pre>
  </div>
</template>

<style scoped>
.terrain { display: grid; place-items: center; width: 100%; height: 100%; overflow: hidden; container-type: inline-size; }
pre { font-family: var(--font-mono); font-size: 8px; line-height: 1.12; letter-spacing: 0; white-space: pre; color: var(--bracket); user-select: none; margin: 0; }
</style>
