<script setup lang="ts">
import { computed, ref } from "vue";
const compact = ref(true);
const large = ref(false);
const frame = computed(() => ({ width: compact.value ? "1280px" : "1440px", height: compact.value ? "720px" : "900px" }));
const preview = ref<HTMLIFrameElement | null>(null);
const source = computed(() => `/qa/app.html?state=${new URLSearchParams(location.search).get("state") ?? "positions"}&large=${large.value}`);
function failRead(): void { preview.value?.contentWindow?.postMessage("fail-read", location.origin); }
</script>
<template>
  <aside class="qa-tools">
    <strong>UI FIXTURE · No venue or execution connection</strong>
    <label><input v-model="compact" type="checkbox" /> 1280 × 720 (off: 1440 × 900)</label>
    <label><input v-model="large" type="checkbox" /> Working text 150%</label>
    <button @click="failRead">Fail account refresh</button>
    <a href="/review.html?state=positions">Populated</a><a href="/review.html?state=empty">Empty</a><a href="/review.html?state=unavailable">Unavailable</a><a href="/review.html?state=loading">Initial read</a>
  </aside>
  <iframe ref="preview" title="Oppen fixture console" class="qa-frame" :style="frame" :src="source" />
</template>
<style>
body { overflow: auto; }
.qa-tools { display: flex; flex-wrap: wrap; gap: 16px; align-items: center; padding: 16px; color: #e7e9ea; font: 12px/1.5 sans-serif; }
.qa-tools button, .qa-tools a { border: 1px solid #7a8188; padding: 6px; }
.qa-frame { display: block; margin: 8px; border: 0; outline: 1px solid #7a8188; }
</style>
