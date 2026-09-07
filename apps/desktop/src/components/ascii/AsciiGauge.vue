<script setup lang="ts">
import { computed, onMounted, onBeforeUnmount, ref } from "vue";
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

const host = ref<HTMLElement | null>(null);
const probe = ref<HTMLElement | null>(null);
const availableCells = ref(props.cells);
const cells = computed(() => Math.min(props.cells, availableCells.value));
const bar = computed(() => props.value === null ? " ".repeat(cells.value) : gauge(props.value, cells.value));
const width = computed(() => `calc(${props.cells + 2} * (1ch + 0.04em))`);
let observer: ResizeObserver | undefined;
onMounted(() => {
  const measure = () => {
    const glyphWidth = probe.value?.getBoundingClientRect().width ?? 0;
    if (host.value && glyphWidth > 0) {
      availableCells.value = Math.max(1, Math.floor(host.value.getBoundingClientRect().width / glyphWidth) - 2);
    }
  };
  observer = new ResizeObserver(measure);
  if (host.value) observer.observe(host.value);
  if (probe.value) observer.observe(probe.value);
  measure();
});
onBeforeUnmount(() => observer?.disconnect());
const description = computed(() => {
  if (props.value === null) return `${props.label} unknown`;
  const pct = Math.round(Math.max(0, props.value) * 100);
  return props.label ? `${props.label} ${pct}%` : `${pct}%`;
});
</script>

<template>
  <span ref="host" class="gauge" role="img" :aria-label="description" :title="description">[<span :class="{ 'gauge--unknown': value === null }">{{ bar.replace(/ /g, ".") }}</span>]<i ref="probe" class="gauge__probe" aria-hidden="true">0</i></span>
</template>

<style scoped>
.gauge {
  display: inline-block;
  position: relative;
  width: v-bind(width);
  max-width: 100%;
  font-family: var(--font-mono);
  white-space: pre;
  letter-spacing: 0.04em;
  color: var(--bracket);
}
.gauge__probe { position: absolute; visibility: hidden; font-style: normal; }
.gauge > span { color: var(--signal-dim); }
.gauge > .gauge--unknown { color: var(--rule-strong); }
</style>
