//! The assembled account view (`docs/spec.md` item 16, items 31-34).
//!
//! One view, two transports. The MCP `get_state` tool and the operator
//! console's readouts are the same question asked over Tauri instead of HTTP,
//! and building them separately would produce two answers that drift
//! (`docs/decisions.md` A3).
//!
//! [`assemble`] is a pure function of venue responses so it can be tested
//! without a network, and so the equity arithmetic below is checkable rather
//! than merely asserted.

use rust_decimal::Decimal;
use serde::Serialize;

use oppen_hl::types::{ClearinghouseState, OpenOrder, ReferencePrices, SpotClearinghouseState};
use oppen_hl::{Address, Network};

use crate::guardrail::LossBudget;

/// How stale a feed may be before the console covers it and execution fails
/// closed (`docs/spec.md` item 34).
pub const STALE_AFTER_MS: u64 = 10_000;

/// One open position, with the numbers an agent needs to size its next order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PositionView {
    pub symbol: String,
    /// Signed: negative is short.
    pub size: Decimal,
    pub entry_px: Option<Decimal>,
    pub position_value_usd: Decimal,
    pub unrealized_pnl_usd: Decimal,
    pub margin_used_usd: Decimal,
    pub liquidation_px: Option<Decimal>,
    /// Distance to liquidation as a fraction of the mark, `None` when the
    /// venue publishes no liquidation price or no mark is available.
    ///
    /// Given rather than left to the caller because every caller wants it and
    /// the sign convention is easy to get backwards: it is always
    /// non-negative, and it is the move *against* the position that liquidates
    /// it.
    pub liq_distance_frac: Option<Decimal>,
    /// The same distance in daily standard deviations (`docs/spec.md` spec F).
    ///
    /// `None` from [`assemble`], which has no volatility to measure against —
    /// the `get_state` tool fills it, and deliberately not `read_state`, so the
    /// guardrail path never pays a candle fetch for a field it does not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liq_distance_sigma: Option<Decimal>,
    /// Funding on this position over a day, signed: negative is what it pays.
    /// Filled by the same path as [`PositionView::liq_distance_sigma`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carry_usd_per_day: Option<Decimal>,
    pub max_leverage: u32,
}

/// One resting order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderView {
    pub symbol: String,
    pub oid: u64,
    pub cloid: Option<String>,
    pub is_buy: bool,
    pub limit_px: Decimal,
    pub size: Decimal,
    pub original_size: Decimal,
    pub reduce_only: bool,
    pub is_trigger: bool,
    pub trigger_px: Option<Decimal>,
    pub placed_ts_ms: u64,
}

/// How the account's money is arranged, and why the obvious field is not it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Balances {
    /// Total account value: perps collateral plus spot holdings.
    ///
    /// **Not** `clearinghouseState.marginSummary.accountValue`, which reports
    /// only what is committed to the perps clearinghouse and reads 0.0 on a
    /// funded unified account. **Not** `webData2.cumLedger` either — that is
    /// cumulative net deposits and equals equity only while PnL is zero.
    pub equity_usd: Decimal,
    /// The perps half of the above, kept separate because a guardrail that
    /// cares about margin cares about this and not about spot.
    pub perps_account_value_usd: Decimal,
    /// Free spot USDC — total minus whatever resting spot orders reserve.
    pub spot_usdc_available: Decimal,
    pub total_margin_used_usd: Decimal,
    pub withdrawable_usd: Decimal,
}

/// Whether a feed can be believed right now (`docs/spec.md` item 34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// A tick arrived within [`STALE_AFTER_MS`].
    Live,
    /// Nothing recent. The console covers the panel and execution fails
    /// closed; this is not a warning the operator may dismiss.
    Stale,
    /// Nothing has ever arrived, which is different from having gone quiet:
    /// there is no last-good value to show behind the overlay.
    NeverConnected,
}

/// The whole answer, versioned so an agent can tell one shape from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountState {
    /// Bumped when a field changes meaning, not when one is added.
    pub contract_version: u32,
    pub network: &'static str,
    pub address: Address,
    pub as_of_ms: u64,
    /// Milliseconds since the newest tick on any subscribed feed, `None` when
    /// nothing has ever arrived.
    pub feed_age_ms: Option<u64>,
    pub feed: Freshness,
    pub balances: Balances,
    /// Hours of free margin against the account's total funding bleed
    /// (`docs/spec.md` spec F).
    ///
    /// **Account-level, because free margin is shared.** A runway computed per
    /// position would divide the same margin among all of them and overstate
    /// each by the position count — the direction that makes an account look
    /// safer than it is. `None` when nothing is being paid, or from
    /// [`assemble`], which has no funding rates; see
    /// [`PositionView::carry_usd_per_day`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_runway_h: Option<Decimal>,
    /// Every loss budget the account is measured against, as a dial rather
    /// than as the step function the breaker is (`docs/spec.md` spec F).
    ///
    /// Empty from [`assemble`], which has no engine to ask; the gateway fills
    /// it in `get_state` for the same reason as [`AccountState::margin_runway_h`]
    /// (`docs/decisions.md` K1). Empty also when no budget is configured — an
    /// operator who set none has no gauge, and a zero row would say the
    /// opposite of what that means.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub loss_budget: Vec<LossBudget>,
    pub positions: Vec<PositionView>,
    pub orders: Vec<OrderView>,
}

/// Assemble the view from venue responses.
///
/// `mids` supplies the mark used for liquidation distance; a symbol missing
/// from it yields `None` rather than a guess, because a fabricated liquidation
/// distance is worse than an absent one.
pub struct VenueReadings<'a> {
    pub perps: &'a ClearinghouseState,
    pub spot: &'a SpotClearinghouseState,
    pub orders: &'a [OpenOrder],
    /// The venue's marks, for the assets it is actually quoting. A symbol
    /// absent here yields no liquidation distance rather than a guessed one —
    /// and [`ReferencePrices`] is what makes absence reachable: the map this
    /// used to take was `allMids`, which answers for an unquoted asset with a
    /// frozen last print, so the guess this comment forbids was being made.
    pub mids: &'a ReferencePrices,
    /// Newest tick across subscribed feeds, `None` if nothing ever arrived.
    pub last_tick_ms: Option<u64>,
}

pub fn assemble(
    network: Network,
    address: Address,
    as_of_ms: u64,
    readings: &VenueReadings<'_>,
) -> AccountState {
    let VenueReadings {
        perps,
        spot,
        orders,
        mids,
        last_tick_ms,
    } = *readings;
    let spot_usdc_total = spot.usdc_total();
    let spot_usdc_held = spot
        .balances
        .iter()
        .find(|balance| balance.coin == "USDC")
        .map(|balance| balance.hold)
        .unwrap_or_default();

    let balances = Balances {
        // Unified margin: the spot USDC *is* perps collateral, so equity is
        // the sum and not either half.
        equity_usd: perps.margin_summary.account_value + spot_usdc_total,
        perps_account_value_usd: perps.margin_summary.account_value,
        spot_usdc_available: spot_usdc_total - spot_usdc_held,
        total_margin_used_usd: perps.margin_summary.total_margin_used,
        withdrawable_usd: perps.withdrawable,
    };

    let positions = perps
        .asset_positions
        .iter()
        .map(|held| {
            let position = &held.position;
            let mark = mids.get(&position.coin);
            PositionView {
                symbol: position.coin.clone(),
                size: position.szi,
                entry_px: position.entry_px,
                position_value_usd: position.position_value,
                unrealized_pnl_usd: position.unrealized_pnl,
                margin_used_usd: position.margin_used,
                liquidation_px: position.liquidation_px,
                liq_distance_frac: liq_distance(position.liquidation_px, mark),
                // Filled by the caller that has the volatility and funding to
                // hand; see the field docs.
                liq_distance_sigma: None,
                carry_usd_per_day: None,
                max_leverage: position.max_leverage,
            }
        })
        .collect();

    let orders = orders
        .iter()
        .map(|order| OrderView {
            symbol: order.coin.clone(),
            oid: order.oid,
            cloid: order.cloid.as_ref().map(|cloid| cloid.as_str().to_owned()),
            is_buy: order.side.is_buy(),
            limit_px: order.limit_px,
            size: order.sz,
            original_size: order.orig_sz,
            reduce_only: order.reduce_only,
            is_trigger: order.is_trigger,
            trigger_px: order.trigger_px,
            placed_ts_ms: order.timestamp,
        })
        .collect();

    let feed_age_ms = last_tick_ms.map(|tick| as_of_ms.saturating_sub(tick));
    let feed = match feed_age_ms {
        None => Freshness::NeverConnected,
        Some(age) if age <= STALE_AFTER_MS => Freshness::Live,
        Some(_) => Freshness::Stale,
    };

    AccountState {
        contract_version: 0,
        network: match network {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        },
        address,
        as_of_ms,
        feed_age_ms,
        feed,
        balances,
        // Filled by the caller that has the funding rates; see the field docs.
        margin_runway_h: None,
        loss_budget: Vec::new(),
        positions,
        orders,
    }
}

/// `|mark - liquidation| / mark`, or `None` when either is unavailable.
fn liq_distance(liquidation_px: Option<Decimal>, mark: Option<Decimal>) -> Option<Decimal> {
    let (liq, mark) = (liquidation_px?, mark?);
    if mark.is_zero() {
        return None;
    }
    Some(((mark - liq) / mark).abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::{MarginSummary, SpotBalance};

    fn d(s: &str) -> Decimal {
        s.parse().expect("decimal")
    }

    fn addr() -> Address {
        "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2"
            .parse()
            .expect("address")
    }

    fn perps(account_value: &str, margin_used: &str, withdrawable: &str) -> ClearinghouseState {
        let summary = MarginSummary {
            account_value: d(account_value),
            total_ntl_pos: Decimal::ZERO,
            total_raw_usd: d(account_value),
            total_margin_used: d(margin_used),
        };
        ClearinghouseState {
            margin_summary: summary.clone(),
            cross_margin_summary: summary,
            cross_maintenance_margin_used: Decimal::ZERO,
            withdrawable: d(withdrawable),
            asset_positions: Vec::new(),
            time: 0,
        }
    }

    fn spot(total: &str, hold: &str) -> SpotClearinghouseState {
        SpotClearinghouseState {
            balances: vec![SpotBalance {
                coin: "USDC".into(),
                token: 0,
                total: d(total),
                hold: d(hold),
            }],
        }
    }

    fn state_of(perps: &ClearinghouseState, spot: &SpotClearinghouseState) -> AccountState {
        assemble(
            Network::Testnet,
            addr(),
            1_788_544_667_000,
            &VenueReadings {
                perps,
                spot,
                orders: &[],
                mids: &ReferencePrices::default(),
                last_tick_ms: Some(1_788_544_666_000),
            },
        )
    }

    /// The exact shape that made `accountValue` look like the balance: a
    /// funded account whose perps clearinghouse holds nothing.
    #[test]
    fn equity_is_the_sum_and_not_the_perps_field() {
        let state = state_of(&perps("0.0", "0.0", "0.0"), &spot("999.0", "0.0"));
        assert_eq!(
            state.balances.equity_usd,
            d("999.0"),
            "the funded account read as empty"
        );
        assert_eq!(state.balances.perps_account_value_usd, Decimal::ZERO);
    }

    /// Once margin is committed the two halves are both non-zero, and equity
    /// is still their sum rather than either one.
    #[test]
    fn equity_sums_both_halves_once_margin_is_committed() {
        let state = state_of(&perps("120.5", "40.0", "80.5"), &spot("878.5", "0.0"));
        assert_eq!(state.balances.equity_usd, d("999.0"));
        assert_eq!(state.balances.total_margin_used_usd, d("40.0"));
        assert_eq!(state.balances.withdrawable_usd, d("80.5"));
    }

    /// `hold` is USDC reserved by resting spot orders. It counts toward
    /// equity, because the account still owns it, and not toward what is
    /// available to commit.
    #[test]
    fn held_spot_usdc_counts_as_equity_but_not_as_available() {
        let state = state_of(&perps("0.0", "0.0", "0.0"), &spot("999.0", "250.0"));
        assert_eq!(state.balances.equity_usd, d("999.0"));
        assert_eq!(state.balances.spot_usdc_available, d("749.0"));
    }

    #[test]
    fn an_account_holding_no_usdc_reads_zero_rather_than_failing() {
        let state = state_of(
            &perps("0.0", "0.0", "0.0"),
            &SpotClearinghouseState {
                balances: Vec::new(),
            },
        );
        assert_eq!(state.balances.equity_usd, Decimal::ZERO);
        assert_eq!(state.balances.spot_usdc_available, Decimal::ZERO);
    }

    #[test]
    fn a_recent_tick_is_live_and_an_old_one_is_stale() {
        let (perps, spot) = (perps("0.0", "0.0", "0.0"), spot("999.0", "0.0"));
        let at = |tick: Option<u64>| {
            assemble(
                Network::Testnet,
                addr(),
                1_000_000,
                &VenueReadings {
                    perps: &perps,
                    spot: &spot,
                    orders: &[],
                    mids: &ReferencePrices::default(),
                    last_tick_ms: tick,
                },
            )
        };
        assert_eq!(at(Some(1_000_000 - STALE_AFTER_MS)).feed, Freshness::Live);
        assert_eq!(
            at(Some(1_000_000 - STALE_AFTER_MS - 1)).feed,
            Freshness::Stale
        );
        assert_eq!(at(None).feed, Freshness::NeverConnected);
        assert_eq!(at(None).feed_age_ms, None);
    }

    /// A clock that reads behind the last tick must not wrap into a huge age
    /// and report a live feed as stale.
    #[test]
    fn a_tick_from_the_future_does_not_wrap_the_age() {
        let (perps, spot) = (perps("0.0", "0.0", "0.0"), spot("999.0", "0.0"));
        let state = assemble(
            Network::Testnet,
            addr(),
            1_000_000,
            &VenueReadings {
                perps: &perps,
                spot: &spot,
                orders: &[],
                mids: &ReferencePrices::default(),
                last_tick_ms: Some(1_000_500),
            },
        );
        assert_eq!(state.feed_age_ms, Some(0));
        assert_eq!(state.feed, Freshness::Live);
    }

    #[test]
    fn liquidation_distance_is_a_non_negative_fraction_either_side() {
        // Long: liquidation below the mark.
        assert_eq!(liq_distance(Some(d("90")), Some(d("100"))), Some(d("0.1")));
        // Short: liquidation above it. Same distance, same sign.
        assert_eq!(liq_distance(Some(d("110")), Some(d("100"))), Some(d("0.1")));
    }

    #[test]
    fn liquidation_distance_is_absent_rather_than_guessed() {
        assert_eq!(
            liq_distance(None, Some(d("100"))),
            None,
            "no liquidation price"
        );
        assert_eq!(
            liq_distance(Some(d("90")), None),
            None,
            "no mark for the symbol"
        );
        assert_eq!(
            liq_distance(Some(d("90")), Some(Decimal::ZERO)),
            None,
            "mark of zero"
        );
    }

    #[test]
    fn the_network_is_named_in_the_envelope() {
        let (perps, spot) = (perps("0.0", "0.0", "0.0"), spot("0.0", "0.0"));
        let named = |network| {
            assemble(
                network,
                addr(),
                0,
                &VenueReadings {
                    perps: &perps,
                    spot: &spot,
                    orders: &[],
                    mids: &ReferencePrices::default(),
                    last_tick_ms: None,
                },
            )
            .network
        };
        assert_eq!(named(Network::Testnet), "testnet");
        assert_eq!(named(Network::Mainnet), "mainnet");
    }

    // -- bridge to the guardrail engine --------------------------------------

    use oppen_hl::types::{Fill, Side};

    fn fill(time: u64, closed_pnl: &str, fee: &str) -> Fill {
        Fill {
            coin: "BTC".into(),
            px: d("80000"),
            sz: d("0.001"),
            side: Side::B,
            time,
            start_position: Decimal::ZERO,
            dir: "Open Long".into(),
            closed_pnl: d(closed_pnl),
            hash: String::new(),
            oid: 1,
            crossed: true,
            fee: d(fee),
            fee_token: "USDC".into(),
            builder_fee: None,
            tid: time,
            cloid: None,
        }
    }

    #[test]
    fn realized_pnl_is_net_of_fees_and_windowed() {
        let fills = [
            fill(100, "10", "1"),
            fill(200, "-4", "0.5"),
            fill(50, "1000", "0"),
        ];
        // The 1000 sits before the window and must not count.
        assert_eq!(realized_pnl_since(&fills, 100), d("4.5"));
    }

    #[test]
    fn a_fee_only_fill_is_a_loss() {
        assert_eq!(realized_pnl_since(&[fill(100, "0", "0.75")], 0), d("-0.75"));
    }

    #[test]
    fn the_utc_day_starts_at_midnight() {
        // 2026-09-04T17:57:47.126Z -> 2026-09-04T00:00:00Z
        assert_eq!(utc_day_start_ms(1_788_544_667_126), 1_788_480_000_000);
        assert_eq!(
            utc_day_start_ms(1_788_480_000_000),
            1_788_480_000_000,
            "midnight is its own start"
        );
    }

    #[test]
    fn unreconciled_is_carried_through_rather_than_softened() {
        let state = state_of(&perps("0.0", "0.0", "0.0"), &spot("999.0", "0.0"));
        let exposure = exposure_from(&state, Decimal::ZERO, None, false, 0);
        assert!(
            !exposure.agent.reconciled,
            "the engine must see the account as unreconciled"
        );
    }

    #[test]
    fn an_absent_peak_falls_back_to_current_equity_not_to_zero() {
        let state = state_of(&perps("0.0", "0.0", "0.0"), &spot("999.0", "0.0"));
        let exposure = exposure_from(&state, Decimal::ZERO, None, true, 0);
        // A zero peak would report the whole balance as drawdown and refuse
        // everything; a peak below equity would report a negative one.
        assert_eq!(exposure.agent.peak_equity_usd, d("999.0"));
    }

    #[test]
    fn a_peak_below_current_equity_is_raised_to_it() {
        let state = state_of(&perps("0.0", "0.0", "0.0"), &spot("999.0", "0.0"));
        let exposure = exposure_from(&state, Decimal::ZERO, Some(d("500")), true, 0);
        assert_eq!(
            exposure.agent.peak_equity_usd,
            d("999.0"),
            "drawdown must never be negative"
        );
    }

    #[test]
    fn resting_orders_are_signed_and_summed_per_symbol() {
        use oppen_hl::types::{OpenOrder, Side};
        let order = |coin: &str, buy: bool, sz: &str, px: &str| OpenOrder {
            coin: coin.into(),
            side: if buy { Side::B } else { Side::A },
            limit_px: d(px),
            sz: d(sz),
            orig_sz: d(sz),
            oid: 1,
            timestamp: 0,
            order_type: "Limit".into(),
            reduce_only: false,
            is_trigger: false,
            trigger_px: None,
            trigger_condition: None,
            is_position_tpsl: false,
            cloid: None,
        };
        let perps = perps("0.0", "0.0", "0.0");
        let spot = spot("999.0", "0.0");
        let orders = [
            order("BTC", true, "2", "100"),
            order("BTC", false, "0.5", "100"),
        ];
        let state = assemble(
            Network::Testnet,
            addr(),
            0,
            &VenueReadings {
                perps: &perps,
                spot: &spot,
                orders: &orders,
                mids: &ReferencePrices::default(),
                last_tick_ms: None,
            },
        );
        let exposure = exposure_from(&state, Decimal::ZERO, None, true, 0);
        let resting = exposure.agent.resting.expect("resting supplied");
        assert_eq!(
            resting.szi.get("BTC"),
            Some(&d("1.5")),
            "buys and sells net per symbol"
        );
        // Notional is the everything-fills reading and does not net.
        assert_eq!(resting.notional_usd, d("250"));
    }

    /// The bridge feeds the real engine, and the refusal it produces changes
    /// with `reconciled` — which is the whole reason that flag is passed in
    /// honestly rather than defaulted to `true`.
    ///
    /// Unreconciled refuses first and masks every other predicate. Reconciled,
    /// the same order reaches the symbol allowlist, which is D-c's "the
    /// refusal is the onboarding" moment.
    #[test]
    fn the_reconciled_flag_decides_which_refusal_the_agent_sees() {
        use crate::guardrail::{
            AgentId, FeedQuality, GuardrailEngine, MarketRef, OrderIntent, SqliteGuardrailStore,
        };
        use crate::ledger::{Ledger, LedgerAuditSink};
        use oppen_hl::OrderKind;
        use oppen_hl::wire::{Grouping, Tif};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let ledger =
            Arc::new(Ledger::open_at(&dir.path().join("t.db"), Network::Testnet).expect("ledger"));
        let engine = GuardrailEngine::new(
            Arc::new(SqliteGuardrailStore::open(dir.path().join("g.db")).expect("store")),
            Arc::new(LedgerAuditSink::new(ledger)),
            Arc::new(crate::keys::KeychainKeyStore::new(Network::Testnet)),
            Network::Testnet,
        )
        .expect("engine");

        let agent = AgentId::new("agent-alpha");
        engine
            .register_agent(&agent, None, 1_000)
            .expect("register");

        let meta: oppen_hl::types::Meta = serde_json::from_str(
            r#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":40}]}"#,
        )
        .expect("meta");
        let universe = oppen_hl::Universe::from_meta(&meta).expect("universe");
        let asset = universe.get("BTC").expect("btc");

        let state = state_of(&perps("0.0", "0.0", "0.0"), &spot("999.0", "0.0"));
        let market = MarketRef {
            symbol: "BTC".into(),
            reference_px: Some(d("80000")),
            as_of_ms: 1_788_544_667_000,
            quality: FeedQuality::Ok,
            mark_divergence_bps: None,
            mark_divergent_since_ms: None,
            snapshot: None,
        };
        let intent = OrderIntent {
            symbol: "BTC".into(),
            is_buy: true,
            px: d("72000"),
            sz: d("0.0002"),
            kind: OrderKind::Limit { tif: Tif::Gtc },
            reduce_only: false,
            cloid: None,
            grouping: Grouping::Na,
            builder: None,
            max_slippage_bps: None,
            reason: "probing".into(),
        };

        let refuse = |reconciled: bool| {
            let exposure = exposure_from(
                &state,
                Decimal::ZERO,
                None,
                reconciled,
                utc_day_start_ms(1_788_544_667_000),
            );
            engine
                .evaluate(
                    &agent,
                    &intent,
                    asset,
                    &market,
                    &exposure,
                    1_788_544_667_000,
                )
                .err()
                .map(|refusal| refusal.to_string())
                .expect("an order must not clear under D-c defaults")
        };

        let unreconciled = refuse(false);
        assert!(
            unreconciled.contains("reconcil"),
            "unreconciled state must refuse first, got: {unreconciled}"
        );

        let reconciled = refuse(true);
        assert!(
            !reconciled.contains("reconcil"),
            "reconciled state still refused on reconciliation: {reconciled}"
        );
        assert!(
            reconciled.contains("BTC") || reconciled.to_lowercase().contains("allow"),
            "expected the symbol allowlist to be the next refusal, got: {reconciled}"
        );
    }
}

/// Live checks against the public testnet. Ignored by default, like
/// `oppen-hl`'s `info_endpoints_round_trip`: they need a network and a funded
/// address, so they document rather than gate.
///
/// Run with `cargo test -p oppen-core --  --ignored live_`.
#[cfg(test)]
mod live {
    use super::*;
    use oppen_hl::InfoClient;

    const FUNDED: &str = "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2";

    /// The bug this module exists to prevent, checked against the venue rather
    /// than against a fixture: equity must match what the perps ticket shows,
    /// while `accountValue` alone reads zero.
    #[tokio::test]
    #[ignore = "hits the public testnet"]
    async fn live_equity_matches_the_venue() {
        let address: Address = FUNDED.parse().expect("address");
        let info = InfoClient::new(Network::Testnet).expect("client");

        let perps = info.clearinghouse_state(address).await.expect("perps");
        let spot = info.spot_clearinghouse_state(address).await.expect("spot");
        let orders = info.frontend_open_orders(address).await.expect("orders");
        // The same source the gateway uses: `allMids` would answer for an
        // asset the venue has stopped quoting, with a frozen last print.
        let mids = info
            .meta_and_asset_ctxs()
            .await
            .expect("asset contexts")
            .reference_pxs();

        let state = assemble(
            Network::Testnet,
            address,
            0,
            &VenueReadings {
                perps: &perps,
                spot: &spot,
                orders: &orders,
                mids: &mids,
                last_tick_ms: None,
            },
        );

        println!("equity      {}", state.balances.equity_usd);
        println!("perps only  {}", state.balances.perps_account_value_usd);
        println!("available   {}", state.balances.spot_usdc_available);
        println!("positions   {}", state.positions.len());
        println!("orders      {}", state.orders.len());

        assert!(
            state.balances.equity_usd > Decimal::ZERO,
            "assembled equity is {} on a funded account — the unified-margin bug is back",
            state.balances.equity_usd
        );
        assert_eq!(
            state.balances.equity_usd,
            perps.margin_summary.account_value + spot.usdc_total(),
            "equity drifted from its own definition"
        );
    }
}

// ---------------------------------------------------------------------------
// Bridge to the guardrail engine (spec items 24-25).
// ---------------------------------------------------------------------------

/// Realised PnL and fees over a window, summed from the venue's own fills.
///
/// Fees are subtracted rather than reported beside the PnL: a daily-loss limit
/// that ignores fees is not a loss limit, and `closedPnl` is gross.
pub fn realized_pnl_since(fills: &[oppen_hl::types::Fill], since_ms: u64) -> Decimal {
    fills
        .iter()
        .filter(|fill| fill.time >= since_ms)
        .map(|fill| fill.closed_pnl - fill.fee)
        .sum()
}

/// Start of the UTC day containing `now_ms`. The day the loss limit resets on.
pub fn utc_day_start_ms(now_ms: u64) -> u64 {
    const DAY: u64 = 86_400_000;
    now_ms - (now_ms % DAY)
}

/// Turn the assembled view into the exposure the engine measures caps against.
///
/// `reconciled` is passed in rather than inferred, and it is the caller's
/// honest answer to "has spec item 9's reconcile completed since the last
/// disconnect". The engine refuses on `false`, which is the point: caps are
/// measured against position sizes, so an unreconciled account means it does
/// not know what it is measuring.
///
/// `peak_equity_usd` comes from the venue's `portfolio` window because nothing
/// local can reconstruct a high-water mark that predates oppen's first run.
/// Absent, it falls back to current equity — the conservative direction, since
/// a peak equal to today's equity reports zero drawdown and lets the *other*
/// limits do the refusing rather than inventing a drawdown that did not
/// happen.
pub fn exposure_from(
    state: &AccountState,
    realized_pnl_today_usd: Decimal,
    peak_equity_usd: Option<Decimal>,
    reconciled: bool,
    day_start_ms: u64,
) -> crate::guardrail::Exposure {
    use crate::guardrail::{AccountSnapshot, Exposure, PositionSnapshot, RestingExposure};
    use std::collections::BTreeMap;

    let equity = state.balances.equity_usd;
    let unrealized: Decimal = state.positions.iter().map(|p| p.unrealized_pnl_usd).sum();

    let positions: BTreeMap<String, PositionSnapshot> = state
        .positions
        .iter()
        .map(|p| (p.symbol.clone(), PositionSnapshot { szi: p.size }))
        .collect();

    let mut resting_szi: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut resting_notional = Decimal::ZERO;
    for order in &state.orders {
        // Signed: a working buy adds, a working sell subtracts. This is the
        // "everything fills" reading, which is what a cap must be measured
        // against.
        let signed = if order.is_buy {
            order.size
        } else {
            -order.size
        };
        *resting_szi.entry(order.symbol.clone()).or_default() += signed;
        resting_notional += (order.size * order.limit_px).abs();
    }

    let agent = AccountSnapshot {
        as_of_ms: state.as_of_ms,
        reconciled,
        equity_usd: equity,
        peak_equity_usd: peak_equity_usd.unwrap_or(equity).max(equity),
        realized_pnl_today_usd,
        unrealized_pnl_usd: unrealized,
        day_start_ms,
        total_position_notional_usd: state
            .positions
            .iter()
            .map(|p| p.position_value_usd.abs())
            .sum(),
        positions,
        resting: Some(RestingExposure {
            szi: resting_szi,
            notional_usd: resting_notional,
        }),
    };

    // One container per agent (D1 as revised), so the fleet aggregate is the
    // same account. A second container would make these differ.
    Exposure {
        agent: agent.clone(),
        fleet: Some(agent),
    }
}
