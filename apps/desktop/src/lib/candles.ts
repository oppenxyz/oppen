/**
 * ASCII candle renderer — the primary chart surface.
 *
 * WHY this exists and why it is a character grid: `decisions.md` U1 makes ASCII the
 * primary renderer and U2 drops the canvas fallback entirely, so this module is the
 * only chart renderer in the product. `specs/charts.md` §2.4 splits the work: the
 * core owns bar assembly, this module owns nothing but the scaling of a fixed bar
 * array onto a fixed grid. It holds no market state, keeps no timers, and does no
 * arithmetic beyond mapping prices to rows and picking axis ticks. That is what
 * makes the acceptance gate in §6 reachable — same bars plus same grid size gives a
 * byte-identical frame on every machine.
 *
 * Geometry is ported verbatim from the marketing site's `candles(hist, cols, rows,
 * per)` (`oppen Site.dc.html`), which `specs/charts.md` §2.1 declares correct: a
 * four-column candle pitch (three body columns, one gap), the wick as a box-drawing
 * vertical at `x + 1`, the body as three columns, cap glyphs drawn only where the
 * body stops short of the wick end, a volume row at the foot, and
 * `row(v) = round((1 − (v − min) / span) · (R − 1))`.
 *
 * Three things the site fakes and this module builds properly, per §2.2: a real Y
 * axis on the 1 / 2 / 2.5 / 5 × 10ⁿ ladder at the asset's own precision, a real X
 * axis with non-overlapping time labels and a day rule, and colour.
 *
 * **Direction is encoded twice**, by glyph and by ink (`specs/charts.md` §2.1,
 * `decisions.md` U3). Never drop one of the two: the glyph is what survives a
 * greyscale screenshot, a deuteranopic reader and a terminal paste, and it costs
 * nothing.
 *
 * Two deliberate departures from the site, both required by the spec:
 *  - Volume is the bar's real volume scaled against the window maximum, not the
 *    site's `seg.length / per` placeholder, and it carries no direction — §2.3 puts
 *    the whole volume row on `--body-dim` because colouring it by direction doubles
 *    the ink for information already carried twice above it.
 *  - The axis rule marks the columns that actually carry a time label instead of
 *    every fifth candle.
 */

/** One assembled bar. Prices are pre-rounded in the core; the view never re-rounds (§5.3). */
export interface Bar {
  /** Bucket open time, epoch milliseconds. Buckets are epoch-aligned (§3.2). */
  time: number;
  open: number;
  high: number;
  low: number;
  close: number;
  /** Base-asset volume. Only its ratio to the window maximum is rendered. */
  volume: number;
}

/**
 * Per-character ink code. One code per cell, parallel to the character grid, so the
 * view maps codes to the design tokens and the renderer stays free of CSS.
 */
export const INK = {
  /** Nothing drawn here. */
  none: " ",
  /** Up candle: body, wick and caps. `--up` (`decisions.md` U3). */
  up: "u",
  /** Down candle: body, wick and caps. `--down`, deliberately dimmer than `--hazard`. */
  down: "d",
  /** Volume row. `--body-dim` regardless of direction (§2.3). */
  volume: "v",
  /** Gridlines, the axis rule and day boundaries. `--rule`. */
  rule: "r",
  /** Axis labels, both price and time. `--bracket`. */
  label: "l",
  /** Last-price marker and its label. `--uranium`, the one live element on the surface. */
  last: "k",
} as const;

/** A run of adjacent cells sharing one ink code, so the view emits one span per run. */
export interface InkRun {
  text: string;
  ink: string;
}

/**
 * Why a frame looks the way it does. Three distinct failures — no bars, nothing finite
 * among them, and a grid too small to carry an axis — all paint the same blank grid, and
 * a caller that cannot tell them apart cannot say anything true about it. A blank panel
 * reads as downtime, which `specs/charts.md` §3.2 names as the failure to avoid. This is
 * AGENTS.md invariant 8 ("every rejection is typed") applied to the view: the renderer
 * reports which one it was and stays free of the copy that explains it.
 */
export type FrameStatus = "ok" | "no_bars" | "no_finite_bars" | "grid_too_small";

/**
 * A rendered frame. `text[y]` and `ink[y]` are the same length as each other and as
 * every other row, which is what lets a caller diff two frames row by row.
 */
export interface CandleFrame {
  width: number;
  height: number;
  text: readonly string[];
  ink: readonly string[];
  /** `"ok"` when the frame carries candles; otherwise which failure produced the blank. */
  status: FrameStatus;
}

/** Everything the renderer needs. No market state, no clock, no venue handle. */
export interface CandleInput {
  /**
   * Closed bars, oldest first. Only the last `cols` are drawn.
   *
   * Deeply readonly because the renderer only ever reads them, and saying so
   * lets a caller pass bars it holds in immutable state without copying the
   * window on every frame.
   */
  closed: readonly Readonly<Bar>[];
  /** The in-progress bar, drawn with the forming glyph. */
  forming?: Readonly<Bar> | null;
  /** Independent observed trade price; never modifies candle OHLCV. */
  latestTrade?: Readonly<{ timeMs: number; price: number; ambiguous: boolean }> | null;
  /** Candle slots across the plot. Plot width is `cols * 4` characters. */
  cols: number;
  /** Host width in character cells, including the price gutter (chart spec §2.2). */
  maxWidth?: number;
  /** Total grid rows, including the axis rule, the volume row and the time-label row. */
  rows: number;
  /** Bar interval in milliseconds. Selects the X-axis label family (§2.2). */
  intervalMs: number;
  /** `max_price_decimals` for the asset. Labels never carry more (§2.2). */
  priceDecimals: number;
  /**
   * Minutes east of UTC for every time label. Explicit rather than read from the host
   * so a frame is reproducible on any machine, which the §6 gate requires.
   */
  tzOffsetMinutes?: number;
}

/* ---- Geometry, ported verbatim from the site ---------------------------- */

/** Candle pitch: three body columns plus one gap. */
const PITCH = 4;
/** Axis rule, volume row, time-label row. */
const AXIS_ROWS = 3;
const MIN_ROWS = AXIS_ROWS + 2;
const MIN_COLS = 2;
/**
 * A hard ceiling on the grid. `cols` and `rows` come from a layout measurement, and the
 * classic measurement failure — a panel measured while hidden, or before the mono font
 * has loaded, so the character width reads 0 — yields `Infinity`. Unbounded, that either
 * throws inside `new Array` or allocates tens of millions of cells on the 90 ms clock.
 * Both ceilings are far past any real terminal, so no caller can notice the clamp.
 */
const MAX_COLS = 500;
const MAX_ROWS = 500;

const GLYPH_UP = "+";
const GLYPH_DOWN = ":";
const GLYPH_FORMING = "▓";
const GLYPH_WICK = "│";
const GLYPH_CAP_TOP = "╥";
const GLYPH_CAP_BOTTOM = "╨";
const GLYPH_VOLUME_LOW = "▁";
const GLYPH_VOLUME_HIGH = "▂";
const GLYPH_RULE = "─";
const GLYPH_RULE_TICK = "┴";
const GLYPH_GRIDLINE = "·";
const GLYPH_LAST = "◄";
const GLYPH_DAY = "│";

/* ---- Axis constants ------------------------------------------------------ */

/** The nice-number mantissa ladder from `specs/charts.md` §2.2. */
const TICK_LADDER = [1, 2, 2.5, 5] as const;
const MIN_GRIDLINES = 4;
const TARGET_GRIDLINES = 6;
const MAX_GRIDLINES = 8;
/** A hard stop so a pathological range cannot spin the tick loop. */
const MAX_TICKS = 64;
const EPS = 1e-9;

const MS_MINUTE = 60_000;
const MS_HOUR = 3_600_000;
const MS_DAY = 86_400_000;

/**
 * Time-label steps, ascending. Capped at fourteen days because these are applied by
 * modulo against the epoch, which is exact for fixed-length steps and would drift for
 * calendar months.
 */
const TIME_STEPS = [
  MS_MINUTE,
  2 * MS_MINUTE,
  5 * MS_MINUTE,
  10 * MS_MINUTE,
  15 * MS_MINUTE,
  30 * MS_MINUTE,
  MS_HOUR,
  2 * MS_HOUR,
  3 * MS_HOUR,
  6 * MS_HOUR,
  12 * MS_HOUR,
  MS_DAY,
  2 * MS_DAY,
  7 * MS_DAY,
  14 * MS_DAY,
] as const;

/* ---- Nice numbers -------------------------------------------------------- */

function gridlineCount(min: number, max: number, step: number): number {
  if (!(step > 0)) return 0;
  const first = Math.ceil(min / step - EPS);
  const last = Math.floor(max / step + EPS);
  return last < first ? 0 : last - first + 1;
}

/**
 * The axis step for `[min, max]`: the entry of the 1 / 2 / 2.5 / 5 × 10ⁿ ladder that
 * lands closest to six gridlines while staying inside four to eight (§2.2). Ties go to
 * the smaller step, which is deterministic and gives the denser axis.
 */
export function niceTickStep(min: number, max: number): number {
  const span = max - min;
  if (!Number.isFinite(span) || span <= 0) return 1;

  const base = Math.floor(Math.log10(span / TARGET_GRIDLINES));
  let best = 1;
  let bestScore = Number.POSITIVE_INFINITY;

  for (let exponent = base - 1; exponent <= base + 2; exponent += 1) {
    const decade = Math.pow(10, exponent);
    for (const mantissa of TICK_LADDER) {
      const step = mantissa * decade;
      if (!Number.isFinite(step) || step <= 0) continue;
      const count = gridlineCount(min, max, step);
      const inRange = count >= MIN_GRIDLINES && count <= MAX_GRIDLINES;
      const score = (inRange ? 0 : 100) + Math.abs(count - TARGET_GRIDLINES);
      if (score < bestScore) {
        bestScore = score;
        best = step;
      }
    }
  }
  return best;
}

/** Every multiple of `step` inside `[min, max]`, ascending. Empty if the range is unusable. */
export function priceTicks(min: number, max: number, step: number): number[] {
  const count = gridlineCount(min, max, step);
  if (count <= 0 || count > MAX_TICKS) return [];
  const first = Math.ceil(min / step - EPS);
  const out: number[] = [];
  for (let k = 0; k < count; k += 1) out.push((first + k) * step);
  return out;
}

/**
 * A grid dimension, whatever arrives: a non-negative integer inside `[0, limit]`. NaN and
 * both infinities collapse to 0, which renders blank with a `"grid_too_small"` status
 * rather than throwing — this runs on every clock tick and must survive any input.
 */
function clampGrid(value: number, limit: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.min(limit, Math.max(0, Math.trunc(value)));
}

/** Keep a caller's `max_price_decimals` inside what `toFixed` accepts, whatever arrives. */
function clampDecimals(value: number): number {
  return Number.isFinite(value) ? Math.max(0, Math.min(12, Math.trunc(value))) : 0;
}

/**
 * The fewest decimals that render `step` exactly, never more than the asset's own
 * precision (§2.2). Tick values are multiples of the step, so they need no more.
 */
function tickDecimals(step: number, priceDecimals: number): number {
  const cap = clampDecimals(priceDecimals);
  for (let d = 0; d <= cap; d += 1) {
    if (Math.abs(Number(step.toFixed(d)) - step) <= Math.abs(step) * EPS) return d;
  }
  return cap;
}

function priceLabel(value: number, decimals: number): string {
  return Number.isFinite(value) ? value.toFixed(decimals) : "";
}

/* ---- Time labels --------------------------------------------------------- */

function pad2(value: number): string {
  return value < 10 ? `0${value}` : String(value);
}

/**
 * `HH:MM` below a day, `MM-DD` at or above it — minute boundaries for sub-hour
 * intervals, hour boundaries for sub-day, dates above that (§2.2). Read through
 * UTC getters after shifting by the caller's offset so the frame never depends on
 * the host timezone.
 */
function formatTimeLabel(timeMs: number, offsetMs: number, stepMs: number): string {
  const shifted = new Date(timeMs + offsetMs);
  if (Number.isNaN(shifted.getTime())) return "";
  if (stepMs >= MS_DAY) return `${pad2(shifted.getUTCMonth() + 1)}-${pad2(shifted.getUTCDate())}`;
  return `${pad2(shifted.getUTCHours())}:${pad2(shifted.getUTCMinutes())}`;
}

/** The smallest step whose labels cannot touch: one blank column between neighbours. */
function pickTimeStep(intervalMs: number, labelWidth: number): number {
  const needed = ((labelWidth + 1) / PITCH) * Math.max(1, intervalMs);
  for (const step of TIME_STEPS) {
    if (step >= intervalMs && step >= needed) return step;
  }
  return TIME_STEPS[TIME_STEPS.length - 1];
}

function localDay(timeMs: number, offsetMs: number): number {
  return Math.floor((timeMs + offsetMs) / MS_DAY);
}

/* ---- The reusable character buffer --------------------------------------- */

class Grid {
  width = 0;
  height = 0;
  private chars: string[] = [];
  private inks: string[] = [];

  reset(width: number, height: number): void {
    const cells = width * height;
    if (this.chars.length !== cells) {
      this.chars = new Array<string>(cells);
      this.inks = new Array<string>(cells);
    }
    this.width = width;
    this.height = height;
    this.chars.fill(" ");
    this.inks.fill(INK.none);
  }

  put(x: number, y: number, char: string, ink: string): void {
    if (x < 0 || y < 0 || x >= this.width || y >= this.height) return;
    const index = y * this.width + x;
    this.chars[index] = char;
    this.inks[index] = ink;
  }

  write(x: number, y: number, text: string, ink: string): void {
    for (let i = 0; i < text.length; i += 1) this.put(x + i, y, text[i], ink);
  }

  /**
   * Right-align `text` in the `width`-wide field starting at `x`, blanking the whole
   * field first. Every gutter write goes through here on purpose. A right-aligned write
   * that covers only part of its field leaves the previous label's leading characters in
   * place and splices them onto the front of the new one: a 5-character last price under
   * a 6-character tick label renders `100000` + `99990` as `199990`, a price that does
   * not exist, in the most prominent ink on the surface. Blanking the field first makes
   * that unrepresentable rather than merely absent from this one call site.
   */
  writeRight(x: number, y: number, width: number, text: string, ink: string): void {
    for (let i = 0; i < width; i += 1) this.put(x + i, y, " ", INK.none);
    // A label wider than its own field would spill left over the plot and read as a
    // different number. The gutter is sized from the widest label, so this cannot happen
    // from `paint`; it is here so it cannot happen from the next call site either.
    if (text.length > width) return;
    this.write(x + (width - text.length), y, text, ink);
  }

  /** True when the grid holds exactly `chars` / `inks` at exactly `width` x `height`. */
  sameCells(width: number, height: number, chars: readonly string[], inks: readonly string[]): boolean {
    if (width !== this.width || height !== this.height) return false;
    const cells = this.width * this.height;
    for (let i = 0; i < cells; i += 1) {
      if (chars[i] !== this.chars[i] || inks[i] !== this.inks[i]) return false;
    }
    return true;
  }

  /** Copy the cells into the caller's arrays, reusing their storage. */
  copyCells(chars: string[], inks: string[]): void {
    const cells = this.width * this.height;
    chars.length = cells;
    inks.length = cells;
    for (let i = 0; i < cells; i += 1) {
      chars[i] = this.chars[i];
      inks[i] = this.inks[i];
    }
  }

  /** Fill `text` and `ink` with one string per row, reusing the caller's arrays. */
  emit(text: string[], ink: string[]): void {
    text.length = this.height;
    ink.length = this.height;
    for (let y = 0; y < this.height; y += 1) {
      let row = "";
      let codes = "";
      const base = y * this.width;
      for (let x = 0; x < this.width; x += 1) {
        row += this.chars[base + x];
        codes += this.inks[base + x];
      }
      text[y] = row;
      ink[y] = codes;
    }
  }
}

/* ---- Painting ------------------------------------------------------------ */

interface Slot {
  bar: Bar;
  /** Left body column. Body occupies `x`, `x + 1`, `x + 2`; `x + 3` is the gap. */
  x: number;
  up: boolean;
  forming: boolean;
}

function finiteBar(bar: Bar): boolean {
  return (
    Number.isFinite(bar.open) &&
    Number.isFinite(bar.high) &&
    Number.isFinite(bar.low) &&
    Number.isFinite(bar.close) &&
    Number.isFinite(bar.time)
  );
}

function paint(grid: Grid, input: CandleInput): FrameStatus {
  const cols = clampGrid(input.cols, MAX_COLS);
  const rows = clampGrid(input.rows, MAX_ROWS);
  const plotWidth = cols * PITCH;
  const offsetMs = Math.trunc(input.tzOffsetMinutes ?? 0) * MS_MINUTE;

  // A grid too small to carry an axis renders blank rather than throwing: this runs on
  // every clock tick and must survive any input, including a layout measurement that
  // came back as 0 or Infinity.
  if (cols < MIN_COLS || rows < MIN_ROWS) {
    grid.reset(Math.max(1, plotWidth), Math.max(1, rows));
    return "grid_too_small";
  }

  const forming = input.forming && finiteBar(input.forming) ? input.forming : null;

  // Walk the closed bars backwards and stop as soon as the window is full. Identical to
  // filtering the whole array and slicing its tail, but the cost tracks what is drawn
  // rather than what is backfilled: `decisions.md` D-d puts 30 days in the foreground,
  // which at 1m is 43,200 bars behind a 120-column plot, on the 90 ms clock (§2.5).
  const want = forming ? cols - 1 : cols;
  const visible: Bar[] = [];
  for (let i = input.closed.length - 1; i >= 0 && visible.length < want; i -= 1) {
    const bar = input.closed[i];
    if (finiteBar(bar)) visible.push(bar);
  }
  visible.reverse();
  if (forming) visible.push(forming);

  if (visible.length === 0) {
    grid.reset(Math.max(1, plotWidth), Math.max(1, rows));
    return input.closed.length === 0 && !input.forming ? "no_bars" : "no_finite_bars";
  }

  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  let maxVolume = 0;
  for (const bar of visible) {
    if (bar.low < min) min = bar.low;
    if (bar.high > max) max = bar.high;
    const volume = Number.isFinite(bar.volume) ? Math.abs(bar.volume) : 0;
    if (volume > maxVolume) maxVolume = volume;
  }
  const marker = input.latestTrade && Number.isFinite(input.latestTrade.price)
    && input.latestTrade.price > 0 && Number.isFinite(input.latestTrade.timeMs) ? input.latestTrade : null;
  if (marker) {
    min = Math.min(min, marker.price);
    max = Math.max(max, marker.price);
  }
  // A flat window — every visible bar at one price — has a zero span. Substituting 1
  // collapses every price onto the bottom plot row and leaves at most one tick, so the Y
  // axis vanishes; §2.2 requires four to eight gridlines in the visible range. Pad
  // symmetrically instead, so the flat line sits centred under a real axis. An illiquid
  // alt on a 1m chart, or a locally-aggregated interval (§3.2) in a quiet stretch,
  // produces exactly this.
  let span = max - min;
  if (!(span > 0)) {
    const pad = Math.max(Math.abs(max) * 0.0005, Math.pow(10, -clampDecimals(input.priceDecimals)));
    min -= pad;
    max += pad;
    span = max - min;
  }

  const plotRows = rows - AXIS_ROWS;
  const ruleRow = plotRows;
  const volumeRow = plotRows + 1;
  const timeRow = plotRows + 2;
  const rowOf = (value: number): number => {
    const row = Math.round((1 - (value - min) / span) * (plotRows - 1));
    return Math.min(plotRows - 1, Math.max(0, row));
  };

  // Y axis: nice-number ticks, labelled at the asset's own precision, right-aligned in
  // a gutter sized from the widest label rather than hardcoded (§2.2).
  const step = niceTickStep(min, max);
  const ticks = priceTicks(min, max, step);
  const decimals = tickDecimals(step, input.priceDecimals);
  const tickLabels = ticks.map((value) => priceLabel(value, decimals));
  const lastBar = visible[visible.length - 1];
  const lastPrice = marker?.price ?? lastBar.close;
  const lastLabel = `${marker?.ambiguous ? "?" : ""}${priceLabel(lastPrice, clampDecimals(input.priceDecimals))}`;

  let gutterText = lastLabel.length;
  for (const label of tickLabels) if (label.length > gutterText) gutterText = label.length;
  const gutterX = plotWidth + 1;

  // The visible price range changes as slots are removed, so measure the actual
  // gutter again after narrowing. Slots strictly decrease; the minimum-grid
  // branch terminates even when a host cannot fit a single price label.
  if (input.maxWidth !== undefined && plotWidth + 1 + gutterText > input.maxWidth) {
    const fitted = Math.max(0, Math.floor((input.maxWidth - 1 - gutterText) / PITCH));
    return paint(grid, { ...input, cols: Math.min(cols - 1, fitted) });
  }

  grid.reset(plotWidth + 1 + gutterText, rows);

  // Gridlines first, so a body, a wick or a cap always covers them (§2.2).
  for (const tick of ticks) {
    const y = rowOf(tick);
    for (let x = 0; x < plotWidth; x += 2) grid.put(x, y, GLYPH_GRIDLINE, INK.rule);
  }

  const slots: Slot[] = visible.map((bar, i) => ({
    bar,
    x: (cols - visible.length + i) * PITCH,
    up: bar.close >= bar.open,
    forming: forming !== null && i === visible.length - 1,
  }));

  // Day boundaries land in the gap column ahead of the candle, which no body, wick or
  // cap ever occupies, so a full-height rule cannot hide data (§2.2).
  for (let i = 1; i < slots.length; i += 1) {
    if (localDay(slots[i].bar.time, offsetMs) === localDay(slots[i - 1].bar.time, offsetMs)) continue;
    const x = slots[i].x - 1;
    for (let y = 0; y < plotRows; y += 1) grid.put(x, y, GLYPH_DAY, INK.rule);
  }

  for (const slot of slots) {
    const { bar, x, up } = slot;
    const ink = up ? INK.up : INK.down;
    const high = rowOf(bar.high);
    const low = rowOf(bar.low);
    const open = rowOf(bar.open);
    const close = rowOf(bar.close);
    const top = Math.min(open, close);
    const bottom = Math.max(open, close);

    for (let y = high; y <= low; y += 1) grid.put(x + 1, y, GLYPH_WICK, ink);

    const body = slot.forming ? GLYPH_FORMING : up ? GLYPH_UP : GLYPH_DOWN;
    for (let y = top; y <= bottom; y += 1) {
      grid.put(x, y, body, ink);
      grid.put(x + 1, y, body, ink);
      grid.put(x + 2, y, body, ink);
    }

    // Caps only where the body stops short of the wick end, exactly as the site draws it.
    if (top > high) grid.put(x + 1, top, GLYPH_CAP_TOP, ink);
    if (bottom < low) grid.put(x + 1, bottom, GLYPH_CAP_BOTTOM, ink);

    const volume = Number.isFinite(bar.volume) ? Math.abs(bar.volume) : 0;
    const filled = maxVolume > 0 && volume / maxVolume >= 0.5;
    const glyph = filled ? GLYPH_VOLUME_HIGH : GLYPH_VOLUME_LOW;
    grid.put(x, volumeRow, glyph, INK.volume);
    grid.put(x + 1, volumeRow, glyph, INK.volume);
    grid.put(x + 2, volumeRow, glyph, INK.volume);
  }

  for (let x = 0; x < plotWidth; x += 1) grid.put(x, ruleRow, GLYPH_RULE, INK.rule);

  paintTimeAxis(grid, slots, {
    intervalMs: input.intervalMs,
    offsetMs,
    plotWidth,
    ruleRow,
    timeRow,
  });

  for (let i = 0; i < ticks.length; i += 1) {
    grid.writeRight(gutterX, rowOf(ticks[i]), gutterText, tickLabels[i], INK.label);
  }

  // The last price is the one live element on the surface, so it takes uranium and
  // replaces — not overlays — whatever tick label shares its row.
  const lastRow = rowOf(lastPrice);
  const markerSlot = marker ? slots.find(slot => marker.timeMs >= slot.bar.time
    && marker.timeMs < slot.bar.time + input.intervalMs) : slots[slots.length - 1];
  if (markerSlot) grid.put(markerSlot.x + 3, lastRow, GLYPH_LAST, INK.last);
  grid.writeRight(gutterX, lastRow, gutterText, lastLabel, INK.last);

  return "ok";
}

interface TimeAxis {
  intervalMs: number;
  offsetMs: number;
  plotWidth: number;
  ruleRow: number;
  timeRow: number;
}

/**
 * Time labels at boundaries chosen for the interval, never overlapping, with the first
 * and last always drawn (§2.2). The first and last are placed before the boundary
 * candidates so a crowded axis drops an interior label rather than an edge one.
 */
function paintTimeAxis(grid: Grid, slots: readonly Slot[], axis: TimeAxis): void {
  const interval = Number.isFinite(axis.intervalMs) && axis.intervalMs > 0 ? axis.intervalMs : MS_MINUTE;
  const step = pickTimeStep(interval, 5);

  const candidates: number[] = [];
  const lastIndex = slots.length - 1;
  candidates.push(lastIndex);
  if (lastIndex !== 0) candidates.push(0);

  const boundaries: number[] = [];
  for (let i = 0; i < slots.length; i += 1) {
    const shifted = slots[i].bar.time + axis.offsetMs;
    if (shifted % step === 0) boundaries.push(i);
  }
  // An interval that shares no boundary with the ladder (7m against 10m, say) would
  // otherwise leave a bare axis; fall back to a fixed candle stride.
  if (boundaries.length < 3) {
    const stride = Math.max(2, Math.ceil(6 / PITCH));
    for (let i = 0; i < slots.length; i += stride) boundaries.push(i);
  }
  for (const i of boundaries) if (i !== 0 && i !== lastIndex) candidates.push(i);

  const taken: Array<{ start: number; end: number }> = [];
  for (const index of candidates) {
    const label = formatTimeLabel(slots[index].bar.time, axis.offsetMs, step);
    if (label.length === 0) continue;
    const centre = slots[index].x + 1;
    const start = Math.max(0, Math.min(axis.plotWidth - label.length, centre - (label.length >> 1)));
    const end = start + label.length - 1;
    let clear = true;
    for (const span of taken) {
      if (start <= span.end + 1 && end >= span.start - 1) {
        clear = false;
        break;
      }
    }
    if (!clear) continue;
    taken.push({ start, end });
    grid.write(start, axis.timeRow, label, INK.label);
    grid.put(centre, axis.ruleRow, GLYPH_RULE_TICK, INK.rule);
  }
}

/* ---- Public entry points ------------------------------------------------- */

/**
 * Render one frame into a fresh buffer. Pure: the same input gives a byte-identical
 * frame every time, which is the `specs/charts.md` §6 acceptance gate. Use this for
 * tests and one-off renders; the live surface should use {@link createCandleRenderer}.
 */
export function renderCandles(input: CandleInput): CandleFrame {
  const grid = new Grid();
  const status = paint(grid, input);
  const text: string[] = [];
  const ink: string[] = [];
  grid.emit(text, ink);
  return { width: grid.width, height: grid.height, text, ink, status };
}

/**
 * A renderer that owns its buffers. An unchanged frame costs a pass over the character
 * cells and nothing else: no row strings are rebuilt and no DOM write follows. Only a
 * changed frame pays for the one string per row that `text` and `ink` are made of.
 */
export interface CandleRenderer {
  /**
   * Paint `input` and report whether the frame differs from the previous one. The
   * returned frame is owned by the renderer and is invalidated by the next call.
   */
  render(input: CandleInput): { frame: CandleFrame; changed: boolean };
}

/**
 * A renderer with a reused character buffer and a frame diff, per `specs/charts.md`
 * §2.5. The naive version allocates a string per row per frame on the 90 ms clock;
 * this one reuses the grid and reports `changed: false` so the caller can skip the DOM
 * write, which is the common case between trades on a long interval.
 */
export function createCandleRenderer(): CandleRenderer {
  const grid = new Grid();
  const text: string[] = [];
  const ink: string[] = [];
  const previousChars: string[] = [];
  const previousInks: string[] = [];
  let previousStatus: FrameStatus | null = null;
  let previousWidth = -1;
  let previousHeight = -1;

  return {
    render(input: CandleInput) {
      const status = paint(grid, input);
      // Diff the cells, not the rows. The old order rebuilt every row string before
      // asking whether anything had changed, which is exactly the "allocates a string per
      // row per frame" that §2.5 exists to remove.
      const changed =
        status !== previousStatus || !grid.sameCells(previousWidth, previousHeight, previousChars, previousInks);
      if (changed) {
        grid.copyCells(previousChars, previousInks);
        previousWidth = grid.width;
        previousHeight = grid.height;
        previousStatus = status;
        grid.emit(text, ink);
      }
      return { frame: { width: grid.width, height: grid.height, text, ink, status }, changed };
    },
  };
}

/**
 * Group each row into runs of one ink code. The view emits one span per run instead of
 * one per character, which keeps a 120 × 44 frame at a few hundred DOM nodes.
 */
export function frameRuns(frame: CandleFrame): InkRun[][] {
  const out: InkRun[][] = [];
  for (let y = 0; y < frame.text.length; y += 1) {
    const text = frame.text[y];
    const ink = frame.ink[y];
    const runs: InkRun[] = [];
    let start = 0;
    for (let x = 1; x <= text.length; x += 1) {
      if (x < text.length && ink[x] === ink[start]) continue;
      runs.push({ text: text.slice(start, x), ink: ink[start] ?? INK.none });
      start = x;
    }
    out.push(runs);
  }
  return out;
}
