<script setup lang="ts">
import { computed, ref } from "vue";
import ColumnHeader from "../components/housing/ColumnHeader.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows, { type ReadoutRow } from "../components/housing/ReadoutRows.vue";
import StatBlock from "../components/housing/StatBlock.vue";

type Ledger = "positions" | "orders" | "fills";

const LEDGER_TABS: ReadonlyArray<{ key: Ledger; label: string; empty: string }> = [
  { key: "positions", label: "Positions · 0", empty: "No open positions." },
  { key: "orders", label: "Open orders · 0", empty: "No open orders." },
  { key: "fills", label: "Fills", empty: "No fills." },
];

const TIMEFRAMES = ["1M", "5M", "15M", "1H", "4H", "1D"] as const;

const POSITION_COLUMNS = ["Market", "Side", "Size", "Entry", "Mark", "Liq", "PnL", "Agent · sub-acct"] as const;
const POSITION_TEMPLATE = "1.2fr 0.8fr 1fr 1fr 1fr 1fr 1fr 1.2fr";

const FEATURES: readonly ReadoutRow[] = [
  { k: "depth ±10bps", v: "—" },
  { k: "imbalance", v: "—" },
  { k: "rv_1h", v: "—" },
  { k: "vol_ratio", v: "—" },
  { k: "basis", v: "—" },
];

const ledger = ref<Ledger>("positions");
const ledgerEmpty = computed(() => LEDGER_TABS.find((tab) => tab.key === ledger.value)?.empty ?? "");
</script>

<template>
  <div class="trade">
    <div class="trade__col trade__col--left">
      <PanelHousing label="Markets">
        <EmptyState line="No markets loaded." />
      </PanelHousing>
      <PanelHousing label="Agents on —" meta="0">
        <EmptyState line="No agents paired." />
        <template #footer>Autonomous · policy-bound</template>
      </PanelHousing>
    </div>

    <div class="trade__col trade__col--center">
      <PanelHousing inset>
        <div class="strip">
          <div>
            <div class="label">Market</div>
            <div class="strip__price">—</div>
          </div>
          <div class="strip__stats">
            <StatBlock label="24h" />
            <StatBlock label="Mark" />
            <StatBlock label="Funding" />
            <StatBlock label="OI" />
            <StatBlock label="Spread" />
          </div>
        </div>
      </PanelHousing>

      <PanelHousing :brackets="['tl', 'br']" data-tour="chart">
        <template #label>
          <span v-for="tf in TIMEFRAMES" :key="tf" class="chart__tf">{{ tf }}</span>
        </template>
        <template #meta>Agent fills marked +</template>
        <EmptyState matrix mode="sweep" size="md" line="No market feed." />
      </PanelHousing>

      <PanelHousing data-tour="positions">
        <template #label>
          <button
            v-for="tab in LEDGER_TABS"
            :key="tab.key"
            type="button"
            class="ledger__tab"
            :class="{ 'ledger__tab--active': ledger === tab.key }"
            :aria-pressed="ledger === tab.key"
            @click="ledger = tab.key"
          >
            {{ tab.label }}
          </button>
        </template>
        <template #meta>0 agent-held · 0 manual</template>
        <ColumnHeader :columns="POSITION_COLUMNS" :template="POSITION_TEMPLATE" />
        <EmptyState :line="ledgerEmpty" />
      </PanelHousing>
    </div>

    <div class="trade__col trade__col--right">
      <PanelHousing label="Book · Hyperliquid" meta="L2 · 5 sigfig">
        <EmptyState line="No book subscribed." />
      </PanelHousing>
      <PanelHousing inset label="Features" meta="get_features" :brackets="['tr']">
        <ReadoutRows :rows="FEATURES" />
      </PanelHousing>
      <PanelHousing inset label="Manual order" meta="Overrides policy" data-tour="ticket">
        <EmptyState line="Ticket opens with a venue connection." />
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.trade {
  display: grid;
  flex: 1;
  grid-template-columns: 200px minmax(0, 1fr) 300px;
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.trade__col {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
}

.trade__col--left {
  grid-template-rows: 2fr 3fr;
}

.trade__col--center {
  grid-template-rows: auto 1fr 240px;
}

.trade__col--right {
  grid-template-rows: 1fr auto auto;
}

.strip {
  display: flex;
  align-items: center;
  gap: var(--s-7);
  min-width: 0;
  overflow: hidden;
}

.strip__price {
  margin-top: var(--s-1);
  font-size: var(--fs-display-xl);
  font-weight: 700;
  line-height: 1;
  letter-spacing: var(--ls-display-tight);
  color: var(--bracket);
}

.strip__stats {
  display: grid;
  grid-template-columns: repeat(5, auto);
  gap: var(--s-6);
}

.chart__tf {
  color: var(--bracket);
}

.ledger__tab {
  display: flex;
  align-items: center;
  height: 100%;
  margin-bottom: -1px;
  border-bottom: 1px solid transparent;
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label);
  text-transform: uppercase;
  color: var(--bracket);
}

.ledger__tab:hover {
  color: var(--body);
}

.ledger__tab--active,
.ledger__tab--active:hover {
  border-bottom-color: var(--signal);
  color: var(--signal);
}
</style>
