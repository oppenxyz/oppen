<script setup lang="ts">
import { computed } from "vue";
import AsciiGauge from "../components/ascii/AsciiGauge.vue";
import ColumnHeader from "../components/housing/ColumnHeader.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import { shell } from "../stores/shell";

const POSITION_COLUMNS = ["Market", "Side", "Size", "Entry", "Liq", "Liq dist", "PnL", "Margin"] as const;
const POSITION_TEMPLATE = "1.2fr 0.8fr 1fr 1fr 1fr 1fr 1fr 1.2fr";

const account = computed(() => shell.account);
const positions = computed(() => account.value?.positions ?? []);
const dash = "—";

/** Decimals arrive as strings and are formatted, never parsed for display. */
function usd(value: string | null | undefined): string {
  if (value === null || value === undefined) return dash;
  const n = Number(value);
  return Number.isFinite(n)
    ? n.toLocaleString("en-US", { style: "currency", currency: "USD", maximumFractionDigits: 2 })
    : dash;
}

function pct(fraction: string | null): string {
  if (fraction === null) return dash;
  const n = Number(fraction);
  return Number.isFinite(n) ? `${(n * 100).toFixed(2)}%` : dash;
}

const equity = computed(() => usd(account.value?.balances.equity_usd));
const marginUsed = computed(() => usd(account.value?.balances.total_margin_used_usd));
const available = computed(() => usd(account.value?.balances.spot_usdc_available));

/** Margin used as a fraction of equity, for the gauge. */
const marginFraction = computed(() => {
  const balances = account.value?.balances;
  if (!balances) return 0;
  const [used, eq] = [Number(balances.total_margin_used_usd), Number(balances.equity_usd)];
  return eq > 0 && Number.isFinite(used) ? Math.min(1, used / eq) : 0;
});

const exposure = computed(() => {
  const total = positions.value.reduce((sum, p) => sum + Math.abs(Number(p.position_value_usd) || 0), 0);
  return positions.value.length ? usd(String(total)) : dash;
});

const unrealised = computed(() => {
  if (!positions.value.length) return dash;
  const total = positions.value.reduce((sum, p) => sum + (Number(p.unrealized_pnl_usd) || 0), 0);
  return usd(String(total));
});

/** The position closest to liquidation, which is the one that matters. */
const nearestLiq = computed(() => {
  const withDistance = positions.value.filter((p) => p.liq_distance_frac !== null);
  if (!withDistance.length) return { value: dash, note: dash };
  const nearest = withDistance.reduce((a, b) =>
    Number(a.liq_distance_frac) <= Number(b.liq_distance_frac) ? a : b,
  );
  return { value: pct(nearest.liq_distance_frac), note: `${nearest.symbol} · ${usd(nearest.liquidation_px)}` };
});

const side = (size: string): string => (Number(size) >= 0 ? "LONG" : "SHORT");
</script>

<template>
  <div class="portfolio">
    <div class="portfolio__tiles">
      <PanelHousing inset label="Equity · unified margin" :brackets="['tl']">
        <div class="tile__value tile__value--xl">{{ equity }}</div>
        <div class="tile__note">{{ available }} available</div>
      </PanelHousing>
      <PanelHousing inset label="Unrealised PnL">
        <div class="tile__value">{{ unrealised }}</div>
        <div class="tile__note">across {{ positions.length }} position(s)</div>
      </PanelHousing>
      <PanelHousing inset label="Exposure">
        <div class="tile__value">{{ exposure }}</div>
        <div class="tile__note">notional at mark</div>
      </PanelHousing>
      <PanelHousing inset label="Margin used">
        <div class="tile__value">{{ marginUsed }}</div>
        <div class="tile__note">
          <AsciiGauge :value="marginFraction" :cells="14" label="Margin used" />
        </div>
      </PanelHousing>
      <PanelHousing inset label="Nearest liq">
        <div class="tile__value">{{ nearestLiq.value }}</div>
        <div class="tile__note">{{ nearestLiq.note }}</div>
      </PanelHousing>
    </div>

    <div class="portfolio__lower">
      <PanelHousing label="Positions · Hyperliquid" :meta="`${positions.length} open`">
        <ColumnHeader :columns="POSITION_COLUMNS" :template="POSITION_TEMPLATE" />
        <div
          v-for="position in positions"
          :key="position.symbol"
          class="row"
          :style="{ gridTemplateColumns: POSITION_TEMPLATE }"
        >
          <span>{{ position.symbol }}</span>
          <span :class="Number(position.size) >= 0 ? 'row__up' : 'row__down'">{{ side(position.size) }}</span>
          <span>{{ position.size }}</span>
          <span>{{ position.entry_px ?? dash }}</span>
          <span>{{ position.liquidation_px ?? dash }}</span>
          <span>{{ pct(position.liq_distance_frac) }}</span>
          <span :class="Number(position.unrealized_pnl_usd) >= 0 ? 'row__up' : 'row__down'">
            {{ usd(position.unrealized_pnl_usd) }}
          </span>
          <span>{{ usd(position.margin_used_usd) }}</span>
        </div>
        <EmptyState v-if="!positions.length" matrix size="md" line="No open positions." />
      </PanelHousing>

      <div class="portfolio__side">
        <PanelHousing inset label="Exposure by agent">
          <EmptyState line="No agents paired." />
        </PanelHousing>
        <PanelHousing inset label="By sub-account">
          <EmptyState line="No sub-accounts mapped." />
        </PanelHousing>
      </div>
    </div>
  </div>
</template>

<style scoped>
.portfolio {
  display: grid;
  flex: 1;
  grid-template-rows: auto 1fr;
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.portfolio__tiles {
  display: grid;
  grid-template-columns: 1.4fr 1fr 1fr 1fr 1fr;
  gap: var(--panel-gap);
}

.portfolio__lower {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 360px;
  gap: var(--panel-gap);
  min-height: 0;
}

.portfolio__side {
  display: grid;
  grid-template-rows: auto auto;
  align-content: start;
  gap: var(--panel-gap);
  min-height: 0;
}

.tile__value {
  font-size: var(--fs-display-lg);
  font-weight: 700;
  line-height: 1;
  letter-spacing: var(--ls-display);
  color: var(--bracket);
}

.tile__value--xl {
  font-size: var(--fs-display-2xl);
  letter-spacing: var(--ls-display-tight);
}

.tile__note {
  margin-top: var(--s-2);
  font-size: var(--fs-body-sm);
  white-space: pre;
  color: var(--bracket);
}

.row {
  display: grid;
  gap: var(--s-2);
  padding: var(--s-2) var(--s-3);
  border-bottom: 1px solid var(--rule);
  font-family: var(--font-mono);
  font-size: var(--fs-body);
  font-variant-numeric: tabular-nums;
  color: var(--body);
}

.row > span {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.row__up { color: var(--up); }
.row__down { color: var(--down); }
</style>
