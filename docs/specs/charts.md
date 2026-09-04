# Charts: ASCII candles, arbitrary intervals, line, and Quantoppen

**Component specification, v0.1 — draft**
Scope: candles and intervals in v1 (phase P5); line chart and Quantoppen in v1.1
Supersedes: the `lightweight-charts` line in `../spec.md` item 31. See §1.1.

---

## 1. What this is

Four related pieces of the market surface:

1. **ASCII candles** — the renderer from the marketing site, promoted to a real
   chart with proper axes and directional colour.
2. **Arbitrary intervals** — any interval the operator types, not a fixed menu.
3. **Line chart** — the same data, one stroke, for when structure is noise.
4. **Quantoppen** — a multi-asset watch grid, each row or card carrying a
   sparkline, OHLC and the fair-value metrics from [fair-value.md](fair-value.md).

### 1.1 Why not lightweight-charts

**Decided 2026-09-03: ASCII is primary and `lightweight-charts` is dropped entirely.**
See [decisions.md](../decisions.md) U1 and U2. The rest of this section records the
reasoning.


`../spec.md` item 31 names `lightweight-charts` for the chart panel. The site's
ASCII renderer is better aligned with the design system, which is built on a
character grid with a five-glyph ramp and forbids gradients, shadows and easing.
A canvas chart inside that shell reads as a foreign object.

The recommendation is to make the ASCII renderer primary and keep
`lightweight-charts` as an optional dense mode for long histories, where a
character grid runs out of horizontal resolution. This is a change to a v1 item
and needs sign-off before P5 starts. Nothing else in this spec depends on which
way it goes: the data contract in §3 is renderer-agnostic.

---

## 2. The ASCII candle renderer

### 2.1 What exists

The marketing site (`oppen Site.dc.html`, `candles(hist, cols, rows, per)`)
already draws candles on a character grid. Its geometry is right and worth
keeping verbatim:

| Element | Glyph | Notes |
|---|---|---|
| Wick | `│` | column `x+1` only |
| Up body | `+` | three columns, `x` through `x+2` |
| Down body | `:` | three columns |
| Forming body | `▓` | the in-progress candle |
| Body cap | `╥` `╨` | drawn only when the body does not reach the wick end |
| Volume | `▁` `▂` | bottom row |
| Axis rule | `─` `┴` | one row above volume |
| Last-price marker | `◄` | at the forming candle's close row |

Candle pitch is 4 columns: three body, one gap. Price maps to rows with
`row(v) = round((1 − (v − min) / span) · (R − 1))`.

**Direction is encoded twice, by glyph and by colour.** Keep that. It is why the
chart survives a deuteranopic reader, a greyscale screenshot and a terminal
paste, and it costs nothing.

### 2.2 What is missing

Three things, all of which the site version fakes because it is decoration.

**A real Y axis.** The site labels every sixth row with
`(max − (y/(R−1))·span).toFixed(1)` — fixed one decimal, arbitrary row spacing,
no relation to the asset's tick size. Replace with:

- Nice-number tick selection: choose a step from the 1 / 2 / 2.5 / 5 × 10ⁿ
  ladder such that between 4 and 8 gridlines land in the visible range.
- Labels rendered at the asset's price precision, never more (`max_price_decimals`
  from `oppen_hl::meta`), right-aligned in a fixed gutter whose width is computed
  from the widest label, not hardcoded.
- Gridlines as `·` at the label rows only, at every second column, behind the
  candles: a gridline never overwrites a body, a wick or a cap.

**A real X axis.** The site marks every fifth candle with `┴` and no time label.
Replace with time labels at tick positions chosen for the interval: minute
boundaries for sub-hour intervals, hour boundaries for sub-day, date boundaries
above that. Labels are clipped to the gutter, never overlap, and the first and
last are always drawn. A day boundary gets a full-height rule in `--rule`, which
is how a reader finds a session at a glance.

**Colour.** The site draws everything in one grey.

### 2.3 Colour

Two new tokens. They are the first colours added to the ladder since the brand
book and they need design sign-off.

```css
--up:   #2fbf71;   /* 8.3:1 on void */
--down: #e5484d;   /* 5.0:1 on void */
```

Constraints that produced these values:

- Both clear WCAG AA for graphical objects (3:1) against `--void` with margin,
  and clear AA for text (4.5:1) in case they are ever used on a number.
- `--up` is far from `--uranium` `#ffd400` in both hue and luminance, so a live
  accent is never mistaken for a green candle.
- `--down` is dimmer than `--hazard` `#ff4d2e`. Hazard stays the brightest red on
  screen and keeps its exclusive meaning: liquidation, error, kill switch.
  Brightness marks a fact, so the fact that matters most stays brightest.
- In greyscale the pair separates to 5.8:1 and 4.4:1 — distinguishable before the
  glyph difference is even considered.

**Hazard overlays drawn across the plot** — the liquidation line, a stale overlay
— use `--hazard` with a dashed pattern, so they cannot be read as a run of down
candles.

The volume row uses `--body-dim` regardless of direction. Colouring volume by
direction doubles the ink for information already carried twice above it.

### 2.4 Where it runs

Aggregation in Rust, rendering in TypeScript.

The core already owns candles: it subscribes, backfills and reconciles them.
Bar assembly must be deterministic and identical to what the fair-value engine
sees, so it cannot live in the view layer. The renderer receives an array of
closed bars plus one forming bar and turns them into a character grid. It holds
no market state and does no arithmetic beyond scaling to rows.

This split also makes the renderer testable: given a fixed bar array and a fixed
grid size, the output string is fixed. Snapshot-test it.

### 2.5 Performance

The grid is `cols × rows` characters, re-rendered on the shared 90 ms clock
(`src/lib/clock.ts`). At 120 × 44 that is 5,280 characters per frame, which is
nothing, but the naive implementation allocates a string per row per frame.
Render into a reused buffer and diff against the previous frame; skip the DOM
write when the frame is identical, which it usually is between trades on a
1-hour chart.

---

## 3. Arbitrary intervals

### 3.1 The constraint

Hyperliquid's `candleSnapshot` serves a fixed menu: `1m 3m 5m 15m 30m 1h 2h 4h
8h 12h 1d 3d 1w 1M`. The operator wants to type `7m` or `90s` and get a chart.

### 3.2 The rule

Every requested interval resolves to one of three cases, and the UI must say
which one it is rather than silently degrading.

| Case | Condition | Source | History |
|---|---|---|---|
| **Native** | Interval is on the venue menu | `candleSnapshot` | Full |
| **Resampled** | Interval is an exact integer multiple of a native one | Aggregate the largest native divisor | Full |
| **Local** | Interval is sub-minute, or not an integer multiple of any native interval | Aggregate the WS `trades` feed locally | **Forward only** |

`7m` is resampled from `1m` (7 × 1m). `45m` resamples from `15m`. `90s` is local:
no native interval divides it, and nothing below `1m` exists to aggregate.

**Resampling is exact, not approximate.** Aggregating k bars gives
`open = first.open`, `close = last.close`, `high = max`, `low = min`,
`volume = Σ`, `trades = Σ`. This is lossless because the venue's bars partition
the same time axis. Bucket boundaries are aligned to the Unix epoch, so the same
interval produces the same buckets on every machine and across restarts.

**Local aggregation has no history and must say so.** A `90s` chart opened for
the first time is empty, and fills forward from the trade feed. Render the empty
region explicitly as "no local history before HH:MM", never as a flat line or a
gap that reads as downtime. Local bars are persisted so the history accumulates
across sessions.

### 3.3 Input

A free-text field accepting `<n><unit>` with units `s m h d w M`, plus the
native menu as one-click presets. Parse strictly: reject `0m`, reject above
`1M`, reject non-integers. On accept, show the resolved case as a chip next to
the interval: `NATIVE`, `RESAMPLED FROM 1M`, `LOCAL · FORWARD ONLY`.

Cap the number of simultaneously materialized non-native intervals per symbol
(recommend 4) so a typing session does not spawn dozens of aggregators.

---

## 4. Line chart

Same bar array, one stroke. Braille or block glyphs give sub-character vertical
resolution on a character grid; the simplest good option is the eight-level block
ramp `▁▂▃▄▅▆▇█` for a column-oriented line, which keeps the renderer's
column-per-bar model intact.

Line mode drops volume and caps, keeps both axes, and colours the stroke by the
sign of the change over the visible window rather than per bar — a line has no
per-bar direction to encode, and colouring each segment produces a rainbow that
reads as noise.

The fair-value overlay from [fair-value.md](fair-value.md) §8 renders in line
mode by construction: a single stroke at `fv_close` plus a `±kσ` band. Never as
candles, for the reason that spec gives.

---

## 5. Quantoppen

### 5.1 What it is

A multi-asset watch surface. The operator adds symbols; each gets a live
sparkline, OHLC numbers and fair-value metrics. Two presentations of one data
model, toggled: a dense **table** and a **card grid**.

This is the UI for `fair_value.scan` — the cross-asset table ranked by `z_sigma`
in [fair-value.md](fair-value.md) §7. The scan tool and this panel are the same
query with two consumers, agent and human, and they must return the same numbers
or one of them is lying.

### 5.2 Columns

| Field | Units | Source |
|---|---|---|
| `symbol` | — | meta |
| sparkline | — | last N bars at the panel interval |
| `open` `high` `low` `close` | price | bars |
| `change_pct` | % over the visible window | bars |
| `mark` | price | `metaAndAssetCtxs` |
| `funding_apr_pct` | % | `funding` compounded hourly |
| `next_funding_s` | s | **derived** — `3600 − (epoch_s mod 3600)`. No such ctx field exists; see [fair-value.md](fair-value.md) §14.5 |
| `basis_bp` | bp | fair value |
| `z_sigma` | σ | fair value |
| `carry_edge_apr` | % | fair value |
| `spread_bps` | bp | book |
| `depth_usd_{achieved}` | $ | book — **the header states the window actually reached**, which is 2.4 bp on BTC from a default `l2Book`; see [fair-value.md](fair-value.md) §14.5 |
| `oi_usd` | $ | ctx |
| `quality` | enum | fair value |

Default sort is `|z_sigma|` descending, which is the whole point of the
normalization: 3 bp on BTC and 40 bp on an illiquid alt become comparable.

### 5.3 Rules

**Quality is never hidden.** A row whose fair value is `Degraded` renders its
fair-value columns dimmed with a marker; `Warmup` renders them as `—`. A number
that is not trustworthy must not look like one that is. This mirrors the
guardrail rule: an agent that trades off a degraded bar is refused, so a human
looking at the same number deserves the same warning.

**Sparklines are the block ramp**, one column per bar, coloured by net direction
over the window. They are 12 to 24 columns; below 12 a sparkline is a decoration.

**Numbers are pre-rounded in the core** to the precision the field deserves, and
the view never re-rounds. Two surfaces rounding independently is how a table and
a tooltip come to disagree.

### 5.4 Performance

The watchlist is capped (recommend 50 symbols). Every visible row needs a book
subscription for spread and depth; those come from the shared WS pool and are
unsubscribed when a row scrolls out of view and stays out for a grace period.
Rows off-screen keep their last values and a staleness age, and are visibly stale
when scrolled back into view until their first tick.

All rows share the one 90 ms clock. No row owns a timer.

### 5.5 What it is not

Not a screener over the full universe, not a backtester, and not a place where a
number appears without a stated source and quality. Ranking assets by a metric is
one step from ranking them by expected return, and the moment a column implies
"this one is good", the product has shipped a signal — which the whitepaper says
it does not do. Column headers name the measurement, never the conclusion.

---

## 6. Acceptance gates

**Candles.** Given a fixed bar array and grid size, the renderer's output string
is byte-identical across runs and machines. Axis ticks land on nice numbers at the
asset's own precision. Up and down are distinguishable in a greyscale screenshot.

**Intervals.** A resampled interval's bars are identical to the same bars computed
by hand from the native source, including at bucket boundaries and across a DST
change in the display timezone. A local interval shows its no-history region
explicitly and accumulates across a restart.

**Quantoppen.** The panel's numbers for a symbol match `fair_value.scan` for the
same symbol and interval, field for field, and a degraded row is visibly degraded
before its numbers are read.

---

## 7. Open decisions

1. **Primary renderer** (§1.1). ASCII primary with an optional dense canvas mode,
   or canvas primary. This blocks P5.
2. **Sign-off on `--up` / `--down`** entering the brand ladder.
3. **Local-interval retention.** Sub-minute bars accumulate forever by default;
   they need a retention policy or a disk budget.
4. **Whether Quantoppen is a tab or a mode of Trade.** It is closer to Portfolio
   in shape and to Trade in use.
