<script setup lang="ts">
import { marketHealth, channelStatus, marketTransport, marketChannels } from "../stores/market-health";
const state = marketHealth.state;
function time(value: number | null) {
  if (value === null || !Number.isFinite(new Date(value).getTime())) return "Unavailable";
  return new Date(value).toISOString();
}
</script>

<template>
  <section class="channel-health" aria-label="Market channel health" data-health="channels">
    <p>Transport · {{ marketTransport }} · Channels · {{ marketChannels }}</p>
    <template v-if="state.snapshot">
      <p>{{ state.snapshot.binding.symbol }} · {{ state.snapshot.binding.interval }} · {{ state.current ? 'Observed' : 'Historical' }}</p>
      <table>
        <thead><tr><th>Channel</th><th>Transport</th><th>Observation</th></tr></thead>
        <tbody><tr v-for="row in state.snapshot.rows" :key="row.channel" :data-health="row.channel">
          <th scope="row">{{ row.channel }}</th><td>{{ row.connected ? 'Connected' : 'Disconnected' }}</td>
          <td>{{ channelStatus(row) }}<br />{{ row.age_ms === null ? 'Age unknown' : `${row.age_ms} ms at sample` }} · {{ row.threshold_ms === null ? (row.channel === 'trades' || row.channel === 'candles' ? 'Event-driven' : 'Budget unavailable') : `${row.threshold_ms} ms budget` }}</td>
        </tr></tbody>
      </table>
      <details data-health="diagnostics">
        <summary>Diagnostics · {{ state.snapshot.diagnostics.length }} · Retained pool losses {{ state.snapshot.pool_last_losses.length }} · Omitted {{ state.snapshot.omitted_diagnostics }}</summary>
        <p>Native sample · {{ time(state.snapshot.observed_at_ms) }}{{ state.snapshot.clock_uncertain ? ' · Clock uncertain' : '' }}</p>
        <div data-health="pool-losses">
          <p v-for="(loss, index) in state.snapshot.pool_last_losses" :key="index">Retained pool loss · {{ loss.owner }} / {{ loss.connection_id ?? 'Unknown connection' }} · {{ loss.subscription_key ?? 'Connection-scoped' }} · {{ loss.channel ?? 'Transport' }} · {{ loss.kind }} · {{ time(loss.received_at_ms) }} · {{ loss.detail }}</p>
        </div>
        <div v-for="row in state.snapshot.rows" :key="row.channel">
          <p>{{ row.channel }} · {{ row.owner }} / {{ row.connection_id ?? 'Unknown connection' }} · Received {{ time(row.last_received_at_ms) }}</p>
          <p v-if="row.consumer_failure">Consumer failed · {{ row.consumer_failure }}</p>
          <p v-if="row.last_loss">Last loss · {{ row.last_loss.kind }} · {{ row.last_loss.subscription_key ?? 'Connection-scoped' }} · {{ time(row.last_loss.received_at_ms) }} · {{ row.last_loss.detail }}</p>
        </div>
        <p v-for="(diagnostic, index) in state.snapshot.diagnostics" :key="index">{{ diagnostic.owner }} / {{ diagnostic.connection_id ?? 'Unknown connection' }} · {{ diagnostic.subscription_key ?? 'Connection-scoped' }} · {{ diagnostic.channel ?? 'Transport' }} · {{ diagnostic.kind }} · {{ time(diagnostic.received_at_ms) }} · {{ diagnostic.detail }}</p>
      </details>
    </template>
    <p v-if="state.detail" role="status">{{ state.detail }}</p>
  </section>
</template>

<style scoped>
.channel-health { padding: var(--s-3); border-bottom: 1px solid var(--rule); font-size: var(--fs-body-sm); line-height: 1.4; overflow-wrap: anywhere; }
p { margin: 0 0 var(--s-2); }
table { width: 100%; table-layout: fixed; border-collapse: collapse; }
th, td { text-align: left; vertical-align: top; padding: 3px 2px; font-weight: 400; }
th:first-child { width: 8ch; } th:nth-child(2) { width: 14ch; }
th:nth-child(2), td:nth-child(2) { white-space: nowrap; overflow-wrap: normal; }
thead, summary { color: var(--bracket); }
details { margin-top: var(--s-2); } summary { cursor: pointer; }
</style>
