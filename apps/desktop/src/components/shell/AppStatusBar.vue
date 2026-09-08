<script setup lang="ts">
import AppUpdates from "./AppUpdates.vue";
import { computed, ref } from "vue";
import { feedLabel, shell, setView, accountObservationLabel } from "../../stores/shell";
import { marketTransport, marketChannels } from "../../stores/market-health";

import { events, operator } from "../../stores/operator";
import { eventText, isAgentDecision } from "../../lib/display";
const lastDecision = computed(() => [...events.value].reverse().find(event => isAgentDecision(event) && eventText(event.payload, "reason")));
const detailsOpen = ref(false);

const feeds = computed(() => ({
  rest: feedLabel(shell.feeds.rest),
}));
</script>

<template>
  <footer class="sb" data-tour="statusbar">
    <span class="sb__k">Last decision</span>
    <button v-if="lastDecision" class="sb__text" @click="setView('agents')" :title="`Agent-authored: ${eventText(lastDecision.payload, 'reason')}`">
      {{ lastDecision.agent_id }} · {{ eventText(lastDecision.payload, 'reason') }}
    </button>
    <span v-else>{{ operator.ledger ? 'None in this ledger window' : 'Not read' }}</span>

    <span class="sb__spacer" />

    <!--
      Spec item 34. A failed refresh is stated, not hidden: a blank panel and a
      stale one look identical to an operator and only one of them is safe.
    -->
    <button v-if="shell.accountError" class="sb__alert" :aria-expanded="detailsOpen" @click="detailsOpen = !detailsOpen">Account read failed · details</button>
    <div v-if="detailsOpen && shell.accountError" class="sb__details" role="status">
      <p>{{ shell.accountError }}</p><button @click="detailsOpen = false">Close details</button>
    </div>

    <span>
      Feeds ·
      <span class="sb__k" data-tour="feeds">WS {{ marketTransport }} · Channels {{ marketChannels }} · Account observation {{ accountObservationLabel() }} · REST {{ feeds.rest }}</span>
    </span>
    <AppUpdates />
    <span>Ledger · {{ operator.error || operator.ledgerError ? (operator.ledger ? 'last read' : 'unavailable') : operator.ledger ? `#${operator.ledger.head_seq}` : 'not read' }}</span>
  </footer>
</template>

<style scoped>
.sb__details { position: fixed; bottom: calc(var(--statusbar-h) + 8px); right: 8px; width: min(520px, 80vw); padding: var(--s-4); border: 1px solid var(--rule-strong); background: var(--plate); white-space: normal; text-transform: none; font-size: var(--fs-body); line-height: 1.6; z-index: 20; color: var(--signal); }
.sb__details button { margin-top: var(--s-3); text-decoration: underline; }

.sb {
  display: flex;
  flex: none;
  align-items: center;
  gap: var(--s-3);
  height: var(--statusbar-h);
  padding: 0 var(--s-4);
  border-top: 1px solid var(--rule);
  overflow: hidden;
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  white-space: nowrap;
  color: var(--bracket);
}

.sb__k {
  color: var(--body);
}

.sb__text {
  max-width: 35vw;
  overflow: hidden;
  text-overflow: ellipsis;
  text-transform: none;
  letter-spacing: 0.04em;
  color: var(--signal);
}

.sb__spacer {
  flex: 1;
}

.sb__alert {
  overflow: hidden;
  max-width: 46ch;
  text-overflow: ellipsis;
  color: var(--hazard, #ff4d2e);
}
</style>
