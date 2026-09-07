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

import { parseBar } from "./market";

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
