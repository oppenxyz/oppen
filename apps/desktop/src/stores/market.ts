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
  type ChartBar,
  type MarketRow,
  type MarketSnapshot,
} from "../lib/bridge";
import { shell } from "./shell";

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
export async function select(symbol: string): Promise<void> {
  state.selected = symbol;
  state.snapshot = null;
  // Cleared, not left in place: bars from the previous symbol under this
  // symbol's header is a chart that lies rather than one that is missing.
  state.chart = null;
  if (!inTauri()) return;
  try {
    state.snapshot = await fetchMarketSnapshot(shell.network, symbol);
    state.snapshotError = null;
  } catch (error) {
    state.snapshotError = reason(error);
  }
  await refreshChart();
}

/** Re-reads the selected symbol. Bound to the panel's own refresh. */
export async function refreshSnapshot(): Promise<void> {
  if (state.selected !== null) await select(state.selected);
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
  try {
    const series = await fetchChartSeries(shell.network, symbol, interval);
    // The selection may have moved while three venue reads were in flight.
    // Landing stale bars under a different symbol's header would be the worst
    // kind of wrong: it looks right.
    if (state.selected !== symbol || state.interval !== interval) return;
    state.chart = {
      closed: series.closed.map(parseBar).filter((bar): bar is Bar => bar !== null),
      forming: series.forming ? parseBar(series.forming) : null,
      intervalMs: series.interval_ms,
      priceDecimals: series.price_decimals,
    };
    state.chartError = null;
  } catch (error) {
    state.chartError = reason(error);
  }
}

/** Switches the chart's interval and re-reads. */
export async function setInterval(interval: ChartInterval): Promise<void> {
  if (state.interval === interval) return;
  state.interval = interval;
  state.chart = null;
  await refreshChart();
}
