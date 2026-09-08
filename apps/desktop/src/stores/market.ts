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

import { computed, reactive, readonly, watch } from "vue";

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
  type FeedBinding,
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

interface QuoteClock {
  source: "bbo" | "book" | "rest";
  venueMs: number | null;
  observedMs: number;
  live: boolean;
}

/** Separate observations: a touch never invents a depth snapshot. */
export function createQuotes() {
  const quotes = reactive<{
    touch: (QuoteClock & { bid: BookLevel | null; ask: BookLevel | null }) | null;
    depth: (QuoteClock & { bids: BookLevel[]; asks: BookLevel[] }) | null;
  }>({ touch: null, depth: null });
  let revision = 0;
  let liveOwned = false;
  let depthObservation: { bids: BookLevel[]; asks: BookLevel[] } | null = null;
  function invalidate() {
    revision++;
    if (quotes.touch) quotes.touch.live = false;
    if (quotes.depth) quotes.depth.live = false;
  }
  function reset() { invalidate(); quotes.touch = null; quotes.depth = null; depthObservation = null; liveOwned = false; }
  function seed(snapshot: MarketSnapshot, requestedRevision: number, observedMs: number) {
    if (liveOwned || revision !== requestedRevision) return;
    const clock = { source: "rest" as const, venueMs: null, observedMs, live: false };
    quotes.touch = { ...clock, bid: snapshot.bids[0] ?? null, ask: snapshot.asks[0] ?? null };
    quotes.depth = { ...clock, bids: snapshot.bids, asks: snapshot.asks };
  }
  function update(frame: Extract<FeedUpdate, { kind: "book" | "bbo" }>, observedMs: number) {
    if (!Number.isSafeInteger(frame.at_ms) || frame.at_ms < 0) return;
    liveOwned = true;
    revision++;
    const clock = { source: frame.kind, venueMs: frame.at_ms, observedMs, live: true };
    const old = quotes.touch;
    const bid = (frame.kind === "bbo" ? frame.bid : frame.bids[0]) ?? null;
    const ask = (frame.kind === "bbo" ? frame.ask : frame.asks[0]) ?? null;
    const sameLevel = (a: BookLevel | null, b: BookLevel | null) => a === null || b === null
      ? a === b : a.px === b.px && a.sz === b.sz && a.n === b.n;
    const repeatTouch = old?.source === frame.kind && old.venueMs === frame.at_ms
      && sameLevel(old.bid, bid) && sameLevel(old.ask, ask);
    if (old?.venueMs == null || frame.at_ms > old.venueMs
      || repeatTouch || (frame.at_ms === old.venueMs && frame.kind === "bbo" && old.source !== "bbo")) {
      quotes.touch = { ...clock, bid, ask };
      if (frame.kind === "bbo" && quotes.depth) {
        quotes.depth.live = false;
        if (!frame.bid) quotes.depth.bids = [];
        if (!frame.ask) quotes.depth.asks = [];
      }
    }
    if (frame.kind === "book") {
      const newerTouch = quotes.touch?.source === "bbo" && (quotes.touch.venueMs ?? 0) >= frame.at_ms;
      const bids = newerTouch && !quotes.touch?.bid ? [] : frame.bids;
      const asks = newerTouch && !quotes.touch?.ask ? [] : frame.asks;
      const sameLevels = (a: BookLevel[], b: BookLevel[]) => a.length === b.length
        && a.every((level, i) => sameLevel(level, b[i]!));
      const depth = quotes.depth;
      if (depth?.venueMs == null || frame.at_ms > depth.venueMs
        || (frame.at_ms === depth.venueMs && depthObservation !== null
          && sameLevels(depthObservation.bids, frame.bids) && sameLevels(depthObservation.asks, frame.asks))) {
        quotes.depth = { ...clock, live: !newerTouch, bids, asks };
        depthObservation = { bids: frame.bids, asks: frame.asks };
      }
    }
  }
  return { quotes: readonly(quotes), invalidate, reset, seed, update, get revision() { return revision; } };
}

const quoteState = createQuotes();
export const quotes = quoteState.quotes;

/** Approximation for display only; exact wire prices remain untouched. */
export function quoteSpread(bid: string | null | undefined, ask: string | null | undefined): number | null {
  const valid = (px: string | null | undefined) => px != null && /^\d+(\.\d+)?$/.test(px) && Number.isFinite(Number(px)) && Number(px) > 0;
  if (!valid(bid) || !valid(ask)) return null;
  const b = Number(bid), a = Number(ask);
  if (b > a) return null;
  const spread = ((a - b) / (a / 2 + b / 2)) * 10_000;
  return Number.isFinite(spread) ? spread : null;
}

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
  const quoteRevision = quoteState.revision;
  try {
    const snapshot = await fetchMarketSnapshot(network, symbol);
    if (request !== snapshotRequest || state.selected !== symbol || shell.network !== network) return;
    state.snapshot = snapshot;
    quoteState.seed(snapshot, quoteRevision, Date.now());
    state.featuresReadMs = snapshot.as_of_ms;
    state.snapshotError = null;
  } catch (error) {
    if (request === snapshotRequest && state.selected === symbol && shell.network === network) state.snapshotError = reason(error);
  }
}

export async function select(symbol: string): Promise<void> {
  state.selected = symbol;
  quoteState.reset();
  state.snapshot = null;
  state.featuresReadMs = null;
  state.snapshotError = null;
  state.chart = null;
  state.chartError = null;
  snapshotRequest += 1;
  const selection = snapshotRequest;
  if (!inTauri()) return;
  await watchSelected();
  if (state.selected !== symbol || selection !== snapshotRequest) return;
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
      if (update.coin !== state.selected) return;
      quoteState.update(update, Date.now());
      return;
    }
    case "book": {
      if (update.coin !== state.selected) return;
      quoteState.update(update, Date.now());
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
      if (!update.connected) quoteState.invalidate();
      return;
  }
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
interface FeedScope {
  network: FeedBinding["network"];
  coin: string | null;
  interval: string;
}

/** Retained IPC ownership and listener lifecycle; injectable for deferred transport proofs. */
export function createMarketFeed(
  scope: () => FeedScope,
  transport: { watch: typeof watchMarket; listen: typeof onFeedUpdate },
  callbacks: {
    update: (update: FeedUpdate, failure?: string) => void;
    invalidate: () => void;
    failure: (detail: string) => void;
    error: (detail: string | null) => void;
  },
) {
  let active = false;
  let lifecycle = 0;
  let desired: FeedScope | null = null;
  let accepted: { binding: FeedBinding; request: FeedScope } | null = null;
  let failedOwner: (FeedBinding & { failure: string }) | null = null;
  let running: Promise<void> | null = null;
  let completed: FeedScope | null = null;
  let unlisten: (() => void) | null = null;
  let unwatch: (() => void) | null = null;

  function invalidate(): void {
    accepted = null;
    callbacks.invalidate();
    callbacks.error(null);
  }

  async function drain(): Promise<void> {
    while (active && desired !== null) {
      const request = desired;
      try {
        const binding = await transport.watch(request.network, request.coin!, request.interval);
        if (active && desired === request) {
          if (binding?.network !== request.network || typeof binding.generation !== "string" || binding.generation.length === 0) {
            throw new Error("Market feed acknowledgment does not match the requested scope.");
          }
          accepted = { binding, request };
          if (failedOwner?.network !== binding.network || failedOwner.generation !== binding.generation) failedOwner = null;
          callbacks.error(null);
        }
      } catch (error) {
        if (active && desired === request) callbacks.error(reason(error));
      }
      completed = request;
      if (desired === request) break;
    }
  }

  function request(): Promise<void> {
    if (!active) return Promise.resolve();
    const next = scope();
    if (desired?.network === next.network && desired.coin === next.coin && desired.interval === next.interval
      && (running !== null || accepted !== null)) return running ?? Promise.resolve();
    invalidate();
    desired = next.coin === null ? null : next;
    // Never clear this slot on stop: an IPC already sent still owns backend work.
    if (running === null && desired !== null) {
      running = drain().finally(() => {
        running = null;
        if (active && desired !== null && completed !== desired) void request();
      });
    }
    return running ?? Promise.resolve();
  }

  async function start(): Promise<void> {
    if (active) return;
    active = true;
    const owner = ++lifecycle;
    unwatch = watch(scope, () => { void request(); }, { flush: "sync", immediate: true });
    try {
      const cleanup = await transport.listen((envelope) => {
        if (!active || owner !== lifecycle || accepted === null || accepted.request !== desired) return;
        if (envelope.network !== accepted.binding.network || envelope.generation !== accepted.binding.generation) return;
        const update = envelope.update;
        if (envelope.failure !== undefined && failedOwner === null) {
          failedOwner = { ...accepted.binding, failure: envelope.failure };
          callbacks.failure(failedOwner.failure);
        }
        if (update.kind === "ctx" && update.row.symbol !== desired?.coin) return;
        if ("coin" in update && update.coin !== desired?.coin) return;
        if (update.kind === "candle" && update.interval !== desired?.interval) return;
        callbacks.update(update, failedOwner?.failure);
      });
      if (!active || owner !== lifecycle) cleanup();
      else unlisten = cleanup;
    } catch (error) {
      if (active && owner === lifecycle) callbacks.error(reason(error));
    }
  }

  function stop(): void {
    active = false;
    lifecycle += 1;
    unwatch?.();
    unwatch = null;
    desired = null;
    invalidate();
    unlisten?.();
    unlisten = null;
  }

  return { start, stop, request };
}

let watchError: string | null = null;
const liveFeed = createMarketFeed(
  () => ({ network: shell.network, coin: state.selected, interval: state.interval }),
  { watch: watchMarket, listen: onFeedUpdate },
  {
    invalidate: () => { quoteState.invalidate(); feedStatus(false, "Awaiting selected market feed."); },
    failure: (detail) => { quoteState.invalidate(); feedStatus(false, detail); },
    error: (detail) => {
      if (detail !== null || state.error === watchError) state.error = detail;
      watchError = detail;
    },
    update: (update, failure) => {
      applyFeed(update);
      if (failure !== undefined) { quoteState.invalidate(); feedStatus(false, failure); }
      else if (update.kind === "status") feedStatus(update.connected, update.detail);
      else feedTick("at_ms" in update ? update.at_ms : Date.now());
    },
  },
);

export async function watchSelected(): Promise<void> {
  if (inTauri()) await liveFeed.request();
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
  if (inTauri()) await liveFeed.start();
}

/** Stop everything `startMarketFeed` started. */
export function stopMarketFeed(): void {
  if (rail !== null) globalThis.clearInterval(rail);
  if (age !== null) globalThis.clearInterval(age);
  liveFeed.stop();
  rail = null;
  age = null;
}
