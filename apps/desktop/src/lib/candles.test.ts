/**
 * Tests for the ASCII candle renderer.
 *
 * The acceptance gate in `specs/charts.md` §6 is a snapshot: a fixed bar array and a
 * fixed grid size must give a byte-identical frame on every run and every machine. The
 * golden below is that snapshot; the rest of the file pins the geometry rules the site
 * port must not lose and the axis rules `specs/charts.md` §2.2 adds on top of it.
 *
 * There is no test runner in `apps/desktop/package.json` yet. The file is written
 * against the standard `describe` / `it` / `expect` globals so it runs unchanged under
 * vitest with `globals: true`; the declarations below are what keep
 * `vue-tsc --noEmit` green until that runner is installed.
 */

import {
  createCandleRenderer,
  frameRuns,
  INK,
  niceTickStep,
  priceTicks,
  renderCandles,
  type Bar,
  type CandleFrame,
} from "./candles";

interface Matchers {
  toBe(expected: unknown): void;
  toEqual(expected: unknown): void;
  toBeGreaterThanOrEqual(expected: number): void;
  toBeLessThanOrEqual(expected: number): void;
  toContain(expected: string): void;
}

declare const describe: (name: string, body: () => void) => void;
declare const it: (name: string, body: () => void) => void;
declare const expect: (actual: unknown) => Matchers;

const HOUR = 3_600_000;
const MINUTE = 60_000;
/** 2026-09-02 21:00 UTC — three bars before a UTC day boundary, so the day rule is exercised. */
const START = Date.UTC(2026, 8, 2, 21, 0, 0);

const CLOSED: Bar[] = [
  { time: START + 0 * HOUR, open: 100, high: 106, low: 99, close: 105, volume: 10 },
  { time: START + 1 * HOUR, open: 105, high: 108, low: 104, close: 104.5, volume: 40 },
  { time: START + 2 * HOUR, open: 104.5, high: 110, low: 104, close: 109, volume: 25 },
  { time: START + 3 * HOUR, open: 109, high: 109.5, low: 101, close: 102, volume: 8 },
  { time: START + 4 * HOUR, open: 102, high: 103, low: 98, close: 98.5, volume: 30 },
];
const FORMING: Bar = {
  time: START + 5 * HOUR,
  open: 98.5,
  high: 102,
  low: 98,
  close: 101.5,
  volume: 12,
};

function fixture() {
  return {
    closed: CLOSED,
    forming: FORMING,
    cols: 8,
    rows: 12,
    intervalMs: HOUR,
    priceDecimals: 1,
    tzOffsetMinutes: 0,
  };
}

const GOLDEN_TEXT = [
  "· · · · · · · · ·│·│·│· · · · ·    110",
  "· · · · · · ·│· +╥+│:╥: · · · ·    108",
  "             │  +++│:::               ",
  "· · · · +++ :╥: +++│::: · · · ·    106",
  "· · · · +++ ::: +++│::: · · · ·    104",
  "· · · · +++ · · · ·│:╨: ::: ·│·    102",
  "        +++        │ │  ::: ▓╥▓◄ 101.5",
  "· · · · +++ · · · ·│· · ::: ▓▓▓    100",
  "· · · · · · · · · ·│· · ::: ▓▓▓     98",
  "─────────┴───────────┴───────┴──      ",
  "        ▁▁▁ ▂▂▂ ▂▂▂ ▁▁▁ ▂▂▂ ▁▁▁       ",
  "       21:00       00:00   02:00      ",
];

const GOLDEN_INK = [
  "r r r r r r r r rurrrdr r r r r    lll",
  "r r r r r r rdr uuurddd r r r r    lll",
  "             d  uuurddd               ",
  "r r r r uuu ddd uuurddd r r r r    lll",
  "r r r r uuu ddd uuurddd r r r r    lll",
  "r r r r uuu r r r rrddd ddd rur    lll",
  "        uuu        r d  ddd uuuk kkkkk",
  "r r r r uuu r r r rrr r ddd uuu    lll",
  "r r r r r r r r r rrr r ddd uuu     ll",
  "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr      ",
  "        vvv vvv vvv vvv vvv vvv       ",
  "       lllll       lllll   lllll      ",
];

/** Every cell whose ink is a candle's, i.e. a body, a wick or a cap. */
function candleCells(frame: CandleFrame): Array<{ x: number; y: number; char: string }> {
  const out: Array<{ x: number; y: number; char: string }> = [];
  for (let y = 0; y < frame.height; y += 1) {
    for (let x = 0; x < frame.width; x += 1) {
      const ink = frame.ink[y][x];
      if (ink === INK.up || ink === INK.down) out.push({ x, y, char: frame.text[y][x] });
    }
  }
  return out;
}

describe("renderCandles", () => {
  it("is byte-identical across runs, which is the specs/charts.md §6 gate", () => {
    const a = renderCandles(fixture());
    const b = renderCandles(fixture());
    expect(a.text).toEqual(b.text);
    expect(a.ink).toEqual(b.ink);
  });

  it("matches the golden frame", () => {
    const frame = renderCandles(fixture());
    expect(frame.width).toBe(38);
    expect(frame.height).toBe(12);
    expect(frame.text).toEqual(GOLDEN_TEXT);
    expect(frame.ink).toEqual(GOLDEN_INK);
  });

  it("keeps every row the same width so two frames diff row by row", () => {
    const frame = renderCandles(fixture());
    for (let y = 0; y < frame.height; y += 1) {
      expect(frame.text[y].length).toBe(frame.width);
      expect(frame.ink[y].length).toBe(frame.width);
    }
  });

  it("holds the four-column pitch: three body columns and one gap", () => {
    const frame = renderCandles(fixture());
    // Column 11 is the gap after the leftmost candle's body at 8, 9, 10.
    for (let y = 0; y < 9; y += 1) {
      const ink = frame.ink[y][11];
      expect(ink === INK.up || ink === INK.down).toBe(false);
    }
    for (const y of [3, 4, 5, 6, 7]) {
      expect(frame.text[y].slice(8, 11)).toBe("+++");
    }
  });

  it("encodes direction twice, by glyph and by ink", () => {
    const frame = renderCandles(fixture());
    const bodies = candleCells(frame).filter((c) => c.char === "+" || c.char === ":");
    for (const cell of bodies) {
      expect(frame.ink[cell.y][cell.x]).toBe(cell.char === "+" ? INK.up : INK.down);
    }
    expect(frame.text.join("")).toContain("+");
    expect(frame.text.join("")).toContain(":");
  });

  it("draws a cap only where the body stops short of the wick end", () => {
    const frame = renderCandles(fixture());
    // Bar 2 (x = 16) closes at 109 with a high of 110: a top cap, no bottom cap.
    expect(frame.text[1].slice(16, 19)).toBe("+╥+");
    // The leftmost bar's body spans its whole range, so it gets neither cap.
    const leftColumn = frame.text.map((row) => row[9]).join("");
    expect(leftColumn.indexOf("╥")).toBe(-1);
    expect(leftColumn.indexOf("╨")).toBe(-1);
  });

  it("never lets a gridline overwrite a body, a wick or a cap", () => {
    const frame = renderCandles(fixture());
    for (const cell of candleCells(frame)) {
      expect(cell.char).toBe(frame.text[cell.y][cell.x]);
      expect(cell.char === "·").toBe(false);
    }
  });

  it("puts the whole volume row on one ink, never on direction", () => {
    const frame = renderCandles(fixture());
    const row = frame.text[10];
    const ink = frame.ink[10];
    for (let x = 0; x < frame.width; x += 1) {
      if (row[x] === " ") continue;
      expect(ink[x]).toBe(INK.volume);
      expect(row[x] === "▁" || row[x] === "▂").toBe(true);
    }
  });

  it("rules a full-height day boundary in a gap column", () => {
    const frame = renderCandles(fixture());
    // 00:00 opens the bar at x = 20, so its rule sits in the gap at column 19.
    for (let y = 0; y < 9; y += 1) {
      expect(frame.text[y][19]).toBe("│");
      expect(frame.ink[y][19]).toBe(INK.rule);
    }
  });

  it("always draws the first and last time label and never overlaps labels", () => {
    const frame = renderCandles(fixture());
    const axis = frame.text[frame.height - 1];
    expect(axis).toContain("21:00");
    expect(axis).toContain("02:00");
    for (const match of axis.split(/\s+/).filter((s) => s.length > 0)) {
      expect(match.length).toBe(5);
    }
  });

  it("marks the last price in uranium at its own row", () => {
    const frame = renderCandles(fixture());
    const y = frame.text.findIndex((row) => row.indexOf("◄") >= 0);
    expect(y).toBe(6);
    expect(frame.ink[y].indexOf(INK.last) >= 0).toBe(true);
    expect(frame.text[y]).toContain("101.5");
  });

  it("labels the price axis at the asset's precision and never more", () => {
    const coarse = renderCandles({ ...fixture(), priceDecimals: 0 });
    const axis = coarse.text.map((row) => row.slice(coarse.width - 6)).join("|");
    expect(axis.indexOf(".")).toBe(-1);
  });

  it("sizes the gutter from the widest label rather than a constant", () => {
    const narrow = renderCandles(fixture());
    const wide = renderCandles({ ...fixture(), priceDecimals: 4 });
    expect(narrow.width).toBe(38);
    // 101.5000 is eight characters, three more than 101.5.
    expect(wide.width).toBe(41);
  });

  it("survives a grid too small to carry an axis", () => {
    const frame = renderCandles({ ...fixture(), rows: 3 });
    expect(frame.height).toBe(3);
    for (const row of frame.text) expect(row.trim()).toBe("");
  });

  it("survives an empty bar array and non-finite prices", () => {
    const empty = renderCandles({ ...fixture(), closed: [], forming: null });
    for (const row of empty.text) expect(row.trim()).toBe("");
    const dirty = renderCandles({
      ...fixture(),
      closed: [{ time: START, open: NaN, high: NaN, low: NaN, close: NaN, volume: NaN }],
      forming: null,
    });
    for (const row of dirty.text) expect(row.trim()).toBe("");
  });

  it("renders a sub-hour interval on minute boundaries", () => {
    const bars: Bar[] = [];
    for (let i = 0; i < 16; i += 1) {
      const price = 100 + (i % 5);
      bars.push({
        time: Date.UTC(2026, 8, 2, 9, 0, 0) + i * MINUTE,
        open: price,
        high: price + 1,
        low: price - 1,
        close: price + 0.5,
        volume: 1,
      });
    }
    const frame = renderCandles({
      closed: bars,
      forming: null,
      cols: 16,
      rows: 12,
      intervalMs: MINUTE,
      priceDecimals: 2,
      tzOffsetMinutes: 0,
    });
    const axis = frame.text[frame.height - 1];
    expect(axis).toContain("09:00");
    expect(axis).toContain("09:15");
  });

  it("reads time labels in the caller's offset, not the host's timezone", () => {
    const utc = renderCandles(fixture());
    const shifted = renderCandles({ ...fixture(), tzOffsetMinutes: 60 });
    expect(utc.text[utc.height - 1]).toContain("21:00");
    expect(shifted.text[shifted.height - 1]).toContain("22:00");
  });
});

describe("niceTickStep", () => {
  it("only ever returns a 1 / 2 / 2.5 / 5 × 10ⁿ step", () => {
    for (let i = 0; i < 500; i += 1) {
      const min = ((i * 7919) % 100000) / 100 + 0.01;
      const max = min + ((i * 104729) % 500000) / 1000 + 0.001;
      const step = niceTickStep(min, max);
      const exponent = Math.floor(Math.log10(step) + 1e-12);
      const mantissa = Number((step / Math.pow(10, exponent)).toFixed(3));
      expect([1, 2, 2.5, 5].indexOf(mantissa) >= 0).toBe(true);
    }
  });

  it("lands between four and eight gridlines in the visible range", () => {
    for (let i = 0; i < 500; i += 1) {
      const min = ((i * 7919) % 100000) / 100 + 0.01;
      const max = min + ((i * 104729) % 500000) / 1000 + 0.001;
      const count = priceTicks(min, max, niceTickStep(min, max)).length;
      expect(count).toBeGreaterThanOrEqual(4);
      expect(count).toBeLessThanOrEqual(8);
    }
  });

  it("does not divide by a zero span", () => {
    expect(niceTickStep(100, 100)).toBe(1);
    expect(priceTicks(100, 100, 0).length).toBe(0);
  });
});

describe("createCandleRenderer", () => {
  it("reports an unchanged frame so the caller can skip the DOM write", () => {
    const renderer = createCandleRenderer();
    expect(renderer.render(fixture()).changed).toBe(true);
    expect(renderer.render(fixture()).changed).toBe(false);
    const moved = { ...fixture(), forming: { ...FORMING, close: 99.5 } };
    expect(renderer.render(moved).changed).toBe(true);
  });

  it("produces the same frame as the allocating renderer", () => {
    const renderer = createCandleRenderer();
    const reused = renderer.render(fixture()).frame;
    const fresh = renderCandles(fixture());
    expect(reused.text).toEqual(fresh.text);
    expect(reused.ink).toEqual(fresh.ink);
  });
});

describe("frameRuns", () => {
  it("rebuilds every row exactly and never splits a run of one ink", () => {
    const frame = renderCandles(fixture());
    const rows = frameRuns(frame);
    expect(rows.length).toBe(frame.height);
    for (let y = 0; y < rows.length; y += 1) {
      const runs = rows[y];
      expect(runs.map((r) => r.text).join("")).toBe(frame.text[y]);
      for (let i = 1; i < runs.length; i += 1) {
        expect(runs[i].ink === runs[i - 1].ink).toBe(false);
      }
    }
  });
});
