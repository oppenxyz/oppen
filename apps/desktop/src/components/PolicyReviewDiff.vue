<script setup lang="ts">
import { computed } from "vue";
import { policyDiff, policyFieldLabel } from "../lib/policy-review";

const props = defineProps<{ before: unknown; proposed: unknown }>();
const rows = computed(() => policyDiff(props.before, props.proposed));
</script>

<template>
  <div class="policy-diff" role="table" aria-label="Complete policy review">
    <div class="policy-diff__row policy-diff__heading" role="row">
      <span role="columnheader">Field</span><span role="columnheader">Before</span><span role="columnheader">Proposed</span>
    </div>
    <div v-for="row in rows" :key="row.key" class="policy-diff__row" role="row">
      <span role="rowheader">{{ policyFieldLabel(row.path) }}<small>{{ row.changed ? 'Changed' : 'Retained' }}</small></span>
      <span role="cell">{{ row.before }}</span><span role="cell">{{ row.proposed }}</span>
    </div>
  </div>
</template>

<style scoped>
.policy-diff { min-width: 0; margin-block: var(--s-4); }
.policy-diff__row { display: grid; grid-template-columns: minmax(0, 2fr) repeat(2, minmax(0, 1fr)); gap: var(--s-3); padding-block: var(--s-3); border-bottom: 1px solid var(--rule); font-size: var(--fs-body); line-height: 1.6; }
.policy-diff__row > span { min-width: 0; overflow-wrap: anywhere; white-space: pre-wrap; }
.policy-diff__heading { color: var(--signal); }
small { display: block; color: var(--body-dim); font-size: var(--fs-label); }
</style>
