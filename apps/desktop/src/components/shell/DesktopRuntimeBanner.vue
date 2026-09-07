<script setup lang="ts">
import { computed } from "vue";
import { runtime, runtimeNotice } from "../../stores/runtime";

const notice = computed(() => runtimeNotice(runtime));
const checkedAt = computed(() => runtime.checkedAt === null ? null : new Date(runtime.checkedAt).toLocaleTimeString());
</script>

<template>
  <section v-if="notice" class="runtime-banner" aria-label="Desktop tasks" role="status" aria-live="polite">
    <div class="runtime-banner__summary">
      <strong>Desktop tasks</strong>
      <span>{{ notice.title }}</span>
      <span v-if="runtime.error && checkedAt" class="runtime-banner__error">Last observed {{ checkedAt }}</span>
    </div>
    <p v-if="notice.detail">{{ notice.detail }}</p>
    <p v-if="notice.error" class="runtime-banner__error">Status read unavailable: {{ notice.error }}</p>
  </section>
</template>

<style scoped>
.runtime-banner {
  box-sizing: border-box;
  flex: none;
  width: min(100%, 100vw);
  max-width: 100vw;
  min-width: 0;
  padding: var(--s-3) var(--s-4);
  border-bottom: 1px solid var(--rule-strong);
  border-left: 2px solid var(--hazard);
  background: var(--plate);
  color: var(--body);
  font: 400 var(--fs-copy-sm) / 1.5 var(--font-sans);
  letter-spacing: 0;
  overflow-wrap: anywhere;
}
.runtime-banner__summary {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--s-1) var(--s-3);
}
.runtime-banner strong {
  color: var(--signal);
  font: 700 var(--fs-copy-sm) / 1.5 var(--font-mono);
}
.runtime-banner p { margin: var(--s-1) 0 0; }
.runtime-banner__error { color: var(--hazard); }
</style>
