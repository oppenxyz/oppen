<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import CandleChart from "../components/CandleChart.vue";
import ColumnHeader from "../components/housing/ColumnHeader.vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows, { type ReadoutRow } from "../components/housing/ReadoutRows.vue";
import StatBlock from "../components/housing/StatBlock.vue";
import {
  INTERVALS,
  market,
  refreshMarkets,
  refreshSnapshot,
  select,
  selectedRow,
  setInterval,
  type ChartInterval,
} from "../stores/market";

type Ledger = "positions" | "orders" | "fills";

const LEDGER_TABS: ReadonlyArray<{ key: Ledger; label: string; empty: string }> = [
  { key: "positions", label: "Positions · 0", empty: "No open positions." },
  { key: "orders", label: "Open orders · 0", empty: "No open orders." },
  { key: "fills", label: "Fills", empty: "No fills." },
];

const POSITION_COLUMNS = ["Market", "Side", "Size", "Entry", "Mark", "Liq", "PnL", "Agent · sub-acct"] as const;
const POSITION_TEMPLATE = "1.2fr 0.8fr 1fr 1fr 1fr 1fr 1fr 1.2fr";

/** A missing number renders as an em dash. It never renders as zero. */
const DASH = "—";

/**
 * How many bars the chart is drawing, and whether one of them is still open.
 *
 * The count is worth showing because the renderer draws the last `cols` of
 * what it is given: a panel narrower than the window says less than the number
 * beside it, and an operator reading a thin chart should be able to tell a
 * short history from a narrow panel.
 */
const chartMeta = computed(() => {
  const chart = market.chart;
  if (chart === null) return "No bars.";
  const forming = chart.forming === null ? "" : " · 1 forming";
  return `${chart.closed.length} closed${forming}`;
});

function show(value: string | undefined, suffix = ""): string {
  return value === undefined ? DASH : `${value}${suffix}`;
}

/**
 * Spec F's packs, as the panel reads them.
 *
 * `micro_tilt_bps` is not here: it needs `bbo` at its ~0.11 s cadence and the
 * console holds no socket, so `get_features` answers it for agents and this
 * panel does not pretend to. Depth carries `covers_band` — a band the ladder
 * never reached is marked rather than shown as if it were the band's contents.
 */
const FEATURES = computed<readonly ReadoutRow[]>(() => {
  const snap = market.snapshot;
  if (snap === null) return FEATURES_EMPTY;
  const band = snap.book.depth.find((d) => d.band_bps === 10);
  return [
    {
      k: band?.covers_band === false ? "depth ±10bps ·floor" : "depth ±10bps",
      v: band === undefined ? DASH : `${band.bid_usd} / ${band.ask_usd}`,
      tone: band?.covers_band === false ? "uranium" : undefined,
    },
    { k: "spread", v: show(snap.book.spread_bps, " bp") },
    { k: "imbalance", v: show(snap.book.book_imbalance) },
    { k: "rv_24h", v: show(snap.vol.rv_24h_bps, " bp") },
    { k: "vol_ratio", v: show(snap.vol.vol_ratio) },
    { k: "funding 1h", v: show(snap.funding.hour_to_date_bps, " bp") },
    { k: "basis", v: show(snap.funding.basis_bps, " bp") },
  ];
});

const FEATURES_EMPTY: readonly ReadoutRow[] = [
  { k: "depth ±10bps", v: DASH },
  { k: "spread", v: DASH },
  { k: "imbalance", v: DASH },
  { k: "rv_24h", v: DASH },
  { k: "vol_ratio", v: DASH },
  { k: "funding 1h", v: DASH },
  { k: "basis", v: DASH },
];

/** The strip above the chart, served by the rail rather than the snapshot. */
const strip = computed(() => {
  const row = selectedRow.value;
  return {
    symbol: row?.symbol ?? DASH,
    mark: row?.mark_px ?? DASH,
    // A market the venue has stopped quoting has no mid, and no substitute.
    mid: row?.mid_px ?? DASH,
    change: row?.change_24h_pct === undefined ? DASH : `${row.change_24h_pct}%`,
    funding: row === null ? DASH : `${row.funding_1h_bps} bp`,
    oi: row?.open_interest ?? DASH,
  };
});

/** Book rows, deepest-first on the bid so the two sides mirror at the touch. */
const bids = computed(() => market.snapshot?.bids.slice(0, 8) ?? []);
const asks = computed(() => market.snapshot?.asks.slice(0, 8) ?? []);

const ledger = ref<Ledger>("positions");
const ledgerEmpty = computed(() => LEDGER_TABS.find((tab) => tab.key === ledger.value)?.empty ?? "");

onMounted(() => void refreshMarkets());
</script>

<template>
  <div class="trade">
    <div class="trade__col trade__col--left">
      <PanelHousing label="Markets" :meta="`${market.rows.length}`">
        <EmptyState v-if="market.rows.length === 0" :line="market.error ?? 'No markets loaded.'" />
        <ul v-else class="rail">
          <li v-for="row in market.rows" :key="row.symbol">
            <button
              type="button"
              class="rail__row"
              :class="{
                'rail__row--on': row.symbol === market.selected,
                'rail__row--dark': !row.has_book,
              }"
              :aria-pressed="row.symbol === market.selected"
              :title="row.has_book ? undefined : 'The venue is quoting no book for this asset.'"
              @click="select(row.symbol)"
            >
              <span class="rail__sym">{{ row.symbol }}</span>
              <span class="rail__px">{{ row.mark_px }}</span>
              <span
                class="rail__chg"
                :class="{
                  'rail__chg--up': Number(row.change_24h_pct ?? 0) > 0,
                  'rail__chg--down': Number(row.change_24h_pct ?? 0) < 0,
                }"
              >{{ row.change_24h_pct === undefined ? "—" : `${row.change_24h_pct}%` }}</span>
            </button>
          </li>
        </ul>
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
            <div class="label">{{ strip.symbol }}</div>
            <div class="strip__price">{{ strip.mid }}</div>
          </div>
          <div class="strip__stats">
            <StatBlock label="24h" :value="strip.change" />
            <StatBlock label="Mark" :value="strip.mark" />
            <StatBlock label="Funding" :value="strip.funding" />
            <StatBlock label="OI" :value="strip.oi" />
            <StatBlock label="Spread" :value="FEATURES[1]?.v ?? '—'" />
          </div>
        </div>
      </PanelHousing>

      <PanelHousing :brackets="['tl', 'br']" data-tour="chart">
        <template #label>
          <button
            v-for="tf in INTERVALS"
            :key="tf"
            type="button"
            class="chart__tf"
            :class="{ 'chart__tf--active': market.interval === tf }"
            :aria-pressed="market.interval === tf"
            @click="setInterval(tf as ChartInterval)"
          >
            {{ tf.toUpperCase() }}
          </button>
        </template>
        <template #meta>{{ chartMeta }}</template>
        <EmptyState
          v-if="market.chartError !== null"
          matrix
          mode="sweep"
          size="md"
          :line="market.chartError"
        />
        <CandleChart v-else :data="market.chart" />
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
      <PanelHousing label="Book · Hyperliquid" meta="L2">
        <EmptyState
          v-if="bids.length === 0 && asks.length === 0"
          :line="market.snapshotError ?? 'No book subscribed.'"
        />
        <div v-else class="book">
          <!-- Asks descend to the touch, bids fall away from it, so the two
               best prices meet in the middle the way a book is read. -->
          <div v-for="lvl in [...asks].reverse()" :key="`a${lvl.px}`" class="book__row book__row--ask">
            <span>{{ lvl.px }}</span><span>{{ lvl.sz }}</span><span class="book__n">{{ lvl.n }}</span>
          </div>
          <div class="book__mid">{{ market.snapshot?.book.spread_bps ?? "—" }} bp</div>
          <div v-for="lvl in bids" :key="`b${lvl.px}`" class="book__row book__row--bid">
            <span>{{ lvl.px }}</span><span>{{ lvl.sz }}</span><span class="book__n">{{ lvl.n }}</span>
          </div>
        </div>
      </PanelHousing>
      <PanelHousing inset label="Features" :brackets="['tr']">
        <template #meta>
          <button type="button" class="refresh" @click="refreshSnapshot">Re-read</button>
        </template>
        <ReadoutRows :rows="FEATURES" />
      </PanelHousing>
      <PanelHousing inset label="Manual order" meta="Overrides policy" data-tour="ticket">
        <EmptyState line="Ticket opens with a venue connection." />
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.rail {
  display: grid;
  margin: 0;
  padding: 0;
  list-style: none;
  overflow-y: auto;
}

.rail__row {
  display: grid;
  grid-template-columns: 1fr auto auto;
  gap: var(--s-2);
  width: 100%;
  padding: var(--s-2) var(--s-3);
  border: 0;
  border-left: 2px solid transparent;
  background: none;
  font: inherit;
  font-size: var(--fs-body);
  color: var(--body);
  text-align: left;
  cursor: pointer;
}

.rail__row:hover {
  background: var(--plate);
}

.rail__row--on {
  border-left-color: var(--uranium);
  color: var(--signal);
}

/* An asset the venue quotes no book for. Dimmed rather than hidden: 38.6% of
   the mainnet universe has none, and an operator who cannot find an asset
   learns less than one who finds it marked untradeable. */
.rail__row--dark {
  color: var(--bracket);
}

.rail__sym {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.rail__px {
  color: var(--signal-dim);
}

.rail__chg {
  min-width: 6ch;
  text-align: right;
}

.rail__chg--up {
  color: var(--up);
}

.rail__chg--down {
  color: var(--down);
}

.book {
  display: grid;
  font-size: var(--fs-body);
}

.book__row {
  display: grid;
  grid-template-columns: 1fr 1fr auto;
  gap: var(--s-2);
  padding: 1px var(--s-3);
}

.book__row--ask {
  color: var(--down);
}

.book__row--bid {
  color: var(--up);
}

.book__n {
  min-width: 3ch;
  text-align: right;
  color: var(--bracket);
}

.book__mid {
  padding: var(--s-1) var(--s-3);
  border-block: 1px solid var(--rule);
  color: var(--bracket);
  text-align: center;
}

.refresh {
  border: 0;
  background: none;
  font: inherit;
  font-size: var(--fs-label);
  letter-spacing: var(--ls-chip);
  text-transform: uppercase;
  color: var(--bracket);
  cursor: pointer;
}

.refresh:hover {
  color: var(--signal);
}

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
  grid-template-columns: repeat(6, auto);
  gap: var(--s-6);
}

.chart__tf {
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label);
  color: var(--bracket);
}

.chart__tf:hover {
  color: var(--body);
}

.chart__tf--active {
  color: var(--signal);
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
