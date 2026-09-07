<script setup lang="ts">
import { computed } from "vue";
import { gauge } from "../../lib/ascii";

const props = withDefaults(
  defineProps<{
    /** Fill fraction in [0, 1]. */
    value: number | null;
    cells?: number;
    label?: string;
  }>(),
  { cells: 14, label: "" },
);

const bar = computed(() => props.value === null ? " ".repeat(props.cells) : gauge(props.value, props.cells));
const width = computed(() => `${props.cells}ch`);
const description = computed(() => {
  if (props.value === null) return `${props.label} unknown`;
  const pct = Math.round(Math.max(0, props.value) * 100);
  return props.label ? `${props.label} ${pct}%` : `${pct}%`;
});
</script>

<template>
  <span class="gauge" role="img" :aria-label="description" :title="description">[<span :class="{ 'gauge--unknown': value === null }">{{ bar.replace(/ /g, ".") }}</span>]</span>
</template>

<style scoped>
.gauge {
  display: inline-block;
  min-width: v-bind(width);
  font-family: var(--font-mono);
  white-space: pre;
  letter-spacing: 0.04em;
  color: var(--bracket);
}
.gauge > span { color: var(--signal-dim); }
.gauge > .gauge--unknown { color: var(--rule-strong); }
</style>
