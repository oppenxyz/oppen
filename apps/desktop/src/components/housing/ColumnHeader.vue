<script setup lang="ts">
import { computed } from "vue";

const props = withDefaults(
  defineProps<{
    columns: readonly string[];
    /** A CSS grid-template-columns track list. */
    template: string;
    /** Drops the horizontal padding inside inset housings. */
    flush?: boolean;
  }>(),
  { flush: false },
);

const template = computed(() => props.template);
</script>

<template>
  <div class="cols" :class="{ 'cols--flush': flush }">
    <span v-for="column in columns" :key="column">{{ column }}</span>
  </div>
</template>

<style scoped>
.cols {
  display: grid;
  flex: none;
  grid-template-columns: v-bind(template);
  gap: var(--s-2);
  padding: var(--s-2) var(--s-3);
  border-bottom: 1px solid var(--rule);
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  color: var(--bracket);
}

.cols--flush {
  padding-right: 0;
  padding-left: 0;
}
</style>
