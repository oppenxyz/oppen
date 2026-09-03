<script setup lang="ts">
import ColumnHeader from "../components/housing/ColumnHeader.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows, { type ReadoutRow } from "../components/housing/ReadoutRows.vue";
import UiButton from "../components/ui/UiButton.vue";
import { setNetwork, shell, type Network } from "../stores/shell";

const SECTIONS = [
  "Permissions & limits",
  "Keys & venues",
  "Models & API keys",
  "Local data",
  "MCP server",
  "Display",
] as const;

const NETWORKS: readonly Network[] = ["testnet", "mainnet"];

const GLOBAL_LIMITS: readonly ReadoutRow[] = [
  { k: "total_notional_cap", v: "—" },
  { k: "total_daily_loss_halt", v: "—" },
  { k: "margin_halt", v: "—" },
  { k: "max_agents_running", v: "—" },
  { k: "venue_allowlist", v: "HYPERLIQUID", tone: "signal" },
  { k: "human_approval", v: "queue — default for new agents", tone: "signal" },
];

const LOCAL: readonly ReadoutRow[] = [
  { k: "runtime", v: "—" },
  { k: "keys", v: "OS keychain · never on disk" },
  { k: "decision_log", v: "—" },
  { k: "network", v: "" },
  { k: "telemetry", v: "none" },
];
</script>

<template>
  <div class="settings">
    <PanelHousing>
      <ul class="snav">
        <li v-for="(section, index) in SECTIONS" :key="section" class="snav__item" :class="{ 'snav__item--active': index === 0 }">
          {{ section }}
        </li>
      </ul>
    </PanelHousing>

    <div class="settings__grid">
      <PanelHousing inset label="Global limits — apply above every agent policy" :brackets="['tl']">
        <ReadoutRows size="md" :rows="GLOBAL_LIMITS" />
      </PanelHousing>

      <PanelHousing inset label="Per-agent caps">
        <ColumnHeader :columns="['Agent', 'Notional', 'Max lev', 'Loss halt']" template="1.4fr 1fr 1fr 1fr" flush />
        <EmptyState matrix line="No agents paired." />
      </PanelHousing>

      <PanelHousing inset label="Kill switch">
        <p class="copy copy--sm">
          Halts every agent, cancels resting orders, and locks the runtime until you unlock it locally. Positions are
          not closed. The halt survives restart, and a venue-side dead-man cancel is armed while any agent runs.
        </p>
        <div class="actions">
          <UiButton variant="hazard" disabled title="Kill switch arrives with the guardrail phase">Halt all</UiButton>
        </div>
      </PanelHousing>

      <PanelHousing inset label="Local">
        <ReadoutRows :rows="LOCAL">
          <template #value="{ row }">
            <span v-if="row.k === 'network'" class="netswitch" role="group" aria-label="Network">
              <button
                v-for="network in NETWORKS"
                :key="network"
                type="button"
                class="netswitch__opt"
                :class="{ [`netswitch__opt--${network}`]: shell.network === network }"
                :aria-pressed="shell.network === network"
                @click="setNetwork(network)"
              >
                {{ network }}
              </button>
            </span>
            <template v-else>{{ row.v }}</template>
          </template>
        </ReadoutRows>
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.settings {
  display: grid;
  flex: 1;
  grid-template-columns: 220px minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.settings__grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  align-content: start;
  gap: var(--panel-gap);
  min-height: 0;
  overflow: auto;
}

.snav__item {
  padding: var(--s-3);
  border-bottom: 1px solid var(--rule);
  border-left: 2px solid transparent;
  font-size: var(--fs-body-sm);
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  color: var(--bracket);
}

.snav__item:last-child {
  border-bottom: 0;
}

.snav__item--active {
  border-left-color: var(--signal);
  color: var(--signal);
}

.actions {
  display: flex;
  gap: var(--s-2);
  margin-top: var(--s-3);
}

.netswitch {
  display: inline-flex;
  border: 1px solid var(--rule-strong);
}

.netswitch__opt {
  padding: 2px var(--s-2);
  font-size: var(--fs-label);
  line-height: 1.6;
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  color: var(--bracket);
}

.netswitch__opt + .netswitch__opt {
  border-left: 1px solid var(--rule-strong);
}

.netswitch__opt:hover {
  color: var(--body);
}

.netswitch__opt--testnet,
.netswitch__opt--testnet:hover {
  color: var(--uranium);
}

.netswitch__opt--mainnet,
.netswitch__opt--mainnet:hover {
  color: var(--signal);
}
</style>
