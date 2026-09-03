<script setup lang="ts">
import { computed } from "vue";
import { boot } from "../../lib/ascii";
import { motionReduced, TICK_MS, useClock } from "../../lib/clock";
import { shell } from "../../stores/shell";

const CHARS_PER_SECOND = 24;

const props = defineProps<{
  /** Overrides the v1 boot lines. */
  lines?: readonly string[];
}>();

function row(key: string, value: string): string {
  return `> ${key.padEnd(18)}${value.padStart(15)}`;
}

const defaultLines = computed<readonly string[]>(() => [
  row("oppen.init", "ok"),
  row("keys.keychain", "ok"),
  row("venue.hyperliquid", "connect"),
  row("subaccounts", "mapped"),
  row("mcp.server", "127.0.0.1  live"),
  row("guardrails", "loaded"),
  row("deadman.schedule", "armed"),
  row("net", shell.network.toUpperCase()),
  "> ready.",
]);

const tick = useClock();
const lines = computed(() => props.lines ?? defaultLines.value);
const chars = computed(() =>
  motionReduced.value ? Number.POSITIVE_INFINITY : Math.floor((tick.value * TICK_MS * CHARS_PER_SECOND) / 1000),
);
const frame = computed(() => boot(lines.value, chars.value));
</script>

<template>
  <pre class="boot">{{ frame }}</pre>
</template>

<style scoped>
.boot {
  font-family: var(--font-mono);
  font-size: var(--fs-body);
  line-height: 1.9;
  white-space: pre;
  color: var(--body);
}
</style>
