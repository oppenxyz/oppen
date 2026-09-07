<script setup lang="ts">
import { computed } from "vue";
import AsciiGauge from "../components/ascii/AsciiGauge.vue";
import AccountPositions from "../components/AccountPositions.vue";
import AccountNotice from "../components/AccountNotice.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import { decimal } from "../lib/display";
import { shell } from "../stores/shell";

const account = computed(() => shell.account);
const positions = computed(() => account.value?.positions ?? []);
const dash = "—";

/** Exact balances are rounded only for the screen. */
function usd(value: string | null | undefined): string {
  const formatted = decimal(value);
  return formatted === dash ? dash : formatted.startsWith("−") ? `−$${formatted.slice(1)}` : `$${formatted}`;
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
  if (!balances) return null;
  const [used, eq] = [Number(balances.total_margin_used_usd), Number(balances.equity_usd)];
  return eq > 0 && Number.isFinite(used) ? used / eq : null;
});

const exposure = computed(() => {
  const total = positions.value.reduce((sum, p) => sum + Math.abs(Number(p.position_value_usd) || 0), 0);
  return account.value ? usd(String(total)) : dash;
});

const totalExposure = computed(() => positions.value.reduce((sum, p) => sum + Math.abs(Number(p.position_value_usd)), 0));
const unrealised = computed(() => {
  if (!account.value) return dash;
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


</script>

<template>
  <div class="portfolio">
    <div><AccountNotice /></div>
    <div class="portfolio__tiles">
      <PanelHousing inset label="Equity · configured account" :brackets="['tl']" data-tour="portfolio">
        <div class="tile__value tile__value--xl">{{ equity }}</div>
        <div class="tile__note">{{ available }} available</div>
      </PanelHousing>
      <PanelHousing inset label="Unrealised PnL">
        <div class="tile__value">{{ unrealised }}</div>
        <div class="tile__note">{{ account ? `across ${positions.length} position(s)` : "Account not read" }}</div>
      </PanelHousing>
      <PanelHousing inset label="Exposure">
        <div class="tile__value">{{ exposure }}</div>
        <div class="tile__note">notional at mark</div>
      </PanelHousing>
      <PanelHousing inset label="Margin used">
        <div class="tile__value">{{ marginUsed }}</div>
        <div class="tile__note">
          <AsciiGauge v-if="marginFraction !== null" :value="marginFraction" :cells="14" label="Margin used" /><span v-else>Utilization unknown</span>
        </div>
      </PanelHousing>
      <PanelHousing inset label="Nearest liq">
        <div class="tile__value">{{ nearestLiq.value }}</div>
        <div class="tile__note">{{ nearestLiq.note }}</div>
      </PanelHousing>
    </div>

    <div class="portfolio__lower">
      <PanelHousing label="Positions · Hyperliquid" :meta="account ? `${positions.length} open` : 'Not read'">
        <AccountPositions />
      </PanelHousing>

      <div class="portfolio__side">
        <PanelHousing inset label="Exposure by market" :brackets="['br']">
          <div v-for="position in positions" :key="position.symbol" class="exposure-row">
            <div>{{ position.symbol }} <span>{{ usd(position.position_value_usd.replace('-', '')) }}</span></div>
            <AsciiGauge :value="totalExposure > 0 ? Math.abs(Number(position.position_value_usd)) / totalExposure : null" :cells="28" :label="`${position.symbol} share of account gross exposure`" />
          </div>
          <EmptyState v-if="!positions.length" :line="account ? 'No position exposure.' : 'Exposure is unknown until the account is read.'" />
        </PanelHousing>
        <PanelHousing inset label="By container">
          <p v-if="account" class="container-address">Hyperliquid · {{ account.network }}<br />{{ account.address }}</p>
          <EmptyState line="Agent attribution and other containers are not connected. Margin remains independent per account." />
        </PanelHousing>
      </div>
    </div>
  </div>
</template>

<style scoped>
.portfolio {
  display: grid;
  flex: 1;
  grid-template-rows: auto auto minmax(0, 1fr);
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
  color: var(--signal);
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

.exposure-row { padding-block: var(--s-3); border-bottom: 1px solid var(--rule); font-size: var(--fs-body); }
.exposure-row > div { display: flex; justify-content: space-between; margin-bottom: var(--s-2); color: var(--signal); }
.container-address { overflow-wrap: anywhere; font-size: var(--fs-body); color: var(--body); line-height: 1.6; }
</style>
