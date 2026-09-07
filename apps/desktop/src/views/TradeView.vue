<script setup lang="ts">
import { computed, ref } from "vue";
import AsciiGauge from "../components/ascii/AsciiGauge.vue";
import CandleChart from "../components/CandleChart.vue";
import AccountPositions from "../components/AccountPositions.vue";
import AccountNotice from "../components/AccountNotice.vue";
import { shell, setView } from "../stores/shell";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows, { type ReadoutRow } from "../components/housing/ReadoutRows.vue";
import StatBlock from "../components/housing/StatBlock.vue";
import {
  INTERVALS,
  market,
  refreshSnapshot,
  select,
  selectedRow,
  setInterval,
  type ChartInterval,
} from "../stores/market";

import { decimal } from "../lib/display";

type Ledger = "positions" | "orders" | "fills";

const LEDGER_TABS = computed(() => [
  { key: "positions" as const, label: `Positions · ${shell.account?.positions.length ?? '—'}` },
  { key: "orders" as const, label: `Open orders · ${shell.account?.orders.length ?? '—'}` },
  { key: "fills" as const, label: "Fills" },
]);
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
      v: band === undefined ? DASH : `${decimal(band.bid_usd, 0)} / ${decimal(band.ask_usd, 0)} USD`,
      tone: band?.covers_band === false ? "uranium" : undefined,
      detail: band ? `Bid ${band.bid_usd} USD; ask ${band.ask_usd} USD. ${band.covers_band ? "Ladder covers the band." : `Observed depth is a floor. Bid reach ${snap.book.bid_reach_bps ?? "unknown"} bp; ask reach ${snap.book.ask_reach_bps ?? "unknown"} bp.`}` : "Depth unavailable",
    },
    { k: "spread", v: decimal(snap.book.spread_bps, 2, " bp"), detail: snap.book.spread_bps ?? "Unavailable" },
    { k: "imbalance · −1 to 1", v: decimal(snap.book.book_imbalance, 4), detail: snap.book.book_imbalance ?? "Unavailable" },
    { k: "volatility · 24h", v: decimal(snap.vol.rv_24h_bps, 2, " bp"), detail: snap.vol.rv_24h_bps ?? "Unavailable" },
    { k: "volume ratio", v: decimal(snap.vol.vol_ratio), detail: snap.vol.vol_ratio ?? "Unavailable" },
    { k: "funding 1h", v: decimal(snap.funding.hour_to_date_bps, 2, " bp"), detail: snap.funding.hour_to_date_bps ?? "Unavailable" },
    { k: "basis", v: decimal(snap.funding.basis_bps, 2, " bp"), detail: snap.funding.basis_bps ?? "Unavailable" },
  ];
});

const FEATURES_EMPTY: readonly ReadoutRow[] = [
  { k: "depth ±10bps", v: DASH },
  { k: "spread", v: DASH },
  { k: "imbalance", v: DASH },
  { k: "volatility · 24h", v: DASH },
  { k: "volume ratio", v: DASH },
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
    funding: row === null ? DASH : decimal(row.funding_1h_bps, 2, " bp"),
    oi: row?.open_interest ?? DASH,
  };
});

/** Book rows, deepest-first on the bid so the two sides mirror at the touch. */
const search = ref("");
const filteredMarkets = computed(() => market.rows.filter(row => row.symbol.toLowerCase().includes(search.value.trim().toLowerCase())));
const bids = computed(() => market.snapshot?.bids.slice(0, 8) ?? []);
const asks = computed(() => market.snapshot?.asks.slice(0, 8) ?? []);

const maxBookSize = computed(() => Math.max(0, ...bids.value.concat(asks.value).map(level => Number(level.sz))));
const ledger = ref<Ledger>("positions");


// The rail is read and re-read by `startMarketFeed`, which App.vue owns
// because the status bar reads the feed from every view. Nothing to do on
// mount here any more.
</script>

<template>
  <div class="trade">
    <div class="trade__col trade__col--left">
      <PanelHousing label="Markets" :meta="`${market.rows.length}`">
        <input v-model="search" type="search" class="market-search" aria-label="Search markets" placeholder="Search markets" />
        <EmptyState v-if="market.rows.length === 0" :line="market.error ?? 'No markets loaded.'" />
        <ul v-else class="rail">
          <li v-for="row in filteredMarkets" :key="row.symbol">
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
        <EmptyState v-if="market.rows.length > 0 && !filteredMarkets.length" line="No matching markets." />
      </PanelHousing>
      <PanelHousing label="Agents" meta="Not read">
        <EmptyState line="Connect the gateway to inspect agents and their containers." action="Open MCP setup" @action="setView('builder')" />
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
        <div v-else class="chart__surface">
          <p v-if="shell.feeds.wsMarket !== 'ok'" class="chart__status" role="status">
            Market feed {{ shell.feeds.wsMarket === 'unknown' ? 'not connected' : shell.feeds.wsMarket }}.
            {{ shell.lastMarketTickMs ? `Last tick ${new Date(shell.lastMarketTickMs).toLocaleTimeString()}.` : 'No live tick received.' }}
          </p>
          <CandleChart :data="market.chart" />
        </div>
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
        <template #meta>Configured account</template>
        <div class="ledger__body">
          <AccountNotice />
          <AccountPositions v-if="ledger === 'positions'" />
          <template v-else-if="ledger === 'orders'">
            <table v-if="shell.account?.orders.length" class="orders">
              <thead><tr><th>Market</th><th>Side</th><th>Size</th><th>Limit · USD</th><th>Reduce only</th></tr></thead>
              <tbody><tr v-for="order in shell.account.orders" :key="order.oid">
                <th scope="row">{{ order.symbol }}</th><td>{{ order.is_buy ? 'BUY' : 'SELL' }}</td><td>{{ order.size }}</td><td>{{ order.limit_px }}</td><td>{{ order.reduce_only ? 'Yes' : 'No' }}</td>
              </tr></tbody>
            </table>
            <EmptyState v-else :line="shell.account ? 'No open orders in this account.' : 'Orders are unknown until the account is read.'" />
          </template>
          <EmptyState v-else line="Fill history is not connected to this console yet." />
        </div>
      </PanelHousing>
    </div>

    <div class="trade__col trade__col--right">
      <PanelHousing label="Book · Hyperliquid" meta="L2">
        <EmptyState
          v-if="bids.length === 0 && asks.length === 0"
          :line="market.snapshotError ?? 'No book subscribed.'"
        />
        <div v-else class="book">
          <div class="book__row book__head"><span>Price · USD</span><span>Size · {{ market.selected }}</span><span>Orders</span><span>Depth</span></div>
          <!-- Asks descend to the touch, bids fall away from it, so the two
               best prices meet in the middle the way a book is read. -->
          <div v-for="lvl in [...asks].reverse()" :key="`a${lvl.px}`" class="book__row book__row--ask">
            <span>{{ lvl.px }}</span><span>{{ lvl.sz }}</span><span class="book__n">{{ lvl.n }}</span><AsciiGauge :value="maxBookSize > 0 ? Number(lvl.sz) / maxBookSize : null" :cells="8" :label="`${lvl.sz} ${market.selected}; relative to largest visible level`" />
          </div>
          <div class="book__mid">Snapshot spread {{ decimal(market.snapshot?.book.spread_bps, 2, " bp") }}</div>
          <div v-for="lvl in bids" :key="`b${lvl.px}`" class="book__row book__row--bid">
            <span>{{ lvl.px }}</span><span>{{ lvl.sz }}</span><span class="book__n">{{ lvl.n }}</span><AsciiGauge :value="maxBookSize > 0 ? Number(lvl.sz) / maxBookSize : null" :cells="8" :label="`${lvl.sz} ${market.selected}; relative to largest visible level`" />
          </div>
        </div>
      </PanelHousing>
      <PanelHousing inset label="Features" :brackets="['tr']">
        <template #meta>
          <button type="button" class="refresh" @click="refreshSnapshot">Re-read</button>
        </template>
        <p class="features-time">Derived snapshot · {{ market.featuresReadMs ? new Date(market.featuresReadMs).toLocaleTimeString() : 'Not read' }}</p>
        <p v-if="market.snapshotError" class="features-time" role="status">{{ market.snapshotError }}</p>
        <ReadoutRows :rows="FEATURES" />
      </PanelHousing>
      <PanelHousing inset label="Manual order" meta="Operator initiated" data-tour="ticket">
        <EmptyState line="Manual ticket is not available yet. Orders use the shared execution checks." />
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.ledger__body { min-height: 0; overflow: auto; }
.orders { width: 100%; border-collapse: collapse; font-size: var(--fs-body); }
.orders th, .orders td { padding: var(--s-2); text-align: right; font-weight: 400; border-bottom: 1px solid var(--rule); }
.orders th:first-child { text-align: left; }
.orders thead { color: var(--bracket); }
.chart__surface { position: relative; flex: 1; min-height: 0; }
.chart__status { position: absolute; z-index: 2; inset: var(--s-2) auto auto var(--s-2); max-width: calc(100% - 24px); padding: var(--s-2); background: var(--void); border: 1px solid var(--uranium); color: var(--signal); font-size: var(--fs-body); }

.features-time { font-size: var(--fs-body-sm); color: var(--bracket); margin-bottom: var(--s-2); line-height: 1.4; }

.market-search { margin: var(--s-2); padding: var(--s-2); min-width: 0; border: 1px solid var(--rule-strong); background: var(--void); color: var(--signal); font: inherit; font-size: var(--fs-body); }

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
  grid-template-columns: 1fr 1fr auto auto;
  gap: var(--s-2);
  padding: 1px var(--s-3);
}

.book__row--ask {
  color: var(--down);
}

.book__row--bid {
  color: var(--up);
}

.book__head { color: var(--bracket); font-size: var(--fs-body-sm); padding-block: var(--s-2); }

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
  grid-template-columns: 210px minmax(0, 1fr) 330px;
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
  grid-template-rows: minmax(0, 1fr) auto;
}

.trade__col--center {
  grid-template-rows: auto minmax(180px, 1fr) minmax(130px, 25vh);
}

.trade__col--right {
  grid-template-rows: 1fr auto auto;
}

.strip {
  display: flex;
  align-items: center;
  gap: var(--s-4);
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
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: var(--s-2) var(--s-4);
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
