<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { operator, recordedAgents } from "../stores/operator";
import DecisionStream from "../components/DecisionStream.vue";
import AsciiGauge from "../components/ascii/AsciiGauge.vue";
import CandleChart from "../components/CandleChart.vue";
import MarketChannelHealth from "../components/MarketChannelHealth.vue";
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
  chartObservation,
  quotes,
  quoteSpread,
  quoteValidity,
  refreshSnapshot,
  select,
  selectedRow,
  setInterval,
  type ChartInterval,
} from "../stores/market";

import { decimal } from "../lib/display";

type Ledger = "positions" | "orders" | "fills" | "activity";

const LEDGER_TABS = computed(() => [
  { key: "positions" as const, label: `Positions · ${shell.account?.positions.length ?? '—'}` },
  { key: "orders" as const, label: `Open orders · ${shell.account?.orders.length ?? '—'}` },
  { key: "fills" as const, label: "Fills" },
  { key: "activity" as const, label: "Activity" },
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
  const chart = chartObservation.data;
  if (chart === null) return "No bars.";
  const forming = chart.forming === null ? "" : " · 1 forming";
  return `${chart.closed.length} elapsed${forming}`;
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
    { k: "imbalance · −1 to 1", v: decimal(snap.book.book_imbalance, 4), detail: `Touch notional (bid − ask) / (bid + ask), from −1 to 1. Positive means more notional on the best bid. Exact: ${snap.book.book_imbalance ?? "unavailable"}.` },
    { k: "volatility · 24h", v: decimal(snap.vol.rv_24h_bps, 2, " bp"), detail: `Realized volatility from ${snap.vol.bars_24h} hourly bars. Exact: ${snap.vol.rv_24h_bps ?? "unavailable"} bp.` },
    { k: "volatility ratio", v: decimal(snap.vol.vol_ratio), detail: `1h volatility / (24h volatility / √24). One means the hour matches the day’s implied hourly volatility. Minute bars: ${snap.vol.bars_1h}; hourly bars: ${snap.vol.bars_24h}. Exact: ${snap.vol.vol_ratio ?? "unavailable"}.` },
    { k: "funding 1h", v: decimal(snap.funding.hour_to_date_bps, 2, " bp"), detail: snap.funding.hour_to_date_bps ?? "Unavailable" },
    { k: "basis", v: decimal(snap.funding.basis_bps, 2, " bp"), detail: snap.funding.basis_bps ?? "Unavailable" },
  ];
});

const FEATURES_EMPTY: readonly ReadoutRow[] = [
  { k: "depth ±10bps", v: DASH },
  { k: "spread", v: DASH },
  { k: "imbalance", v: DASH },
  { k: "volatility · 24h", v: DASH },
  { k: "volatility ratio", v: DASH },
  { k: "funding 1h", v: DASH },
  { k: "basis", v: DASH },
];

/** The strip above the chart, served by the rail rather than the snapshot. */
const strip = computed(() => {
  const row = selectedRow.value;
  return {
    symbol: row?.symbol ?? market.selected ?? DASH,
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
const bids = computed(() => quotes.depth?.bids.slice(0, 8) ?? []);
const asks = computed(() => quotes.depth?.asks.slice(0, 8) ?? []);
const spread = computed(() => {
  const value = quoteSpread(quotes.touch?.bid?.px, quotes.touch?.ask?.px);
  return value === null ? "Unavailable" : `${value.toFixed(2)} bp`;
});
const quoteLabel = computed(() => !quotes.touch ? "Touch unavailable"
  : quotes.touch.source === "rest" ? "REST touch snapshot"
  : quotes.touch.live ? `Latest observed ${quotes.touch.source.toUpperCase()} touch` : "Retained touch · not live");
const observationNow = ref(Date.now());
watch([() => quotes.touch?.observedMs, () => quotes.depth?.observedMs,
  () => chartObservation.projection?.last_observation_received_at_ms], () => {
  observationNow.value = Date.now();
}, { flush: "sync" });
let observationTimer: ReturnType<typeof globalThis.setInterval> | null = null;
onMounted(() => {
  observationNow.value = Date.now();
  observationTimer = globalThis.setInterval(() => { observationNow.value = Date.now(); }, 1000);
});
onUnmounted(() => {
  if (observationTimer !== null) globalThis.clearInterval(observationTimer);
});
function observationAge(time: number | null | undefined): string {
  if (time == null) return "Unavailable";
  const elapsed = observationNow.value - time;
  return elapsed < 0 ? "UI clock moved backward" : `${Math.floor(elapsed / 1000)}s ago`;
}
function quoteTime(time: number | null | undefined): string {
  if (time == null) return "Unavailable";
  const date = new Date(time);
  return Number.isFinite(date.getTime()) ? date.toISOString() : "Unavailable";
}

const chartBars = computed(() => {
  const projection = chartObservation.projection;
  return projection ? [...projection.closed, ...(projection.forming ? [projection.forming] : [])] : [];
});
const chartSource = computed(() => {
  const bars = chartBars.value;
  const venue = bars.filter(bar => bar.source === "venue").length;
  const partial = bars.filter(bar => bar.partial).length;
  const ambiguous = bars.filter(bar => bar.open_close_ambiguous).length;
  return `Venue ${venue} · Observed tape ${bars.length - venue} · Partial ${partial} · Ambiguous O/C ${ambiguous}`;
});
const tapeLabel = computed(() => {
  if (chartObservation.failure) return "Chart consumer stopped";
  switch (chartObservation.projection?.tape_status) {
    case "observing": return "Tape observing · incomplete coverage";
    case "interrupted": return "Tape interrupted";
    case "capacity_exceeded": return "Tape frozen · identity capacity exceeded";
    case "invalid_observation": return "Tape frozen · invalid observation";
    default: return "Tape not observed";
  }
});
const markerLabel = computed(() => {
  const projection = chartObservation.projection;
  const marker = projection?.latest_trade;
  if (!marker) return "Latest observed trade unavailable.";
  const unverified = chartObservation.retained || projection?.tape_status !== "observing";
  return `Latest observed trade ${marker.price} · ${quoteTime(marker.time_ms)}${marker.price_ambiguous ? ' · Ambiguous' : ''}${unverified ? ' · Unverified' : ''}`;
});
const chartSummary = computed(() => `${chartSource.value}. Volume follows each bar's source; venue and observed-trade volume are never combined. ${tapeLabel.value}. ${markerLabel.value}. The trade marker is independent of candle OHLCV and is not asserted newer than the venue candle. ${chartObservation.retained ? 'Retained chart; not live.' : ''} ${chartObservation.failure?.detail ?? ''}`);

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
      <PanelHousing label="Recorded agents" :meta="operator.policy || operator.ledger ? String(recordedAgents.length) : 'Not read'">
        <EmptyState :line="recordedAgents.length ? recordedAgents.join(', ') : 'Connect the gateway to inspect agents and their containers.'" :action="recordedAgents.length ? 'Inspect records' : 'Open MCP setup'" @action="setView(recordedAgents.length ? 'agents' : 'builder')" />
        <template #footer>Records do not prove active pairing</template>
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
            <StatBlock data-quote="bid" label="Best bid" :value="quotes.touch?.bid?.px ?? 'Unavailable'" />
            <StatBlock data-quote="ask" label="Best ask" :value="quotes.touch?.ask?.px ?? 'Unavailable'" />
            <StatBlock data-quote="spread" :label="quotes.touch?.live ? 'Latest observed approx. spread' : 'Retained approx. spread'" :value="spread" />
          </div>
        </div>
        <p class="quote-time" data-quote="touch-clock">{{ quoteLabel }} · Venue {{ quoteTime(quotes.touch?.venueMs) }} · Observed by UI {{ quoteTime(quotes.touch?.observedMs) }} · {{ observationAge(quotes.touch?.observedMs) }}</p>
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
        <div class="chart__surface">
          <p class="chart-observation" data-chart="status">{{ chartObservation.retained ? 'Retained chart · not live' : 'Latest chart observation' }} · {{ tapeLabel }}<br />{{ chartSource }}</p>
          <p class="chart-observation" data-chart="age">Host observed {{ quoteTime(chartObservation.projection?.last_observation_received_at_ms) }} · {{ observationAge(chartObservation.projection?.last_observation_received_at_ms) }}</p>
          <p class="chart-observation" data-chart="latest-trade">{{ markerLabel }}</p>
          <p v-if="chartObservation.data?.priceDecimals == null" class="chart-observation" data-chart="precision">Asset precision unavailable · display fallback: 6 decimals.</p>
          <p v-if="chartObservation.error" class="chart-observation chart-observation--error" data-chart="history-error" role="status">History read: {{ chartObservation.error }}</p>
          <p v-if="chartObservation.projection?.observation_error" class="chart-observation chart-observation--error" data-chart="observation-error" role="status">{{ chartObservation.projection.observation_error }}</p>
          <p v-if="chartObservation.failure" class="chart-observation chart-observation--error" data-chart="consumer-error" role="status">{{ chartObservation.failure.detail }}</p>
          <div class="chart__plot"><CandleChart :data="chartObservation.data" :observation-summary="chartSummary" /></div>
          <details v-if="chartBars.length" class="chart-details">
            <summary>Bar observations · exact readings</summary>
            <div class="chart-details__rows"><p v-for="bar in chartBars" :key="bar.time_ms">
              {{ quoteTime(bar.time_ms) }} · {{ bar.source === 'venue' ? 'Venue-reported' : 'Observed trades only' }} · {{ bar.partial ? 'Partial coverage' : 'Venue bar' }}{{ bar.open_close_ambiguous ? ' · Open/close ambiguous' : '' }}<br />
              O {{ bar.open }} · H {{ bar.high }} · L {{ bar.low }} · C {{ bar.close }} · Volume {{ bar.volume }} {{ market.selected }}<br />Received by native host {{ quoteTime(bar.received_at_ms) }}
            </p></div>
          </details>
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
          <DecisionStream v-else :fills-only="ledger === 'fills'" />
        </div>
      </PanelHousing>
    </div>

    <div class="trade__col trade__col--right">
      <MarketChannelHealth />
      <PanelHousing label="Book · Hyperliquid" :meta="quotes.depth?.source === 'rest' ? 'REST snapshot' : 'L2 depth'">
        <p class="quote-time" data-quote="validity">Quote · {{ quoteValidity }}{{ quotes.touch && !quotes.touch.live ? ' · Retained' : '' }}</p>
        <p class="quote-time" data-quote="depth-clock">{{ !quotes.depth ? 'Depth unavailable' : quotes.depth.live ? 'Latest observed depth' : 'Retained depth · not live' }}<br />Venue {{ quoteTime(quotes.depth?.venueMs) }}<br />Observed by UI {{ quoteTime(quotes.depth?.observedMs) }} · {{ observationAge(quotes.depth?.observedMs) }}</p>
        <EmptyState
          v-if="bids.length === 0 && asks.length === 0"
          line="Depth unavailable."
        />
        <div v-else class="book">
          <div class="book__row book__head"><span>Price · USD</span><span>Size · {{ market.selected }}</span><span>Orders</span><span>Depth</span></div>
          <!-- Asks descend to the touch, bids fall away from it, so the two
               best prices meet in the middle the way a book is read. -->
          <div v-for="lvl in [...asks].reverse()" :key="`a${lvl.px}`" class="book__row book__row--ask">
            <span>{{ lvl.px }}</span><span>{{ lvl.sz }}</span><span class="book__n">{{ lvl.n }}</span><AsciiGauge :value="maxBookSize > 0 ? Number(lvl.sz) / maxBookSize : null" :cells="8" :label="`${lvl.sz} ${market.selected}; relative to largest visible level`" />
          </div>
          <div class="book__mid">{{ quoteLabel }} · Approx. spread {{ spread }}</div>
          <div v-for="lvl in bids" :key="`b${lvl.px}`" class="book__row book__row--bid">
            <span>{{ lvl.px }}</span><span>{{ lvl.sz }}</span><span class="book__n">{{ lvl.n }}</span><AsciiGauge :value="maxBookSize > 0 ? Number(lvl.sz) / maxBookSize : null" :cells="8" :label="`${lvl.sz} ${market.selected}; relative to largest visible level`" />
          </div>
        </div>
      </PanelHousing>
      <PanelHousing inset label="Features" :brackets="['tr']">
        <template #meta>
          <button type="button" class="refresh" @click="refreshSnapshot">Re-read</button>
        </template>
        <p class="features-time">Derived REST snapshot · Host request started {{ market.featuresReadMs === null ? 'Not read' : quoteTime(market.featuresReadMs) }}</p>
        <p v-if="market.snapshotError" class="features-time" role="status">{{ market.snapshotError }}</p>
        <ReadoutRows :rows="FEATURES" />
        <details class="feature-details">
          <summary>Definitions &amp; exact readings</summary>
          <dl><template v-for="feature in FEATURES" :key="feature.k"><dt>{{ feature.k }}</dt><dd>{{ feature.detail ?? 'Not read' }}</dd></template></dl>
        </details>
      </PanelHousing>
      <PanelHousing inset label="Manual order" meta="Operator initiated" data-tour="ticket">
        <EmptyState line="Manual ticket is not available yet. Orders use the shared execution checks." />
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.quote-time { padding: var(--s-2) var(--s-3); font-size: var(--fs-body-sm); color: var(--bracket); overflow-wrap: anywhere; line-height: 1.5; }
.feature-details { padding-top: var(--s-3); font-size: var(--fs-body-sm); line-height: 1.6; }
.feature-details summary { cursor: pointer; color: var(--body); }
.feature-details dt { margin-top: var(--s-3); color: var(--signal); }
.feature-details dd { color: var(--body); overflow-wrap: anywhere; }

.ledger__body { min-height: 0; overflow: auto; }
.orders { width: 100%; border-collapse: collapse; font-size: var(--fs-body); }
.orders th, .orders td { padding: var(--s-2); text-align: right; font-weight: 400; border-bottom: 1px solid var(--rule); }
.orders th:first-child { text-align: left; }
.orders thead { color: var(--bracket); }
.chart__surface { display: flex; flex-direction: column; flex: 1; min-height: 0; overflow: auto; }
.chart__plot { flex: 1; min-height: 180px; }
.chart-observation, .chart-details { font-size: var(--fs-body-sm); padding: 2px var(--s-2); color: var(--body); line-height: 1.3; overflow-wrap: anywhere; }
.chart-observation--error { color: var(--down); }
.chart-details summary { cursor: pointer; }
.chart-details__rows { max-height: 140px; overflow: auto; }
.chart-details__rows p { margin-block: var(--s-2); }

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
  grid-template-rows: auto minmax(340px, 1fr) minmax(110px, 18vh);
  overflow: auto;
}

.trade__col--right {
  grid-template-rows: max-content max-content max-content;
  align-content: start;
  overflow: auto;
}

.strip {
  display: flex;
  align-items: center;
  gap: var(--s-4);
  min-width: 0;
  flex-wrap: wrap;
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
  min-width: 0;
  flex: 1;
  overflow-wrap: anywhere;
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
