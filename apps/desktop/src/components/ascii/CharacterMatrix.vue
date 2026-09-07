<script setup lang="ts">
import { computed } from "vue";
import { matrix, type MatrixMode } from "../../lib/ascii";
import { useClock } from "../../lib/clock";

const props = withDefaults(
  defineProps<{
    /** breathe = idle aperture; sweep = a scanning arm, for loading and waiting. */
    mode?: MatrixMode;
    static?: boolean;
    size?: "xs" | "sm" | "md" | "lg" | "xl";
    /** matrix = bracket-grey ink; field = one step darker, for backgrounds. */
    tone?: "matrix" | "field";
  }>(),
  { mode: "breathe", size: "sm", tone: "matrix" },
);

const tick = useClock();
const frame = computed(() => matrix(props.static ? 0 : tick.value, props.mode));
</script>

<template>
  <pre class="matrix" :class="[`matrix--${size}`, `matrix--${tone}`]" aria-hidden="true">{{ frame }}</pre>
</template>

<style scoped>
.matrix {
  font-family: var(--font-mono);
  font-weight: 400;
  line-height: 1.05;
  white-space: pre;
  color: var(--ink-matrix);
  user-select: none;
}

.matrix--field {
  color: var(--ink-field);
}

.matrix--xs {
  font-size: 6px;
}

.matrix--sm {
  font-size: 8px;
}

.matrix--md {
  font-size: 11px;
}

.matrix--lg {
  font-size: 18px;
}

.matrix--xl {
  font-size: 24px;
}
</style>
