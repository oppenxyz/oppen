<script setup lang="ts">
import type { CancelTarget } from "../lib/bridge";
defineProps<{ targets: readonly CancelTarget[] }>();
</script>

<template>
  <div class="cancel-targets" role="list" aria-label="Exact cancellation targets">
    <div v-for="target in targets" :key="`${target.asset_index}:${target.oid}`" class="cancel-targets__row" role="listitem">
      <h4>{{ target.symbol }} · {{ target.is_buy ? 'Buy' : 'Sell' }} · {{ target.order_type }}</h4>
      <p>{{ target.sz }} @ {{ target.limit_px }} USD · Original size {{ target.orig_sz }}</p>
      <p>OID {{ target.oid }} · Client ID {{ target.cloid ?? 'None' }}</p>
      <p :class="{ 'cancel-targets__protective': target.reduce_only || target.is_trigger || target.is_position_tpsl }">Reduce only: {{ target.reduce_only ? 'Yes' : 'No' }} · Trigger: {{ target.is_trigger ? 'Yes' : 'No' }} · Position TP/SL: {{ target.is_position_tpsl ? 'Yes' : 'No' }}</p>
      <p v-if="target.trigger_px !== null || target.trigger_condition !== null">Trigger {{ target.trigger_px ?? 'Unknown price' }} USD · {{ target.trigger_condition ?? 'Unknown condition' }}</p>
      <p>Asset {{ target.asset_index }} · {{ new Date(target.timestamp).toLocaleString() }}</p>
    </div>
  </div>
</template>

<style scoped>
.cancel-targets { min-width: 0; }
.cancel-targets__row { padding: var(--s-2) 0; border-top: 1px solid var(--rule); }
.cancel-targets h4, .cancel-targets p { font: inherit; margin: 0; overflow-wrap: anywhere; white-space: normal; }
.cancel-targets h4 { color: var(--signal); }
.cancel-targets__protective { color: var(--hazard); }
</style>
