//! Transaction cost analysis over the ledger's own fills (`docs/spec.md`
//! spec F).
//!
//! The numbers here are read from chained rows and nothing else: an execution
//! report that could disagree with the audit trail would be worse than no
//! report, because it is the artefact an operator uses to decide whether an
//! agent is worth running.
//!
//! **Every statistic carries its `n`, and every one carries a baseline.** Spec
//! F asks for both in the same breath, and they are the same discipline: a
//! mean slippage of 3 bps means nothing without knowing it came from four
//! fills, and it means nothing without something to compare it against. The
//! baseline here is the maker/taker split, because `crossed` is on every fill
//! the venue reports and the comparison it supports — what crossing the spread
//! actually cost, against what resting earned — is the one an agent can act on.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

/// One fill as the report reads it, projected from a chained `Fill` row.
///
/// A struct rather than the raw `Value` so the parsing happens once, at the
/// edge, and every statistic below is computed from typed numbers. A row
/// missing anything required is skipped rather than defaulted — a fill with no
/// slippage is a fill nobody can score, not a fill that slipped zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredFill {
    pub symbol: String,
    pub agent_id: String,
    pub ts_ms: i64,
    /// Signed, in the convention [`crate::reconcile`] stamps: positive is
    /// cost, negative is price improvement.
    pub slip_bps: Decimal,
    pub notional_usd: Decimal,
    pub fee_usd: Decimal,
    pub closed_pnl_usd: Decimal,
    /// True when this fill took liquidity. The report's baseline.
    pub crossed: bool,
}

impl ScoredFill {
    /// Projects a chained `Fill` payload, or `None` when the row cannot be
    /// scored.
    ///
    /// Only fills that carry a `slip_bps` are scoreable, which by construction
    /// means only fills attributed to an agent's own intent: an external or
    /// manual fill has no arrival mid, because oppen never took the decision
    /// that would have stamped one. That is the honest scope of an *execution*
    /// report — it measures oppen's own execution.
    pub fn from_payload(payload: &Value) -> Option<Self> {
        let decimal = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_str)
                .and_then(|text| Decimal::from_str_exact(text).ok())
        };
        let px = decimal("px")?;
        let sz = decimal("sz")?;
        Some(ScoredFill {
            symbol: payload.get("coin")?.as_str()?.to_owned(),
            agent_id: payload.get("agent_id")?.as_str()?.to_owned(),
            // `ts_ms`, the venue's own timestamp for the trade — the same
            // key `reconcile::fill_payload` writes. Not oppen's clock: a fill
            // recovered from an outage days later still belongs to the moment
            // it traded.
            ts_ms: payload.get("ts_ms")?.as_i64()?,
            slip_bps: decimal("slip_bps")?,
            notional_usd: px.checked_mul(sz)?.abs(),
            // A fee the venue did not report is absent, not zero: `closed_pnl`
            // net of a fee that was really charged would overstate the result.
            fee_usd: decimal("fee")?,
            closed_pnl_usd: decimal("closed_pnl")?,
            crossed: payload.get("crossed")?.as_bool()?,
        })
    }
}

/// A statistic and the sample it came from.
///
/// `n` is not optional and not a footnote: spec F says "`n=` … on every stat",
/// and the reason is that a mean over three fills and a mean over three
/// hundred are different claims wearing the same number. Serialized together
/// so they cannot be separated by a caller reading one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stat {
    pub n: usize,
    pub mean_bps: Decimal,
    pub median_bps: Decimal,
    /// The worst decile, which is where execution damage actually lives — a
    /// good mean with a bad tail is a strategy that works until it does not.
    pub p90_bps: Decimal,
}

impl Stat {
    /// `None` for an empty sample. A mean of nothing is not zero.
    fn of(mut slips: Vec<Decimal>) -> Option<Self> {
        if slips.is_empty() {
            return None;
        }
        slips.sort_unstable();
        let n = slips.len();
        let sum = slips
            .iter()
            .try_fold(Decimal::ZERO, |acc, slip| acc.checked_add(*slip))?;
        Some(Stat {
            n,
            mean_bps: sum.checked_div(Decimal::from(n))?,
            median_bps: percentile(&slips, 50),
            p90_bps: percentile(&slips, 90),
        })
    }
}

/// Nearest-rank percentile on an already sorted, non-empty slice.
///
/// Nearest-rank rather than interpolated because every value it can return is
/// a slippage that actually happened. An interpolated p90 is a number no fill
/// ever printed, and this report is read next to the ledger rows it came from.
fn percentile(sorted: &[Decimal], pct: usize) -> Decimal {
    let rank = (sorted.len() * pct).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Slippage split by whether the fill took liquidity — the report's baseline.
///
/// `taker` against `maker` is the comparison an agent can act on: it is the
/// price of crossing the spread, measured rather than assumed. Either side is
/// `None` when nothing landed on it, which is itself the finding for an agent
/// that only ever crosses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlippageBaseline {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taker: Option<Stat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maker: Option<Stat>,
}

/// What the fills cost, separated rather than netted.
///
/// `docs/specs/history.md` §5 requires the decomposition to survive into
/// storage and it survives out again here: price and fees are distinct lines
/// because they are fixed by different decisions — one by execution, one by
/// the venue's schedule and the volume tier — and a net number hides which of
/// them is the problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Costs {
    pub n: usize,
    /// Realized PnL on the closing side of these fills, before fees.
    pub closed_pnl_usd: Decimal,
    pub fees_usd: Decimal,
    /// Fees as a share of the notional they were charged on, which is the
    /// figure comparable across accounts of different sizes. `None` when no
    /// notional traded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_bps_of_notional: Option<Decimal>,
    pub notional_usd: Decimal,
}

/// One symbol's slice of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolReport {
    pub symbol: String,
    pub slippage: SlippageBaseline,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all: Option<Stat>,
    pub costs: Costs,
}

/// The whole report (`docs/spec.md` spec F, `get_execution_report`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionReport {
    /// Bumped when a field changes meaning, not when one is added.
    pub contract_version: u32,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Scoreable fills in the window — attributed to an agent's own intent and
    /// carrying an arrival mid. Deliberately not the count of *all* fills:
    /// see [`ExecutionReport::unscored_fills`].
    pub scored_fills: usize,
    /// Fills in the window this report could not score, because they carried
    /// no arrival mid — an external or manual trade, or an intent from before
    /// arrival mids were stamped. Reported rather than dropped: a report whose
    /// denominator silently excluded half the account's trading would be the
    /// most misleading artefact in the product.
    pub unscored_fills: usize,
    pub slippage: SlippageBaseline,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all: Option<Stat>,
    pub costs: Costs,
    pub by_symbol: Vec<SymbolReport>,
}

/// The report's own version. `1` because this is its first shape.
const CONTRACT_VERSION: u32 = 1;

/// Basis points per unit.
const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

impl ExecutionReport {
    /// Builds the report from fills already filtered to the window and to one
    /// agent.
    ///
    /// `unscored` is passed in rather than inferred because only the caller
    /// knows how many rows it read and discarded — this function sees the
    /// survivors.
    pub fn of(fills: &[ScoredFill], from_ms: i64, to_ms: i64, unscored: usize) -> Self {
        let mut by_symbol: BTreeMap<&str, Vec<&ScoredFill>> = BTreeMap::new();
        for fill in fills {
            by_symbol
                .entry(fill.symbol.as_str())
                .or_default()
                .push(fill);
        }
        let borrowed: Vec<&ScoredFill> = fills.iter().collect();
        ExecutionReport {
            contract_version: CONTRACT_VERSION,
            from_ms,
            to_ms,
            scored_fills: fills.len(),
            unscored_fills: unscored,
            slippage: baseline(&borrowed),
            all: Stat::of(borrowed.iter().map(|fill| fill.slip_bps).collect()),
            costs: costs(&borrowed),
            // `BTreeMap` iteration, so the symbol order is one serialization
            // whatever order the fills arrived in (`AGENTS.md` invariant 6).
            by_symbol: by_symbol
                .into_iter()
                .map(|(symbol, group)| SymbolReport {
                    symbol: symbol.to_owned(),
                    slippage: baseline(&group),
                    all: Stat::of(group.iter().map(|fill| fill.slip_bps).collect()),
                    costs: costs(&group),
                })
                .collect(),
        }
    }
}

fn baseline(fills: &[&ScoredFill]) -> SlippageBaseline {
    let side = |crossed: bool| {
        Stat::of(
            fills
                .iter()
                .filter(|fill| fill.crossed == crossed)
                .map(|fill| fill.slip_bps)
                .collect(),
        )
    };
    SlippageBaseline {
        taker: side(true),
        maker: side(false),
    }
}

fn costs(fills: &[&ScoredFill]) -> Costs {
    let sum = |pick: fn(&ScoredFill) -> Decimal| {
        fills
            .iter()
            .fold(Decimal::ZERO, |acc, fill| acc.saturating_add(pick(fill)))
    };
    let notional_usd = sum(|fill| fill.notional_usd);
    let fees_usd = sum(|fill| fill.fee_usd);
    Costs {
        n: fills.len(),
        closed_pnl_usd: sum(|fill| fill.closed_pnl_usd),
        fees_usd,
        fee_bps_of_notional: fees_usd
            .checked_div(notional_usd)
            .and_then(|ratio| ratio.checked_mul(BPS)),
        notional_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(text: &str) -> Decimal {
        Decimal::from_str_exact(text).expect("decimal")
    }

    fn fill(slip: &str, crossed: bool) -> ScoredFill {
        ScoredFill {
            symbol: "BTC".to_owned(),
            agent_id: "alpha".to_owned(),
            ts_ms: 0,
            slip_bps: d(slip),
            notional_usd: d("1000"),
            fee_usd: d("0.35"),
            closed_pnl_usd: d("12"),
            crossed,
        }
    }

    /// The baseline is the whole point of the report. An agent that crosses
    /// pays the spread and one that rests earns it, and a single blended mean
    /// hides the difference — which is exactly the decision the agent is
    /// trying to make.
    #[test]
    fn slippage_is_split_by_whether_the_fill_crossed_the_spread() {
        let fills = [
            fill("8", true),
            fill("6", true),
            fill("-2", false),
            fill("-4", false),
        ];
        let report = ExecutionReport::of(&fills, 0, 1_000, 0);

        let taker = report.slippage.taker.expect("taker fills");
        let maker = report.slippage.maker.expect("maker fills");
        assert_eq!(taker.n, 2);
        assert_eq!(taker.mean_bps, d("7"));
        assert_eq!(maker.n, 2);
        assert_eq!(maker.mean_bps, d("-3"));
        // The blend is 2 bps and says nothing useful about either side.
        assert_eq!(report.all.expect("all fills").mean_bps, d("2"));
    }

    /// **Price improvement is reported, not clamped.** The guardrail's own
    /// slippage floors at zero because only the costly direction can refuse an
    /// order; a TCA mean that floors at zero is biased upward by every fill
    /// that went well, which makes the maker/taker comparison meaningless in
    /// the direction it most needs to work.
    #[test]
    fn a_fill_better_than_arrival_pulls_the_mean_down() {
        let improved = ExecutionReport::of(&[fill("-6", false), fill("2", false)], 0, 1, 0);
        assert_eq!(improved.all.expect("stats").mean_bps, d("-2"));
    }

    /// A statistic with no sample is absent, not zero. `docs/specs/fair-value.md`
    /// §13.4 divergence 21: a zero that means "unknown" is the worst possible
    /// encoding, and an execution report is read to decide whether to keep
    /// running an agent.
    #[test]
    fn an_empty_sample_has_no_statistic_rather_than_a_zero_one() {
        let report = ExecutionReport::of(&[], 0, 1, 0);
        assert!(report.all.is_none());
        assert!(report.slippage.taker.is_none());
        assert!(report.slippage.maker.is_none());
        assert_eq!(report.scored_fills, 0);
        assert_eq!(report.costs.n, 0);
        assert!(report.costs.fee_bps_of_notional.is_none());

        // And one that is all takers has no maker baseline, which is itself
        // the finding for an agent that never rests.
        let crossing = ExecutionReport::of(&[fill("5", true)], 0, 1, 0);
        assert!(crossing.slippage.maker.is_none());
        assert_eq!(crossing.slippage.taker.expect("taker").n, 1);
    }

    /// Every stat carries its own `n`, and the report carries the count of
    /// what it could **not** score. A report that silently dropped the
    /// unscoreable rows would put a clean number on an account half of whose
    /// trading it never saw.
    #[test]
    fn what_the_report_could_not_score_is_counted_rather_than_dropped() {
        let report = ExecutionReport::of(&[fill("4", true)], 0, 1, 7);
        assert_eq!(report.scored_fills, 1);
        assert_eq!(report.unscored_fills, 7);
        assert_eq!(report.all.expect("stats").n, 1);
        assert_eq!(report.by_symbol[0].all.as_ref().expect("stats").n, 1);
    }

    /// Costs stay separated. `docs/specs/history.md` §5 keeps `fee` and
    /// `closed_pnl` apart in storage because they are fixed by different
    /// decisions, and netting them here would undo that at the last step.
    #[test]
    fn fees_and_realized_pnl_are_reported_apart() {
        let report = ExecutionReport::of(&[fill("4", true), fill("4", true)], 0, 1, 0);
        assert_eq!(report.costs.closed_pnl_usd, d("24"));
        assert_eq!(report.costs.fees_usd, d("0.70"));
        assert_eq!(report.costs.notional_usd, d("2000"));
        // 0.70 of 2,000 is 3.5 bps.
        assert_eq!(report.costs.fee_bps_of_notional, Some(d("3.5")));
    }

    /// Nearest-rank, so every percentile the report prints is a slippage that
    /// actually happened and can be found in the ledger next to it.
    #[test]
    fn percentiles_are_values_that_really_occurred() {
        let fills: Vec<ScoredFill> = ["1", "2", "3", "4", "100"]
            .iter()
            .map(|slip| fill(slip, true))
            .collect();
        let stat = ExecutionReport::of(&fills, 0, 1, 0).all.expect("stats");
        assert_eq!(stat.n, 5);
        assert_eq!(stat.median_bps, d("3"));
        assert_eq!(stat.p90_bps, d("100"), "the tail is where the damage is");
    }

    /// A row missing any required field is skipped rather than defaulted: a
    /// fill with no fee is not a free fill, and a fill with no slippage is one
    /// nobody can score.
    #[test]
    fn a_payload_missing_a_field_is_not_scored() {
        let complete = json!({
            "coin": "BTC", "agent_id": "alpha", "ts_ms": 5,
            "slip_bps": "3", "px": "100", "sz": "2",
            "fee": "0.1", "closed_pnl": "4", "crossed": true,
        });
        assert!(ScoredFill::from_payload(&complete).is_some());

        for missing in [
            "slip_bps",
            "fee",
            "closed_pnl",
            "crossed",
            "agent_id",
            "px",
            "sz",
            "coin",
            "ts_ms",
        ] {
            let mut payload = complete.clone();
            payload.as_object_mut().expect("object").remove(missing);
            assert!(
                ScoredFill::from_payload(&payload).is_none(),
                "a row with no {missing} must not be scored"
            );
        }
    }

    /// Symbols come out in one order whatever order the fills arrived in
    /// (`AGENTS.md` invariant 6).
    #[test]
    fn the_per_symbol_breakdown_has_one_serialization() {
        let named = |symbol: &str| ScoredFill {
            symbol: symbol.to_owned(),
            ..fill("3", true)
        };
        let forward = ExecutionReport::of(&[named("BTC"), named("ETH"), named("SOL")], 0, 1, 0);
        let backward = ExecutionReport::of(&[named("SOL"), named("BTC"), named("ETH")], 0, 1, 0);
        let symbols: Vec<&str> = forward
            .by_symbol
            .iter()
            .map(|row| row.symbol.as_str())
            .collect();
        assert_eq!(symbols, ["BTC", "ETH", "SOL"]);
        assert_eq!(forward.by_symbol, backward.by_symbol);
    }
}
