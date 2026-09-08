<script setup lang="ts">
import { computed, ref, watch } from "vue";
import DecisionStream from "../components/DecisionStream.vue";
import ApprovalQueuePanel from "../components/ApprovalQueuePanel.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows from "../components/housing/ReadoutRows.vue";
import UiButton from "../components/ui/UiButton.vue";
import { decimal } from "../lib/display";
import { events, operator, recordedAgents, storedPolicy, policySourceLabel, refreshOperator } from "../stores/operator";
import { setView } from "../stores/shell";
import { openPolicySettings } from "../stores/settings";

const selected = ref("");
watch(recordedAgents, agents => { if (!agents.includes(selected.value)) selected.value = agents[0] ?? ""; }, { immediate: true });
const policy = computed(() => storedPolicy.value?.guardrails[selected.value]);
const lastSeen = computed(() => [...events.value].reverse().find(event => event.agent_id === selected.value)?.ts_ms);
const halt = computed(() => storedPolicy.value?.kill.global ?? storedPolicy.value?.kill.agents[selected.value]);
const policyRows = computed(() => policy.value ? [
  { k: "Allowed markets", v: policy.value.symbols.join(", ") || "None · orders refused" },
  { k: "Order cap · USD", v: decimal(policy.value.max_order_usd) },
  { k: "Position cap · USD", v: decimal(policy.value.max_position_usd) },
  { k: "Slippage cap · bp", v: decimal(policy.value.max_slippage_bps) },
  { k: "Leverage cap", v: `${policy.value.risk.max_leverage}×` },
  { k: "Daily loss halt · USD", v: policy.value.loss.max_daily_loss_usd === null ? "Unset" : decimal(policy.value.loss.max_daily_loss_usd) },
  { k: "Approval required", v: policy.value.approval_required ? "Yes" : "No" },
  { k: "Reduce only", v: policy.value.reduce_only ? "Yes" : "No" },
] : []);
</script>

<template>
  <div class="agents">
    <PanelHousing label="Recorded agents" :meta="operator.policy || operator.ledger ? `${recordedAgents.length}` : 'Not read'" data-tour="agents">
      <div class="roster" v-if="recordedAgents.length">
        <button v-for="agent in recordedAgents" :key="agent" :aria-pressed="selected === agent" @click="selected = agent">
          <strong>{{ agent }}</strong><span>Container route unavailable</span>
        </button>
      </div>
      <EmptyState v-else matrix size="sm" :reading="operator.reading && !operator.policy && !operator.ledger" :line="operator.error ?? operator.policyError ?? operator.ledgerError ?? (operator.policy && operator.ledger ? 'No agents in stored policies or recent events.' : 'No agent records have been read.')" action="Open MCP setup" @action="setView('builder')" />
      <template #footer>From stored policies and recent events. A record does not prove an active pairing.</template>
    </PanelHousing>
    <div class="agents__main">
      <PanelHousing inset :brackets="['tl']" :label="selected || 'Gateway data'">
        <div class="agent-summary">
          <span>Last recorded event · {{ lastSeen ? new Date(lastSeen).toLocaleString() : 'Unknown' }}</span>
          <span>Stored halt · {{ storedPolicy ? (halt ? 'Engaged' : 'Not engaged') : 'Unknown' }}</span>
          <UiButton size="sm" @click="refreshOperator" :disabled="operator.reading">Refresh records</UiButton>
        </div>
        <p v-if="operator.error || operator.policyError" class="agents__note" role="status">{{ operator.error ?? operator.policyError }} Last successful readings remain visible.</p>
      </PanelHousing>
      <div class="agents__lower">
        <PanelHousing label="Decision log" meta="Recorded events">
          <DecisionStream :agent="selected || undefined" />
        </PanelHousing>
        <div class="agents__side">
          <PanelHousing inset label="Stored policy">
            <p class="agents__note" role="status">{{ policySourceLabel }}<span v-if="operator.policy?.revision != null"> · Revision {{ operator.policy.revision }}</span></p>
            <ReadoutRows v-if="policy" :rows="policyRows" />
            <EmptyState v-else line="No policy read for this agent." />
            <p class="agents__note">{{ operator.policyReadMs ? `Read ${new Date(operator.policyReadMs).toLocaleTimeString()}` : 'Not read' }} · Paused policy setup requires existing TESTNET authority and idle MCP.</p>
            <UiButton size="sm" @click="openPolicySettings">Set up paused policy</UiButton>
          </PanelHousing>
          <ApprovalQueuePanel />
          <PanelHousing inset label="Runtime state">
            <ReadoutRows :rows="[{ k: 'Active pairing', v: 'Not read' }, { k: 'Dead-man coverage', v: 'Not read' }, { k: 'Cancel completion', v: 'Not read' }]" />
          </PanelHousing>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.roster { overflow: auto; min-height: 0; }
.roster button { display: grid; gap: var(--s-2); width: 100%; padding: var(--s-4); text-align: left; border-bottom: 1px solid var(--rule); border-left: 2px solid transparent; }
.roster button[aria-pressed="true"] { border-left-color: var(--signal); background: var(--void); }
.roster strong { color: var(--signal); font-weight: 400; }
.roster span { color: var(--bracket); font-size: var(--fs-body-sm); overflow-wrap: anywhere; }
.agent-summary { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: var(--s-3); font-size: var(--fs-body); }

.agents {
  display: grid;
  flex: 1;
  grid-template-columns: 380px minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.agents__main {
  display: grid;
  grid-template-rows: auto 1fr;
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
}

.agents__lower {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 320px;
  gap: var(--panel-gap);
  min-height: 0;
}

.agents__side {
  display: grid;
  grid-template-rows: max-content max-content max-content;
  align-content: start;
  overflow: auto;
  gap: var(--panel-gap);
  min-height: 0;
}

.agents__note {
  margin-top: var(--s-2);
  font-size: var(--fs-label);
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--bracket);
}
</style>
