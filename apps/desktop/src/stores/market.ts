/**
 * The market the operator is looking at (`docs/spec.md` items 30–31).
 *
 * Two reads on two clocks, and the split is the whole design. The **rail**
 * refreshes on a tick because it is one venue read and it drives the strip
 * too; the **snapshot** is three reads and is fetched when a symbol is
 * selected, not on a timer. Polling the snapshot would triple the console's
 * venue traffic for a book nobody is watching change.
 *
 * That is also why `MarketSnapshot` carries `as_of_ms`: it ages differently
 * from everything beside it, and spec item 34's rule — surface staleness per
 * feed rather than collapsing it into one outage — applies to a feed that
 * happens to be a REST read.
 */

import { computed, reactive, readonly } from "vue";

import type { Bar } from "../lib/candles";
import {
  fetchChartSeries,
  fetchMarketSnapshot,
  fetchMarkets,
  inTauri,
  isConsoleError,
  onFeedUpdate,
  watchMarket,
  type BookLevel,
  type ChartBar,
  type FeedUpdate,
  type MarketRow,
  type MarketSnapshot,
} from "../lib/bridge";
import { ageFeeds, feedStatus, feedTick, shell } from "./shell";

/** Bar intervals the chart offers. Native on the venue, so nothing resamples. */
export const INTERVALS = ["1m", "5m", "15m", "1h", "4h", "1d"] as const;
export type ChartInterval = (typeof INTERVALS)[number];

/** What the renderer needs, once the strings have been parsed. */
export interface ChartData {
  closed: readonly Readonly<Bar>[];
  forming: Readonly<Bar> | null;
  intervalMs: number;
  priceDecimals: number;
}

interface MarketState {
  rows: MarketRow[];
  /** `null` until the operator picks one, or the rail's busiest arrives. */
  selected: string | null;
  snapshot: MarketSnapshot | null;
  /** Derived packs keep their own REST time when the book receives new ticks. */
  featuresReadMs: number | null;
  /** Why the last rail read failed, shown beside the stale rows. */
  error: string | null;
  /** Why the last snapshot read failed. Kept apart: the two fail separately. */
  snapshotError: string | null;
  /** Parsed bars for the chart. `null` before the first read of a symbol. */
  chart: ChartData | null;
  interval: ChartInterval;
  /**
   * Why the chart is empty. A third error channel rather than a shared one,
   * because a broken partition costs the chart and nothing else — the book
   * beside it is still good, and collapsing the two would make the panel
   * claim an outage it is not having.
   */
  chartError: string | null;
}

const state = reactive<MarketState>({
  rows: [],
  selected: null,
  snapshot: null,
  featuresReadMs: null,
  error: null,
  snapshotError: null,
  chart: null,
  interval: "1h",
  chartError: null,
});

/**
 * Parse one bar's strings into the numbers the renderer scales to pixels.
 *
 * This is the single place a price stops being exact, and it is deliberate:
 * the renderer maps every price to a character cell, so the rounding is far
 * below anything drawable. **Nothing here may reach an order.** The order path
 * reads the venue's own decimals through the guardrail engine and never sees
 * this module.
 *
 * A bar carrying anything unparseable is dropped rather than drawn as zero —
 * the renderer reports `no_finite_bars` for a window with nothing in it, which
 * is a state the panel can name.
 *
 * Exported as a test seam (`AGENTS.md` leanness rule 2): this is the single
 * point where an exact decimal becomes a float, and a boundary that silently
 * turned a bad price into `0` would draw a candle to the floor of the chart
 * and look deliberate.
 */
export function parseBar(raw: ChartBar): Bar | null {
  const open = num(raw.open);
  const high = num(raw.high);
  const low = num(raw.low);
  const close = num(raw.close);
  const volume = num(raw.volume);
  if (open === null || high === null || low === null || close === null || volume === null) {
    return null;
  }
  return { time: raw.time_ms, open, high, low, close, volume };
}

/**
 * One decimal string to a float, or `null`.
 *
 * **The empty check is not redundant with the finite check**, which is the
 * whole reason this is a named function. `Number("")` is `0`, not `NaN`, so a
 * guard built only on `Number.isFinite` accepts a missing price and hands the
 * renderer a candle drawn to the floor of the chart — a reading that looks
 * deliberate and never happened. The same holds for a whitespace-only field.
 */
function num(raw: string): number | null {
  if (raw.trim() === "") return null;
  const value = Number(raw);
  return Number.isFinite(value) ? value : null;
}

export const market = readonly(state);

/** The selected row, for the strip above the chart. */
export const selectedRow = computed<MarketRow | null>(
  () => state.rows.find((row) => row.symbol === state.selected) ?? null,
);

function reason(error: unknown): string {
  if (isConsoleError(error)) return error.detail;
  return String(error);
}

/**
 * Refreshes the rail.
 *
 * A failed read keeps the last good rows and says why beside them — the same
 * posture `refreshAccount` takes, and for the same reason: a rail that empties
 * on one bad response loses the operator their place.
 */
export async function refreshMarkets(): Promise<void> {
  if (!inTauri()) return;
  try {
    state.rows = await fetchMarkets(shell.network);
    state.error = null;
    // First good read picks the busiest market, so the console opens on
    // something rather than on an empty strip.
    if (state.selected === null && state.rows.length > 0) {
      await select(state.rows[0]!.symbol);
    }
  } catch (error) {
    state.error = reason(error);
  }
}

/**
 * Selects a symbol and reads it in depth.
 *
 * The selection lands immediately and the snapshot follows, so the strip —
 * which is served by the rail, not the snapshot — updates without waiting on
 * three venue reads.
 */
let snapshotRequest = 0;
async function readSnapshot(symbol: string): Promise<void> {
  const request = ++snapshotRequest;
  const network = shell.network;
  try {
    const snapshot = await fetchMarketSnapshot(network, symbol);
    if (request !== snapshotRequest || state.selected !== symbol || shell.network !== network) return;
    state.snapshot = snapshot;
    state.featuresReadMs = snapshot.as_of_ms;
    state.snapshotError = null;
  } catch (error) {
    if (request === snapshotRequest && state.selected === symbol && shell.network === network) state.snapshotError = reason(error);
  }
}

export async function select(symbol: string): Promise<void> {
  state.selected = symbol;
  state.snapshot = null;
  state.featuresReadMs = null;
  state.snapshotError = null;
  state.chart = null;
  state.chartError = null;
  snapshotRequest += 1;
  if (!inTauri()) return;
  await watchSelected();
  if (state.selected !== symbol) return;
  await Promise.all([readSnapshot(symbol), refreshChart()]);
}

/** Re-read derived packs without tearing down a live chart or book. */
export async function refreshSnapshot(): Promise<void> {
  if (state.selected !== null && inTauri()) await readSnapshot(state.selected);
}

/**
 * Reads bars for the selected symbol at the selected interval.
 *
 * Its own read on its own trigger, like the snapshot and for the same reason:
 * a chart of hourly bars does not change between hours, and putting it on the
 * rail's tick would spend a venue read a minute redrawing an identical frame.
 */
export async function refreshChart(): Promise<void> {
  if (state.selected === null || !inTauri()) return;
  const symbol = state.selected;
  const interval = state.interval;
  const network = shell.network;
  try {
    const series = await fetchChartSeries(network, symbol, interval);
    // The selection may have moved while three venue reads were in flight.
    // Landing stale bars under a different symbol's header would be the worst
    // kind of wrong: it looks right.
    if (state.selected !== symbol || state.interval !== interval || shell.network !== network) return;
    state.chart = {
      closed: series.closed.map(parseBar).filter((bar): bar is Bar => bar !== null),
      forming: series.forming ? parseBar(series.forming) : null,
      intervalMs: series.interval_ms,
      priceDecimals: series.price_decimals,
    };
    state.chartError = null;
  } catch (error) {
    if (state.selected === symbol && state.interval === interval && shell.network === network) state.chartError = reason(error);
  }
}

/** Switches the chart's interval and re-reads. */
export async function setInterval(interval: ChartInterval): Promise<void> {
  if (state.interval === interval) return;
  state.interval = interval;
  state.chart = null;
  // The candle subscription names its interval, so switching timeframes has
  // to move the socket too or the chart would stream the old bucket width.
  await watchSelected();
  await refreshChart();
}

// ---------------------------------------------------------------------------
// The live feed (`docs/spec.md` items 31, 34)
// ---------------------------------------------------------------------------

/**
 * Fold one socket frame into what the panels draw.
 *
 * **REST seeds, the socket moves.** Every panel is still filled by its own
 * read on selection — the socket carries no history, and a console that waited
 * for the first frame would open blank. From there this is what changes the
 * numbers, so the strip, the ladder and the forming bar age on the venue's
 * clock rather than on a poll.
 *
 * Frames for a symbol the operator has moved on from are dropped. The
 * unsubscribe is not instantaneous, so one more frame for the previous symbol
 * after a selection is ordinary — and drawing it under this symbol's header
 * would be the failure `select` already refuses for the chart.
 */
export function applyFeed(update: FeedUpdate): void {
  switch (update.kind) {
    case "ctx": {
      // The whole row, so the rail and the strip stay one value. Replaced in
      // place rather than re-sorted: the rail is ordered by day volume, and
      // re-sorting on a tick would make rows jump under the pointer.
      const at = state.rows.findIndex((row) => row.symbol === update.row.symbol);
      if (at === -1) return;
      state.rows[at] = update.row;
      return;
    }
    case "bbo": {
      if (update.coin !== state.selected || state.snapshot === null) return;
      // Top of book only. An absent side stays absent — §5.2 makes an empty
      // side stale, and splicing a zero in would quote a spread the venue
      // never showed.
      state.snapshot = {
        ...state.snapshot,
        bids: mergeTop(state.snapshot.bids, update.bid, "bid"),
        asks: mergeTop(state.snapshot.asks, update.ask, "ask"),
        as_of_ms: update.at_ms,
      };
      return;
    }
    case "book": {
      if (update.coin !== state.selected || state.snapshot === null) return;
      // The ladder is replaced whole. `book`, `funding` and `vol` are left as
      // the snapshot read them: they are derived packs, and recomputing them
      // from a depth frame here would be a second implementation of
      // `oppen_core::market::snapshot` living in TypeScript.
      state.snapshot = {
        ...state.snapshot,
        bids: update.bids,
        asks: update.asks,
        as_of_ms: update.at_ms,
      };
      return;
    }
    case "candle": {
      if (update.coin !== state.selected || update.interval !== state.interval) return;
      if (state.chart === null) return;
      const bar = parseBar({
        time_ms: update.time_ms,
        open: update.open,
        high: update.high,
        low: update.low,
        close: update.close,
        volume: update.volume,
      });
      if (bar === null) return;
      state.chart = foldForming(state.chart, bar);
      return;
    }
    case "trade": {
      if (update.coin !== state.selected || state.chart === null) return;
      const px = num(update.px);
      const sz = num(update.sz);
      const high = num(update.high);
      const low = num(update.low);
      if (px === null || sz === null || high === null || low === null) return;
      state.chart = foldTrade(state.chart, { px, high, low, sz }, update.at_ms);
      return;
    }
    case "status":
      return;
  }
}

/**
 * Replace the touch of one side, or leave it exactly as it was.
 *
 * Exported as a test seam (`AGENTS.md` leanness rule 2). The absent case is
 * the whole point: `docs/specs/fair-value.md` §5.2 makes an empty side stale,
 * never zero, so a `bbo` frame with no ask must leave the ask ladder alone
 * rather than splice a hole into it — a spread computed off that hole is a
 * number the venue never quoted.
 */
export function mergeTop(levels: BookLevel[], top: BookLevel | undefined, side: "bid" | "ask"): BookLevel[] {
  if (top === undefined) return levels;
  // A worsened touch invalidates deeper cached levels that now outrank it.
  // Keep exact wire strings; numeric comparison only orders the displayed ladder.
  const price = Number(top.px);
  return [top, ...levels.slice(1).filter(level => side === "bid" ? Number(level.px) < price : Number(level.px) > price)];
}

/**
 * Fold a live bar into the chart.
 *
 * Exported as a test seam for the same reason. A frame whose bucket has moved
 * past the forming bar *closes* that bar and starts a new one, which is how
 * the chart grows between REST reads; a frame in the same bucket replaces it
 * in place. Getting this backwards would either duplicate every bar or freeze
 * the chart one bucket behind the market.
 */
export function foldForming(chart: ChartData, bar: Bar): ChartData {
  const forming = chart.forming;
  const rolled = forming !== null && forming.time < bar.time;
  return {
    ...chart,
    closed: rolled ? [...chart.closed, forming] : chart.closed,
    forming: bar,
  };
}

/**
 * Fold one print into the bar that is forming.
 *
 * **This is what makes the chart move.** The venue's own `candle` channel is
 * the reconcile, not the drive: measured on testnet BTC it delivered eight
 * frames in a minute with a seventeen-second tail carrying none, so a chart
 * waiting on it sits still through prints the operator can watch on the tape.
 * Each print extends the bar optimistically here, and `foldForming` overwrites
 * that bar with the venue's own OHLCV when the venue gets round to sending it.
 *
 * A print past the bucket boundary closes the bar and opens the next one at
 * the print's own price, so a chart left open across a boundary does not draw
 * one bar twice as wide as the rest.
 */
export function foldTrade(chart: ChartData, frame: TradeFrame, atMs: number): ChartData {
  const bucket = Math.floor(atMs / chart.intervalMs) * chart.intervalMs;
  const forming = chart.forming;
  if (forming === null || bucket > forming.time) {
    return {
      ...chart,
      closed: forming !== null ? [...chart.closed, forming] : chart.closed,
      // The frame's own extremes, not its close: a batch carrying a spike
      // opens a bar that already reaches it.
      forming: {
        time: bucket,
        open: frame.px,
        high: frame.high,
        low: frame.low,
        close: frame.px,
        volume: frame.sz,
      },
    };
  }
  // A print older than the bar being drawn is dropped rather than folded
  // backwards: the venue batches the tape, and a late frame must not reopen a
  // bar the chart has already moved past.
  if (bucket < forming.time) return chart;
  return {
    ...chart,
    forming: {
      ...forming,
      high: Math.max(forming.high, frame.high),
      low: Math.min(forming.low, frame.low),
      close: frame.px,
      volume: forming.volume + frame.sz,
    },
  };
}

/** One batch off the tape, as the bar consumes it. */
export interface TradeFrame {
  /** The last print: the close. */
  px: number;
  high: number;
  low: number;
  /** Every print in the batch, summed. */
  sz: number;
}

/**
 * Point the socket at the selected symbol.
 *
 * Failure is recorded on the chart's own channel and nothing else is torn
 * down: losing the socket costs the panels their live updates, not the values
 * the REST reads already put there.
 */
export async function watchSelected(): Promise<void> {
  if (state.selected === null || !inTauri()) return;
  try {
    await watchMarket(shell.network, state.selected, state.interval);
  } catch (error) {
    state.error = reason(error);
  }
}

/**
 * How often the rail re-reads the universe.
 *
 * The rail lists every listed perp and no socket channel answers for all of
 * them at once — `activeAssetCtx` is per coin, and subscribing two hundred of
 * them to keep a list ordered would spend the venue's per-IP subscription
 * budget on rows nobody is looking at. So the rail polls, and the symbol the
 * operator selected is the one that streams.
 */
const RAIL_REFRESH_MS = 10_000;

/** How often the staleness overlay re-reads the clock (item 34). */
const AGE_TICK_MS = 1_000;

let rail: ReturnType<typeof globalThis.setInterval> | null = null;
let age: ReturnType<typeof globalThis.setInterval> | null = null;
let unlisten: (() => void) | null = null;

/**
 * Start the console's live data (`docs/spec.md` items 31, 34).
 *
 * Three clocks, deliberately not one: the socket for the selected symbol, a
 * poll for the rail that no socket can serve, and a timer that ages the
 * staleness overlay because a feed going quiet announces nothing. Idempotent,
 * so a remount does not stack listeners or timers.
 */
export async function startMarketFeed(): Promise<void> {
  if (rail !== null) return;
  void refreshMarkets();
  rail = globalThis.setInterval(() => void refreshMarkets(), RAIL_REFRESH_MS);
  age = globalThis.setInterval(() => ageFeeds(Date.now()), AGE_TICK_MS);
  unlisten = await onFeedUpdate((update) => {
    applyFeed(update);
    if (update.kind === "status") {
      feedStatus(update.connected, update.detail);
      return;
    }
    // Every payload frame is evidence the socket is alive, and it carries the
    // venue's own instant where it has one — which is what item 34 wants the
    // overlay to age against rather than the moment the renderer woke up.
    feedTick("at_ms" in update ? update.at_ms : Date.now());
  });
}

/** Stop everything `startMarketFeed` started. */
export function stopMarketFeed(): void {
  if (rail !== null) globalThis.clearInterval(rail);
  if (age !== null) globalThis.clearInterval(age);
  unlisten?.();
  rail = null;
  age = null;
  unlisten = null;
}
