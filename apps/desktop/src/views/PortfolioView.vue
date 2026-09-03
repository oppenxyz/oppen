<script setup lang="ts">
import AsciiGauge from "../components/ascii/AsciiGauge.vue";
import ColumnHeader from "../components/housing/ColumnHeader.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";

const POSITION_COLUMNS = ["Market", "Side", "Size", "Entry", "Mark", "Liq", "PnL", "Agent · sub-acct"] as const;
const POSITION_TEMPLATE = "1.2fr 0.8fr 1fr 1fr 1fr 1fr 1fr 1.2fr";
</script>

<template>
  <div class="portfolio">
    <div class="portfolio__tiles">
      <PanelHousing inset label="Equity · unified margin" :brackets="['tl']">
        <div class="tile__value tile__value--xl">—</div>
      </PanelHousing>
      <PanelHousing inset label="PnL 24h">
        <div class="tile__value">—</div>
        <div class="tile__note">price — · funding — · fees —</div>
      </PanelHousing>
      <PanelHousing inset label="Exposure">
        <div class="tile__value">—</div>
        <div class="tile__note">— of equity</div>
      </PanelHousing>
      <PanelHousing inset label="Margin used">
        <div class="tile__value">—</div>
        <div class="tile__note"><AsciiGauge :value="0" :cells="14" label="Margin used" /> halt —</div>
      </PanelHousing>
      <PanelHousing inset label="Nearest liq">
        <div class="tile__value">—</div>
        <div class="tile__note">—</div>
      </PanelHousing>
    </div>

    <div class="portfolio__lower">
      <PanelHousing label="Positions · Hyperliquid" meta="0 open">
        <ColumnHeader :columns="POSITION_COLUMNS" :template="POSITION_TEMPLATE" />
        <EmptyState matrix size="md" line="No open positions." />
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
</style>
