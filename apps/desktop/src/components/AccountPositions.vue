<script setup lang="ts">
import { shell } from "../stores/shell";
import { decimal } from "../lib/display";
import EmptyState from "./housing/EmptyState.vue";
</script>

<template>
  <div class="positions">
    <table v-if="shell.account?.positions.length">
      <caption>Positions · configured account · {{ shell.account.address }}</caption>
      <thead><tr><th>Market</th><th>Side</th><th>Size</th><th>Entry · USD</th><th>Liq · USD</th><th>PnL · USD</th><th>Margin · USD</th></tr></thead>
      <tbody><tr v-for="p in shell.account.positions" :key="p.symbol">
        <th scope="row">{{ p.symbol }}</th><td>{{ p.size.startsWith('-') ? 'SHORT' : 'LONG' }}</td>
        <td>{{ p.size }}</td><td>{{ p.entry_px ?? '—' }}</td><td>{{ p.liquidation_px ?? '—' }}</td>
        <td :title="p.unrealized_pnl_usd">{{ decimal(p.unrealized_pnl_usd) }}</td><td :title="p.margin_used_usd">{{ decimal(p.margin_used_usd) }}</td>
      </tr></tbody>
    </table>
    <EmptyState v-else :line="shell.account ? 'No open positions in this account.' : 'Positions are unknown until the account is read.'" />
  </div>
</template>

<style scoped>
.positions { overflow: auto; min-height: 0; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-body); white-space: nowrap; }
caption { padding: var(--s-2); text-align: left; color: var(--bracket); font-size: var(--fs-body-sm); }
th, td { padding: var(--s-2); border-bottom: 1px solid var(--rule); text-align: right; font-weight: 400; }
th:first-child { text-align: left; }
thead { color: var(--bracket); }
td { color: var(--signal); }
</style>
