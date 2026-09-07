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

import {
  fetchMarketSnapshot,
  fetchMarkets,
  inTauri,
  isConsoleError,
  type MarketRow,
  type MarketSnapshot,
} from "../lib/bridge";
import { shell } from "./shell";

interface MarketState {
  rows: MarketRow[];
  /** `null` until the operator picks one, or the rail's busiest arrives. */
  selected: string | null;
  snapshot: MarketSnapshot | null;
  /** Why the last rail read failed, shown beside the stale rows. */
  error: string | null;
  /** Why the last snapshot read failed. Kept apart: the two fail separately. */
  snapshotError: string | null;
}

const state = reactive<MarketState>({
  rows: [],
  selected: null,
  snapshot: null,
  error: null,
  snapshotError: null,
});

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
  if (!inTauri()) return;
  try {
    state.snapshot = await fetchMarketSnapshot(shell.network, symbol);
    state.snapshotError = null;
  } catch (error) {
    state.snapshotError = reason(error);
  }
}

/** Re-reads the selected symbol. Bound to the panel's own refresh. */
export async function refreshSnapshot(): Promise<void> {
  if (state.selected !== null) await select(state.selected);
}
