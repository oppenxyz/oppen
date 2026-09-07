<script setup lang="ts">
import { eventText, isAgentDecision } from "../lib/display";
import { computed } from "vue";
import { events, operator, refreshOperator } from "../stores/operator";
import { setView } from "../stores/shell";
import EmptyState from "./housing/EmptyState.vue";

const props = defineProps<{ agent?: string; fillsOnly?: boolean }>();
const rows = computed(() => events.value.filter(event => (!props.agent || event.agent_id === props.agent) && (!props.fillsOnly || event.kind === "fill")).slice().reverse());
const unavailable = computed(() => operator.error ?? operator.ledgerError);
const loaded = computed(() => operator.ledger !== null);
</script>

<template>
  <div class="stream">
    <div class="stream__status">
      <span>{{ operator.ledgerReadMs ? `Read ${new Date(operator.ledgerReadMs).toLocaleTimeString()}` : 'Ledger not read' }} · latest 200 event rows</span>
      <button @click="refreshOperator" :disabled="operator.reading">Refresh</button>
    </div>
    <p v-if="unavailable" class="stream__error" role="status">{{ unavailable }}{{ loaded ? ' Showing the last successful reading.' : '' }}</p>
    <p v-if="operator.ledger?.resync_required" class="stream__error">The ledger requires a history resync. This page is incomplete.</p>
    <article v-for="event in rows" :key="event.seq" class="event">
      <header><time>{{ new Date(event.ts_ms).toLocaleString() }}</time><strong>{{ event.kind.split('_').join(' ') }}</strong><span>{{ event.agent_id ?? 'System / operator' }} · #{{ event.seq }}</span></header>
      <p v-if="eventText(event.payload, 'reason') !== null"><span class="event__label">{{ isAgentDecision(event) ? 'Agent-authored reason' : 'Recorded reason' }}</span>{{ eventText(event.payload, 'reason') }}</p>
      <p v-if="eventText(event.payload, 'refusal')"><span class="event__label">Recorded refusal</span>{{ eventText(event.payload, 'refusal') }}</p>
      <p v-if="event.payload === null">Payload redacted; chain metadata retained.</p>
      <details v-else><summary>Recorded event fields</summary><pre>{{ JSON.stringify(event.payload, null, 2) }}</pre></details>
      <button v-if="event.kind === 'refusal' || event.kind === 'guardrail_trip'" @click="setView('settings')">Inspect stored limits</button>
    </article>
    <EmptyState v-if="!rows.length" :reading="operator.reading && !loaded" :line="loaded ? (fillsOnly ? 'No fills in the latest 200 ledger events.' : 'No matching events in this ledger window.') : 'Decision history is unknown until the gateway ledger is connected.'" />
  </div>
</template>

<style scoped>
.stream { min-height: 0; overflow: auto; font-size: var(--fs-body); }
.stream__status { display: flex; justify-content: space-between; gap: var(--s-2); padding: var(--s-2) var(--s-3); color: var(--bracket); font-size: var(--fs-body-sm); }
.stream__error { padding: var(--s-3); color: var(--body); overflow-wrap: anywhere; }
.event { padding: var(--s-3); border-top: 1px solid var(--rule); }
.event header { display: flex; flex-wrap: wrap; gap: var(--s-3); color: var(--bracket); font-size: var(--fs-body-sm); }
.event strong { color: var(--signal); font-weight: 400; text-transform: uppercase; }
.event p { margin-block: var(--s-3); line-height: 1.6; white-space: pre-wrap; overflow-wrap: anywhere; }
.event__label { display: block; color: var(--bracket); font-size: var(--fs-body-sm); }
.event pre { white-space: pre-wrap; overflow-wrap: anywhere; margin-block: var(--s-3); color: var(--body); }
.event button { margin-top: var(--s-2); text-decoration: underline; color: var(--signal); }
summary { color: var(--body); cursor: pointer; }
</style>
