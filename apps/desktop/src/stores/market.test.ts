/**
 * Tests for the market store's one lossy boundary.
 *
 * Everything else in this store is plumbing over the bridge, but `parseBar` is
 * where an exact decimal from the venue becomes a float for the renderer. That
 * conversion is safe *only* because its output is mapped to character cells
 * and never to an order, and the two things worth pinning are that it drops
 * what it cannot read rather than defaulting it, and that it keeps the
 * timestamp intact.
 *
 * Same `describe` / `it` / `expect` shims as `lib/candles.test.ts`, so this
 * runs unchanged under `bun test` and keeps `vue-tsc --noEmit` green.
 */

import { foldForming, foldTrade, mergeTop, parseBar } from "./market";

interface Assertions {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
}

interface Matchers extends Assertions {
  not: Assertions;
}

declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void) => void;
declare const expect: (actual: unknown) => Matchers;

const GOOD = {
  time_ms: 1_788_544_667_000,
  open: "100.5",
  high: "110",
  low: "90.25",
  close: "101.75",
  volume: "7.5",
};

describe("parseBar", () => {
  it("carries every field across, timestamp included", () => {
    expect(parseBar(GOOD)).toEqual({
      time: 1_788_544_667_000,
      open: 100.5,
      high: 110,
      low: 90.25,
      close: 101.75,
      volume: 7.5,
    });
  });

  /**
   * The failure this exists to prevent. `Number("")` is `0` and
   * `Number(undefined)` is `NaN`, so a boundary that trusted the cast would
   * draw a candle at the floor of the chart — a price that looks deliberate
   * and never happened. Dropping the bar hands the renderer a shorter window,
   * which it reports rather than invents.
   */
  it("drops a bar it cannot read rather than defaulting it to zero", () => {
    expect(parseBar({ ...GOOD, low: "" })).toBe(null);
    expect(parseBar({ ...GOOD, close: "not a price" })).toBe(null);
    expect(parseBar({ ...GOOD, volume: "Infinity" })).toBe(null);
    expect(parseBar({ ...GOOD, high: "   " })).toBe(null);
  });

  /**
   * A zero is a real reading and must survive: a bucket in which nothing
   * traded has zero volume, and treating that as unreadable would drop bars
   * from every quiet market.
   */
  it("keeps a genuine zero", () => {
    const quiet = parseBar({ ...GOOD, volume: "0" });
    expect(quiet === null).toBe(false);
    expect(quiet?.volume).toBe(0);
  });
});

/**
 * The live feed's two decisions (`docs/spec.md` items 31, 34).
 *
 * `applyFeed` itself is dispatch over module state, but these two are where a
 * frame can be folded *wrong* rather than merely dropped, so they are the two
 * the socket path pins.
 */
describe("mergeTop", () => {
  it("removes duplicate and outranked cached levels when the touch worsens", () => {
    const bids = [{ px: "100", sz: "1", n: 1 }, { px: "99", sz: "2", n: 1 }, { px: "98", sz: "3", n: 1 }];
    expect(mergeTop(bids, { px: "99", sz: "4", n: 1 }, "bid")).toEqual([
      { px: "99", sz: "4", n: 1 }, { px: "98", sz: "3", n: 1 },
    ]);
    const asks = [{ px: "100", sz: "1", n: 1 }, { px: "101", sz: "2", n: 1 }, { px: "102", sz: "3", n: 1 }];
    expect(mergeTop(asks, { px: "102", sz: "4", n: 1 }, "ask")).toEqual([{ px: "102", sz: "4", n: 1 }]);
  });

  const LEVELS = [
    { px: "100", sz: "1", n: 1 },
    { px: "99", sz: "2", n: 2 },
  ];

  it("replaces the touch and leaves the depth behind it", () => {
    expect(mergeTop(LEVELS, { px: "100.5", sz: "3", n: 1 }, "bid")).toEqual([
      { px: "100.5", sz: "3", n: 1 },
      { px: "99", sz: "2", n: 2 },
    ]);
  });

  it("leaves an empty side exactly as it was rather than zeroing it", () => {
    // §5.2: an absent side is stale, never zero. Splicing a hole in here would
    // let the strip quote a spread against a price the venue never showed.
    expect(mergeTop(LEVELS, undefined, "bid")).toEqual(LEVELS);
  });
});

describe("foldForming", () => {
  const BAR = { time: 1_000, open: 1, high: 2, low: 0.5, close: 1.5, volume: 10 };
  const CHART = { closed: [], forming: BAR, intervalMs: 60_000, priceDecimals: 2 };

  it("replaces the forming bar when the frame is the same bucket", () => {
    const next = foldForming(CHART, { ...BAR, close: 1.9 });
    expect(next.closed.length).toBe(0);
    expect(next.forming?.close).toBe(1.9);
  });

  it("closes the forming bar when the bucket has rolled", () => {
    const next = foldForming(CHART, { ...BAR, time: 61_000, close: 2.1 });
    // The bar that was forming is now history, and the new one takes its
    // place — this is what makes the chart grow without a REST read.
    expect(next.closed.length).toBe(1);
    expect(next.closed[0]?.time).toBe(1_000);
    expect(next.forming?.time).toBe(61_000);
  });

  it("never drops the bar that was forming", () => {
    const next = foldForming({ ...CHART, closed: [BAR] }, { ...BAR, time: 61_000 });
    expect(next.closed.length).toBe(2);
  });
});

/**
 * The forming bar is driven by the tape, corrected by the venue.
 *
 * Measured on testnet BTC: the `candle` channel delivered 8 frames in a minute
 * with a 17-second tail carrying none, while the tape kept printing. A chart
 * driven by the venue's aggregation alone sits still through those prints, so
 * the tape drives and `foldForming` reconciles.
 */
describe("foldTrade", () => {
  const CHART = { closed: [], forming: null, intervalMs: 60_000, priceDecimals: 2 };

  it("opens a bar at the print's own price when none is forming", () => {
    const next = foldTrade(CHART, { px: 100, high: 100, low: 100, sz: 2 }, 61_000);
    // Bucket start, not the print instant: the bar belongs to its interval.
    expect(next.forming?.time).toBe(60_000);
    expect(next.forming?.open).toBe(100);
    expect(next.forming?.close).toBe(100);
    expect(next.forming?.volume).toBe(2);
  });

  it("extends the range and accumulates volume within the bucket", () => {
    const opened = foldTrade(CHART, { px: 100, high: 100, low: 100, sz: 2 }, 61_000);
    const high = foldTrade(opened, { px: 105, high: 105, low: 105, sz: 1 }, 62_000);
    const low = foldTrade(high, { px: 95, high: 95, low: 95, sz: 3 }, 63_000);
    expect(low.forming?.high).toBe(105);
    expect(low.forming?.low).toBe(95);
    // The close is the latest print, not the extreme.
    expect(low.forming?.close).toBe(95);
    expect(low.forming?.volume).toBe(6);
    expect(low.closed.length).toBe(0);
  });

  it("closes the bar and opens the next one across a bucket boundary", () => {
    const opened = foldTrade(CHART, { px: 100, high: 100, low: 100, sz: 2 }, 61_000);
    const rolled = foldTrade(opened, { px: 110, high: 110, low: 110, sz: 1 }, 121_000);
    expect(rolled.closed.length).toBe(1);
    expect(rolled.closed[0]?.time).toBe(60_000);
    expect(rolled.forming?.time).toBe(120_000);
    // The new bar opens at the print, not at the previous close.
    expect(rolled.forming?.open).toBe(110);
    expect(rolled.forming?.volume).toBe(1);
  });

  it("drops a print older than the bar being drawn", () => {
    const opened = foldTrade(CHART, { px: 100, high: 100, low: 100, sz: 2 }, 121_000);
    // A late frame must not reopen a bucket the chart has moved past, or the
    // bar would take a price that belongs to history.
    expect(foldTrade(opened, { px: 50, high: 50, low: 50, sz: 9 }, 61_000)).toEqual(opened);
  });

  it("lets the venue's own bar overwrite what the tape composed", () => {
    const composed = foldTrade(CHART, { px: 100, high: 100, low: 100, sz: 2 }, 61_000);
    const venue = { time: 60_000, open: 99, high: 106, low: 94, close: 101, volume: 12 };
    const reconciled = foldForming(composed, venue);
    // Same bucket, so the optimistic bar is replaced rather than closed: the
    // venue's OHLCV is authoritative and the tape's was an estimate.
    expect(reconciled.closed.length).toBe(0);
    expect(reconciled.forming).toEqual(venue);
  });
});

/**
 * A batched frame carries extremes its close does not show.
 *
 * The venue batches the tape, so one frame can hold twenty prints. A bar built
 * from the last print alone misses the high the market actually traded, and
 * stays wrong until the venue's next candle frame corrects it — which W2
 * measured at up to seventeen seconds away.
 */
describe("foldTrade · batched frames", () => {
  const CHART = { closed: [], forming: null, intervalMs: 60_000, priceDecimals: 2 };

  it("opens a bar reaching the frame's extremes, not just its close", () => {
    const next = foldTrade(CHART, { px: 100, high: 120, low: 90, sz: 5 }, 61_000);
    expect(next.forming?.high).toBe(120);
    expect(next.forming?.low).toBe(90);
    expect(next.forming?.close).toBe(100);
  });

  it("widens an open bar to a spike that the close does not show", () => {
    const opened = foldTrade(CHART, { px: 100, high: 100, low: 100, sz: 1 }, 61_000);
    const spiked = foldTrade(opened, { px: 101, high: 150, low: 80, sz: 4 }, 62_000);
    expect(spiked.forming?.high).toBe(150);
    expect(spiked.forming?.low).toBe(80);
    // The close is still the last print, not the extreme.
    expect(spiked.forming?.close).toBe(101);
    expect(spiked.forming?.volume).toBe(5);
  });
});
