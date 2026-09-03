<script setup lang="ts">
import { computed } from "vue";
import { gauge } from "../../lib/ascii";

const props = withDefaults(
  defineProps<{
    /** Fill fraction in [0, 1]. */
    value: number;
    cells?: number;
    label?: string;
  }>(),
  { cells: 14, label: "" },
);

const bar = computed(() => gauge(props.value, props.cells));
const width = computed(() => `${props.cells}ch`);
const description = computed(() => {
  const pct = Math.round(Math.max(0, Math.min(1, props.value)) * 100);
  return props.label ? `${props.label} ${pct}%` : `${pct}%`;
});
</script>

<template>
  <span class="gauge" role="img" :aria-label="description">{{ bar }}</span>
</template>

<style scoped>
.gauge {
  display: inline-block;
  min-width: v-bind(width);
  font-family: var(--font-mono);
  white-space: pre;
  letter-spacing: 0.04em;
  color: var(--body-dim);
}
</style>
