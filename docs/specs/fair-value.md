# OPPEN — Fair Value Engine

**Component specification, v0.1**

Status: draft for implementation
Scope: v1.5, Hyperliquid perpetuals. Feeds the Quantoppen panel in [charts.md](charts.md) §5.
Reference implementations: `perp_fair_value.py`, `fair_value_bars.py` (Python, for validation only — production path is the Rust core). §13 records where those files and this document disagree, and which one wins.

---

## 1. Purpose

Display a per-asset fair value series alongside OHLC candles, and expose the same
values to agents through the MCP gateway as a first-class market primitive.

The engine answers three separate questions that are routinely conflated:

| Question | Object | Horizon | Consumer |
|---|---|---|---|
| Where should this contract trade, given carry? | fair funding rate `f*` | hours–days | basis/carry signals |
| What will the chain use to margin and liquidate me? | mark price | continuous | risk guardrails |
| What is the best estimate of price right now? | combined fair value `FV` | ms–days | chart overlay, quoting |

These are different objects with different failure modes. The engine computes all
three, keeps them separate in the type system, and never silently substitutes one
for another.

---

## 2. Theory

### 2.1 The universal relation

A perpetual has no expiry, so no-arbitrage does not pin down a price level. It
pins down a **rate**. Run the cash-and-carry — long perp, short spot, hold
indefinitely. Over `dt` the hedged book earns:

```
d(PnL) = (r_collateral − y_asset − f) · dt   +   basis noise
```

Zero drift at equilibrium gives the only universal statement available:

```
                    f* = r − y                                    (1)
```

The fair funding rate equals the cost of carry. This is the same object as a
dated future's annualized basis, expressed as a rate rather than a price gap.

### 2.2 From rate to price

Every venue defines funding as a function `g` of the observed premium. The fair
price is the inverse:

```
    b* = g⁻¹(f*)
    P_fair = P_oracle · (1 + b*)                                  (2)
```

Everything venue-specific lives in `g`. Swapping venues means swapping `g`, not
rewriting the engine — this is the extension point for HIP-3 DEXes and any
non-Hyperliquid venue in v2.

---

## 3. Hyperliquid specialization

### 3.1 The funding function

Per Hyperliquid docs, the 8-hour funding rate is:

```
    F₈ₕ = P̄ + clamp(i − P̄, −0.0005, +0.0005)
    i   = 0.0001                    (fixed, 0.01% per 8h)
```

where `P̄` is the average premium index, sampled every 5 seconds and averaged over
the hour. Funding is **paid hourly at `F₈ₕ / 8`**, capped at 4%/hour.

The premium is defined on **impact prices**, not mid:

```
    premium = impact_price_difference / oracle_px
    impact_price_difference = max(impact_bid − oracle, 0) − max(oracle − impact_ask, 0)
```

Funding payment uses the **oracle** price for notional conversion, not the mark:

```
    payment = position_size · oracle_px · F₁ₕ                      (3)
```

### 3.2 The inverse, and the dead zone

Inverting the clamp yields three regimes:

| Average premium `P̄` (per 8h) | Funding `F₈ₕ` |
|---|---|
| `P̄ < −0.0004` | `P̄ + 0.0005` |
| **`−0.0004 ≤ P̄ ≤ +0.0006`** | **`i = 0.0001`, flat** |
| `P̄ > +0.0006` | `P̄ − 0.0005` |

So:

```
    b* = f* + 0.0005 · sign(f* − i)                                (4)
```

with the inverse becoming **interval-valued** when `f* = i`.

**This is the single most important implementation fact in the spec.** Whenever
the hourly-averaged premium sits between −4bp and +6bp, funding is pinned at
exactly `0.01%/8h` and the printed rate carries **zero information** about the
premium. Any estimator fitted on funding history must treat those prints as
**censored interval observations**, not as data points. Failing to do so biases
every estimate of premium persistence — which matters because premium persistence
feeds quote skew.

Corollary: the pinned rate is ≈11.6% APR compounded, structurally biased toward
shorts. It is a mechanism constant, not a carry estimate. The gap between it and
true USD/asset carry is the persistent basis-trade edge and explains why
Hyperliquid funding skews positive. **Never use `i` as `f*`.**

### 3.3 Oracle and mark price

**Oracle** — validator-computed weighted median of CEX **spot** mids:
Binance 3, OKX 2, Bybit 2, Kraken 1, Kucoin 1, Gate 1, MEXC 1, Hyperliquid spot 1.
Assets whose primary liquidity is external (BTC) exclude HL spot; assets whose
primary liquidity is on HL (HYPE) exclude external sources until a liquidity
threshold is met. Updated roughly every 3 seconds.

**Mark price** — median of three components:

1. `oracle + EMA₁₅₀ₛ(HL_mid − oracle)`
2. `median(HL best bid, HL best ask, HL last trade)`
3. weighted median of Binance 3, OKX 2, Bybit 2, Gate 1, MEXC 1 **perp** mids

If exactly two of the three exist, a 30-second EMA of component (2) is added as a
fourth input.

Mark price must be replicated **bit-exactly**. It is not an estimate we are free
to improve on — it is the number the chain uses for margining, liquidations, TP/SL
triggers and unrealized PnL. Any divergence between our reconstruction and
`markPx` is a bug in our reconstruction, and the guardrail layer must treat a
persistent divergence as a hard stop before signing.

---

## 4. The combined fair value

### 4.1 Components

At each sample `s` on a fixed clock, in price units:

| Key | Estimator | Source |
|---|---|---|
| `micro` | microprice on the HL L2 book | `l2Book` |
| `mark` | replicated venue mark price | local + `markPx` cross-check |
| `carry` | `oracle · (1 + b*)` from (2) and (4) | `oraclePx` + config carry |
| `stat` | slow macro/cointegration fit (v2) | external, forward-filled |

### 4.2 Combination

```
    FV(s; Δ, a) = Σₖ βₖ(Δ, a) · Xₖ(s)      s.t.  Σβ = 1,  β ≥ 0     (5)
```

Weights depend on **both** the bar interval `Δ` and the asset `a`. This is not a
refinement — it is the load-bearing idea. Microprice dominates BTC at 1s and is
noise for an illiquid alt at 1d; the correct weighting is not a constant and
should not be a tunable.

### 4.3 Weight estimation

Define each component's forecast error against the realized mid one horizon
ahead, in relative terms:

```
    eₖ(s) = Xₖ(s) / M(s + Δ) − 1
    Σ(Δ, a) = Cov[e]
    β* = Σ⁻¹1 / (1ᵀΣ⁻¹1)                                          (6)
```

This is the Bates–Granger minimum-variance combination — algebraically identical
to a minimum-variance portfolio.

Three requirements, each of which was observed to matter in the reference
implementation:

1. **Use the full covariance solve, not inverse-MSE.** Every component's error
   shares the same `−M(s+Δ)` random-walk term, which dominates the diagonal and
   masks real differences between estimators. In the synthetic test, components
   with noise 1e-5 / 4e-5 / 2e-4 produced near-identical raw MSEs (3.1e-8, 3.2e-8,
   7.1e-8) but correctly separated to weights 0.81 / 0.14 / 0.05 under the
   covariance solve. The diagonal shortcut would have given roughly equal weights.
2. **Shrink toward equal weights**, `β = (1−λ)β* + λ·β_eq`, λ ≈ 0.15 for liquid
   assets and 0.3–0.4 for thin books. Add a scale-aware ridge to `Σ` before
   inversion.
3. **Estimate on a lagged rolling window.** A component vector is only fed to the
   estimator once its realization horizon has elapsed. The displayed line must be
   strictly out-of-sample or the overlay is circular.

Projection to the simplex: clip negatives, renormalize, repeat (3 iterations is
sufficient in practice).

**Degradation:** if a component is missing or stale for any sample within a bar,
drop it and renormalize `β` over the survivors. Mark the bar `degraded`. Never
substitute a default value for a missing component — that injects a fictitious
estimator into the combination.

### 4.4 Bar aggregation

FV aggregates exactly like price:

```
    FV_open  = FV(s_first)
    FV_high  = max FV(s)
    FV_low   = min FV(s)
    FV_close = FV(s_last)
```

### 4.5 Residual series

The level is not the interesting output; the residual is.

```
    basis_bp = 10⁴ · (Close − FV_close) / FV_close                (7)
    z        = (basis_bp − μ_b) / σ_b            over W bars      (8)
    z_sigma  = (basis_bp / 10⁴) / σ_Δ                             (9)
```

where `σ_Δ` is per-bar realized volatility of log returns.

`z` is the per-asset dislocation signal. `z_sigma` expresses the basis in units
of that asset's own volatility and is what makes a cross-asset scan meaningful —
3bp on BTC and 40bp on HYPE land on the same scale.

---

## 5. Data contract

### 5.1 Endpoints

| Purpose | Call | Cadence |
|---|---|---|
| Asset universe + live ctx | `POST /info {"type":"metaAndAssetCtxs"}` | poll or WS |
| Order book | `POST /info {"type":"l2Book","coin":C}` | WS `l2Book` |
| Funding history | `POST /info {"type":"fundingHistory",...}` | on demand, backfill |
| Live ctx stream | WS `activeAssetCtx` | push |

`metaAndAssetCtxs` returns `[meta, assetCtxs[]]` with per-asset fields:
`funding` (hourly rate), `openInterest`, `prevDayPx`, `dayNtlVlm`, `premium`,
`oraclePx`, `markPx`, `midPx`, `impactPxs` (`[impact_bid, impact_ask]`).

Verify exact WS subscription names and payload shapes on first run; the schema
above follows the documented REST response.

### 5.2 Sampling

**Sample the clock, not the events.** Components update at different rates —
oracle ~3s, book far faster, funding hourly. Building FV on event arrival creates
phantom correlation between whichever components happened to tick together, which
corrupts `Σ` and therefore `β`.

- Fixed cadence δ, default 1s (configurable down to 250ms).
- Forward-fill each component; carry a `stale: BitSet` per sample.
- Staleness thresholds per component: oracle 10s, book 2s, mark 5s.
- A sample with an empty book on either side marks `micro` stale, not zero.

### 5.3 Units

Strict convention, enforced by newtypes:

| Suffix | Meaning |
|---|---|
| `_8h` | rate over one 8-hour funding period |
| `_1h` | rate over one hour — what the `funding` field returns |
| `_apr` | annualized, compounded hourly (`(1+r₁ₕ)^8760 − 1`) |
| `_bp` | basis points |

Unit confusion between `F₈ₕ` and `F₁ₕ` is a factor-of-8 error in every carry
number downstream. Newtype it.

---

## 6. Rust core

### 6.1 Module layout

```
core/
  fairvalue/
    mod.rs
    funding.rs        // g, g⁻¹, dead zone, censoring, carry
    mark.rs           // mark price replication, EMA, weighted median
    book.rs           // microprice, imbalance, impact prices
    combine.rs        // covariance, min-variance solve, simplex projection
    bars.rs           // sampler, bar assembly, basis/z/z_sigma
    types.rs          // newtypes, Sample, FairValueBar
```

### 6.2 Core types

```rust
#[derive(Copy, Clone, Debug, PartialEq)] pub struct Rate8h(pub f64);
#[derive(Copy, Clone, Debug, PartialEq)] pub struct Rate1h(pub f64);
#[derive(Copy, Clone, Debug, PartialEq)] pub struct Apr(pub f64);
#[derive(Copy, Clone, Debug, PartialEq)] pub struct Px(pub f64);
#[derive(Copy, Clone, Debug, PartialEq)] pub struct Bp(pub f64);

pub const INTEREST_8H: Rate8h = Rate8h(0.0001);
pub const CLAMP: f64 = 0.0005;
pub const FUNDING_CAP_1H: f64 = 0.04;

/// Inverse of the funding function. Interval-valued inside the dead zone.
pub enum FairPremium {
    Point(f64),
    Censored { lo: f64, hi: f64 },
}

pub struct Sample {
    pub ts_ns: u64,
    pub mid: Px,
    pub components: ComponentVec,   // fixed-size, ordered
    pub stale: ComponentMask,
}

pub struct FairValueBar {
    pub coin: SmolStr,
    pub interval_s: u32,
    pub start_ts: u64,
    pub end_ts: u64,
    pub ohlc: Ohlc,
    pub fv_ohlc: Ohlc,
    pub basis_bp: Bp,
    pub z: f64,
    pub z_sigma: f64,
    pub weights: ComponentVec,
    pub n_samples: u32,
    pub quality: Quality,
}

pub enum Quality { Ok, Degraded(ComponentMask), Warmup, Unusable }
```

`ComponentVec` is a fixed-size array, not a `HashMap` — the component set is
known at compile time in v1 and this keeps the hot path allocation-free.

### 6.3 Traits

```rust
pub trait FairValueComponent {
    fn name(&self) -> ComponentId;
    fn value(&self, snap: &MarketSnapshot) -> Option<Px>;
    fn max_age(&self) -> Duration;
}

/// The venue-specific funding function. Swap this for other venues.
pub trait FundingModel {
    fn funding_from_premium(&self, p: Rate8h) -> Rate8h;
    fn premium_from_funding(&self, f: Rate8h) -> FairPremium;
    fn dead_zone(&self) -> (f64, f64);
    fn payment(&self, size: f64, oracle: Px, f: Rate1h) -> f64;
}
```

### 6.4 Linear algebra

`Σ⁻¹1` on a 3–4 dimensional symmetric positive-definite matrix. Use a hand-rolled
Cholesky rather than pulling in `nalgebra` for a 4×4 — keeps the dependency
surface small, which matters for a local-first, auditable binary. Add ridge
`ε·tr(Σ)/n` with `ε = 1e-6` before factorization.

### 6.5 Determinism

The FV engine must be deterministic given an input sample stream: same samples in,
same bars out, on any machine. No wall-clock reads inside the computation, no
iteration over hash maps, no floating-point reduction order that depends on thread
scheduling. This is a prerequisite for replay-based testing and for reproducing an
agent's decision after the fact — which the guardrail audit trail requires.

---

## 7. MCP surface

Agents consume fair value through the gateway. Tools are read-only; nothing here
touches the signing path.

| Tool | Returns |
|---|---|
| `fair_value.bars` | `{coin, interval, limit}` → `FairValueBar[]` |
| `fair_value.snapshot` | `{coin}` → current FV, basis_bp, z, z_sigma, quality |
| `fair_value.weights` | `{coin, interval}` → live β + per-component MSE |
| `fair_value.carry` | `{coin}` → observed funding APR, `f*`, edge APR, censored flag |
| `fair_value.scan` | `{interval, sort}` → cross-asset table ranked by `z_sigma` |

Every response carries `quality` and, where relevant, `censored`. An agent that
receives `Quality::Degraded` or `Quality::Unusable` and proceeds to quote against
the FV anyway is exactly the class of behavior the Rust guardrails exist to catch:

**Guardrail rule.** Reject any order whose price was derived from a FV bar with
`quality != Ok`, or where the reconstructed mark price has diverged from `markPx`
by more than a configured tolerance for longer than a configured window. The
rejection happens in the core before signing, not in the agent.

---

## 8. Display

Chart panel, per asset, per interval:

- **Price** — candles, as now.
- **Fair value** — a single line at `fv_close`, plus a band at `±k·σ_b`. Render as
  a line and band, **not** as candles: a smoothed series produces a ribbon that
  reads as a rendering bug next to real OHLC. `fv_high`/`fv_low` are stored and
  available (a wide intra-bar FV range means the components disagreed) but are a
  diagnostic, not a default visual.
- **Degraded bars** — visually distinct (dashed / reduced opacity). Do not hide
  them and do not interpolate across them.

Sub-panels:

- **Basis** — `basis_bp` histogram with the `±k·σ` band.
- **Weights** — stacked area of live β over time. This is the panel most worth
  building. A live β vector tells you at a glance whether an asset's price is
  currently being set by its own book or by external spot, which is a
  liquidity-regime indicator obtained free from the estimator and more
  informative than the FV line itself.
- **Carry** — observed funding APR vs `f*`, with the dead-zone band shaded and
  censored prints marked.

**Naming:** call this *basis* or *dislocation* in the UI. "Fair value gap" already
means an unrelated three-candle imbalance pattern in retail TA and will be
misread.

---

## 9. Configuration

| Key | Default | Notes |
|---|---|---|
| `sample_cadence_ms` | 1000 | 250 min |
| `intervals_s` | `[1, 60, 3600]` | one engine per (asset, interval) |
| `horizon_bars` | 1 | display; signal engines may request longer |
| `weight_window` | 2000 | error observations retained |
| `weight_min_obs` | 100 | below this, equal weights + `Quality::Warmup` |
| `shrink_lambda` | 0.15 | 0.3–0.4 for thin books |
| `basis_window` | 500 | bars, for `z` |
| `vol_window` | 500 | bars, for `σ_Δ` |
| `collateral_apr` | **operator-set** | what USDC margin actually earns |
| `asset_lend_apr` | 0.0 per asset | borrow cost of the underlying |
| `mark_divergence_bp` | 5 | guardrail tolerance |
| `mark_divergence_window_s` | 30 | guardrail window |

`collateral_apr` is an operator input and must not default to the venue's
`0.01%/8h`. If an empirical estimate is wanted, annualize the dated CME or Binance
quarterly basis for the same asset — the same carry, observed on a venue where it
is not distorted by a hardcoded constant.

---

## 10. Validation

Build in this order; each step gates the next.

1. **Mark price replication.** Poll `metaAndAssetCtxs`, run the reconstruction
   alongside `markPx`, log the residual distribution per asset. Target: residual
   under 1bp at the 99th percentile in normal conditions. This is the first thing
   to build because it is the one place where being wrong costs liquidations
   rather than basis points, and because a wrong 150s EMA or CEX weighting shows
   up here immediately.
2. **Censoring audit.** Over a week of `fundingHistory`, count the fraction of
   prints exactly equal to `0.0000125` per asset. For liquid assets this should be
   a large share. Confirm the estimator treats them as intervals.
3. **Weight recovery.** Replay synthetic streams with known component noise and
   confirm `β` recovers the ordering. Confirm the covariance solve beats
   inverse-MSE on a stream where components share a common walk term.
4. **Out-of-sample check.** Confirm that removing the lag in the training queue
   measurably improves in-sample fit and degrades forward fit. If it does not,
   the lag is not wired correctly.
5. **Replay determinism.** Same sample file, two machines, byte-identical bars.
6. **Degradation drills.** Kill the book feed, stale the oracle, drop a CEX
   source. Confirm `β` renormalizes, `quality` propagates to MCP, and the
   guardrail rejects orders derived from degraded bars.

---

## 11. Open items

- Confirm WS subscription names and payload shapes against the live gateway.
- Confirm `impact_notional_usd` per asset; the API returns `impactPxs` directly so
  this is only needed for replaying raw book snapshots.
- `stat` component deferred to v2 — requires a cointegration fit and external
  macro series, which conflicts with local-first until the data path is decided.
- HIP-3 builder DEXes carry a `dex` parameter on `metaAndAssetCtxs` and may use a
  modified funding multiplier; `FundingModel` is the extension point.
- Hyperps replace the external oracle with an 8h EMA of minutely mark prices and
  scale premium samples to 1% of the usual formula. They need a distinct
  `FundingModel` impl; do not route them through the standard one.

---

## 12. References

- Hyperliquid docs — Funding, Oracle, Robust price indices, Hyperps
  (`hyperliquid.gitbook.io`)
- Stoikov, S. (2017), *The Micro-Price: A High Frequency Estimator of Future
  Prices*, SSRN 2970694; code at `github.com/sstoikov/microprice`
- Bates, J. & Granger, C. (1969), *The Combination of Forecasts*
- BIS Quarterly Review, Sept 2016, *Covered interest parity lost* — for the carry
  framing in §2.1

---

## 13. Reference implementation: divergences and resolutions

The two Python files were read line by line against this document. They disagree
with it in 37 places. Most are the reference being a sketch, but several are
places where this document is genuinely ambiguous and the code settled it one way
without saying so. Every one is resolved below. **Where a row says "spec wins",
the Python is a known-incomplete sketch and the Rust must not copy it.**

### 13.1 Mark price

| # | Finding | Resolution |
|---|---|---|
| 1 | The two-component fallback appends `c2` a second time, so `median([c1,c2,c2]) == c2` unconditionally and component (1) is discarded whenever the CEX perp feed is absent. The code says `# replace with a real 30s EMA in production`. | **Spec wins.** Implement the real 30 s EMA of component (2). The Python path is wrong and would silently drop the oracle-plus-basis component in exactly the degraded conditions where it matters. |
| 2 | The 150 s and 30 s EMA constants appear only in this document; the Python takes `basis_ema` as a caller argument and never constructs one. | Rust supplies both as named constants. |
| 3 | `EMA`'s docstring says half-life; the implementation is a time constant (`w = exp(−dt/τ)`). | **Time constant**, not half-life. A Rust port that reads the docstring and uses `exp(−dt·ln2/τ)` diverges. §3.3's "EMA₁₅₀ₛ" means τ = 150 s. |
| 4 | `weighted_median` drops non-positive weights, uses `acc ≥ total/2`, no interpolation, no tie-averaging. | Adopt verbatim. It is deterministic, which §6.5 requires. |
| 5 | Oracle reconstruction is not implemented; `oraclePx` is consumed from the API. | Correct for v1.5. Reconstructing the oracle is a v2 item and is not required for FV. |

### 13.2 Funding and carry

| # | Finding | Resolution |
|---|---|---|
| 6 | `FUNDING_CAP_1H = 0.04` is defined and never applied. | Apply it in `funding_from_premium`. A missing cap is a real error at the tail. |
| 7 | `fair_funding_8h` computes `apr_to_8h(r) − apr_to_8h(y)`, not `apr_to_8h(r − y)`. Eq (1) is ambiguous. | **Adopt the code**: convert each leg, then subtract. Document it in the newtype. |
| 8 | `carry_edge_apr` uses raw `collateral_apr − asset_lend_apr` for the fair leg while `fair_funding_8h` compounds each leg. The two are not mutually consistent. | **Neither wins as written.** Pick one convention — compound each leg — and use it in both. This is a bug in the reference and the kind of factor-level inconsistency §5.3 exists to prevent. |
| 9 | `is_censored` takes `funding_1h`, scales by 8 and compares to `INTEREST_8H` with `abs_tol = 1e-12`, an effective hourly tolerance of 1.25e-13. §10.2 says only "exactly equal". | **Adopt the tolerance.** Exact `f64` equality on a value that made a round trip through JSON will miss censored prints. |
| 10 | `premium_from_funding_8h` and `is_censored` take different units (8 h vs 1 h) under near-identical names. | Exactly the factor-of-8 trap §5.3 warns about. The newtypes make it unrepresentable; keep them non-negotiable. |
| 11 | `fair_perp_price` returns a fair **impact-bid/ask** level, not a fair mid, and `build_sample` feeds it into the combination as `carry` with no adjustment. The `carry` component is therefore offset from `micro` and `mark` by roughly half the impact spread. | **Bug, spec wins.** Subtract half the depth-weighted spread at the impact notional before `carry` enters the combination. Uncorrected, the weight solve is fitting a constant bias. |

### 13.3 Combination weights

| # | Finding | Resolution |
|---|---|---|
| 12 | The interval-valued `FairPremium` is collapsed to the arithmetic midpoint in `build_sample`. This document defines the enum but never says how the display component collapses it. | **Adopt the midpoint**, and carry `censored: bool` on the bar so a consumer can tell a midpoint from a point estimate. |
| 13 | `np.cov` is mean-centred with `ddof=1`, so component **bias** is not penalized — only error variance. §4.3's "`Σ = Cov[e]`" could be read as the uncentred second moment. | **Use the uncentred second moment.** A biased component should lose weight. This is the single most consequential divergence in the list and it changes the weights. |
| 14 | The 3-iteration simplex projection is effectively one iteration: after the first clip-and-renormalize everything is already non-negative and sums to one. | Harmless. Keep one pass and delete §4.3's claim that three are needed. |
| 15 | Shrinkage is applied over the full name set, then the available subset is sliced and renormalized. Degradation does not re-solve on the sub-covariance. | **Adopt slice-and-renormalize**, matching §4.3's wording. Re-solving per degradation pattern would thrash the weights. |
| 16 | `np.linalg.solve` (LU) is used, not the Cholesky §6.4 mandates. Fallback on failure is equal weights. | **Spec wins.** Hand-rolled Cholesky for a 3×4 matrix, with the equal-weights fallback preserved. |
| 17 | Stale components still enter the training set: `_queue_for_training` copies components wholesale and ignores `stale`. Staleness is honoured only at bar close. | **Bug, spec wins.** A stale component's error is not a real observation. Exclude it from `Σ` estimation. |
| 18 | `add_observation` requires every name present; a sample missing one contributes nothing at all. | Adopt. Partial observations would make `Σ` entries estimated on different sample sets. |
| 19 | The realized mid is the triggering sample's `mid`, which may be later than exactly `ts + horizon` under irregular sampling. | Acceptable on a fixed clock (§5.2). Assert the sampler's regularity rather than interpolating. |

### 13.4 Bars and residuals

| # | Finding | Resolution |
|---|---|---|
| 20 | `z` excludes the current bar from its history; `z_sigma` includes the current bar's return. Asymmetric out-of-sampleness. Eqs (8) and (9) do not specify. | **Exclude the current bar from both.** The displayed residual must not be normalized by a statistic that contains it. |
| 21 | Warm-up thresholds for `z` and `z_sigma` are a hardcoded `30`, unrelated to `basis_window` / `vol_window` = 500. | Make it configurable, default 30, and emit `Quality::Warmup` below it rather than silently returning `0.0` as the Python does. **A zero that means "unknown" is the worst possible encoding.** |
| 22 | `start_ts` is bucket-aligned but `end_ts` is the last sample's raw timestamp, so bars are not uniform in duration. | Use bucket edges for both. Store `last_sample_ts` separately if needed. |
| 23 | No empty bars are emitted for gaps; skipped intervals simply produce no bar. | Emit a bar with `Quality::Unusable` and `n_samples = 0`. A missing bar and a bar with no data are different facts and the chart must not interpolate across either. |
| 24 | If no component survives, `_close_bar` returns `None` and the bar vanishes. The code has only a boolean `degraded` — no `Warmup`, no `Unusable`. | **Spec wins.** The four-state `Quality` enum is load-bearing for the §7 guardrail rule. |
| 25 | `build_sample` feeds `ctx.mark_px` straight from the API as the `mark` component. `mark_price()` is never called by the bars module. | **Spec wins.** §4.1 says replicated-with-cross-check, and §10.1 makes replication the first validation gate. Consuming `markPx` directly makes that gate untestable. |
| 26 | The target `mid` is the L2 top-of-book simple mid, not `ctx.midPx`. | Adopt. It is the quantity the components are forecasting. |
| 27 | Empty-book fallback is self-referential: `micro = ctx.mid_px or ctx.mark_px`, then `mid = micro`. The target becomes a copy of a component, and `micro` is populated-but-stale rather than omitted. | **Bug, spec wins.** §4.3: never substitute a default for a missing component. Omit `micro` and mark the sample. |
| 28 | Only two of the three staleness thresholds exist. Oracle 10 s and empty-book are implemented; book 2 s and mark 5 s are not, and `mark` is never marked stale. | Implement all three per §5.2. |
| 29 | `FairValueBook.engine` never passes `shrink`, `basis_window` or `vol_window`, despite a comment saying thin books get more shrinkage. Per-asset shrinkage is unimplemented. | Implement per-asset shrinkage. Note the numeric discrepancy: the code's docstring says 0.2–0.4, §4.3 says 0.3–0.4. **Use 0.3–0.4.** |
| 30 | `FairValueBook.on_sample` feeds one identical sample stream to every interval engine, so the 3600 s engine estimates `Σ` on 1 s-cadence errors with a 3600 s lag. | **Bug, spec wins.** One sampler per interval, or a decimated stream per engine. As written, the hourly weights are fitted on the wrong error distribution. |
| 31 | A component present in `Sample` but absent from `CombinationWeights.names` is silently ignored (`stat`). | Make it a hard error at construction. Silent omission of a component is indistinguishable from it having zero weight. |
| 32 | No newtypes anywhere; all bare `float`. | **Spec wins**, §5.3 and §6.2. |
| 33 | No WebSocket code exists; REST polling only. | Expected. The WS pool is P2 and this engine consumes it. |
| 34 | `fundingHistory` is fetched and never parsed or consumed. | Needed for the §10.2 censoring audit. Implement the consumer. |

### 13.5 Constants confirmed by the reference

Adopt verbatim: `INTEREST_8H = 0.0001`, `CLAMP = 0.0005`, `FUNDING_CAP_1H = 0.04`,
dead zone `(−0.0004, +0.0006)`, hours per year `8760`, ridge `ε = 1e-6` with an
absolute floor of `1e-18` on `trace/n`, censoring tolerance `1e-12` absolute,
default window `2000`, `min_obs` `100`, `shrink` `0.15`, `basis_window` and
`vol_window` `500`, CEX **perp** weights `{binance 3, okx 2, bybit 2, gate 1,
mexc 1}` — note this correctly omits Kraken and Kucoin, which appear only in the
**spot** oracle list in §3.3.

### 13.6 The one that matters most

Divergence 13 — centred versus uncentred covariance — changes every weight the
engine produces, and neither document stated a choice. Divergences 11, 25 and 30
each inject a systematic error into the combination that the estimator cannot
detect, because a constant bias, a substituted component and a mis-sampled error
distribution all look like a well-behaved component to a variance solve.

These four are the reason §10's validation order starts with mark-price
replication and ends with replay determinism, and why weight recovery on
synthetic streams with known noise is step 3 rather than an afterthought.
