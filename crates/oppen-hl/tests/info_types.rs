//! Info response types against fixtures captured from the live testnet on
//! 2026-09-03 (`tests/fixtures/`), trimmed to a few assets.

use oppen_hl::Universe;
use oppen_hl::types::*;

#[test]
fn meta_and_asset_ctxs() {
    let MetaAndAssetCtxs(meta, ctxs) =
        serde_json::from_str(include_str!("fixtures/meta_and_asset_ctxs.json")).unwrap();
    assert_eq!(meta.universe.len(), ctxs.len());
    let sol = &ctxs[0];
    assert_eq!(sol.funding.to_string(), "0.0000125");
    assert_eq!(sol.impact_pxs.as_ref().unwrap()[0].to_string(), "104.7452");
    assert_eq!(sol.mid_px.unwrap().to_string(), "104.985");
    let u = Universe::from_meta(&meta);
    assert_eq!(u.len(), 4);
}

#[test]
fn l2_book() {
    let book: L2Book = serde_json::from_str(include_str!("fixtures/l2Book.json")).unwrap();
    assert_eq!(book.coin, "BTC");
    assert_eq!(book.bids().len(), 20);
    assert!(book.bids()[0].px < book.asks()[0].px);
    assert!(book.bids()[0].px > book.bids()[1].px, "bids best-first");
    assert!(book.asks()[0].px < book.asks()[1].px, "asks best-first");
}

#[test]
fn candles() {
    let candles: Vec<Candle> =
        serde_json::from_str(include_str!("fixtures/candleSnapshot.json")).unwrap();
    assert_eq!(candles.len(), 10);
    assert_eq!(candles[0].i, "1m");
    assert_eq!(candles[0].t_close - candles[0].t, 59_999);
}

#[test]
fn clearinghouse_state_empty_account() {
    let state: ClearinghouseState =
        serde_json::from_str(include_str!("fixtures/clearinghouseState.json")).unwrap();
    assert!(state.asset_positions.is_empty());
    assert_eq!(
        state.margin_summary.account_value.to_string(),
        "1438.041001"
    );
}

/// DOC-PERPS clearinghouseState example with an open isolated position.
#[test]
fn clearinghouse_state_with_position() {
    let json = r#"{"assetPositions":[{"position":{"coin":"ETH","cumFunding":{"allTime":"514.085417","sinceChange":"0.0","sinceOpen":"0.0"},"entryPx":"2986.3","leverage":{"rawUsd":"-95.059824","type":"isolated","value":20},"liquidationPx":"2866.26936529","marginUsed":"4.967826","maxLeverage":50,"positionValue":"100.02765","returnOnEquity":"-0.0026789","szi":"0.0335","unrealizedPnl":"-0.0134"},"type":"oneWay"}],"crossMaintenanceMarginUsed":"0.0","crossMarginSummary":{"accountValue":"13104.514502","totalMarginUsed":"0.0","totalNtlPos":"0.0","totalRawUsd":"13104.514502"},"marginSummary":{"accountValue":"13109.482328","totalMarginUsed":"4.967826","totalNtlPos":"100.02765","totalRawUsd":"13009.454678"},"time":1708622398623,"withdrawable":"13104.514502"}"#;
    let state: ClearinghouseState = serde_json::from_str(json).unwrap();
    let p = &state.asset_positions[0].position;
    assert_eq!(p.coin, "ETH");
    assert_eq!(p.leverage.kind, "isolated");
    assert_eq!(p.leverage.value, 20);
    assert_eq!(p.liquidation_px.unwrap().to_string(), "2866.26936529");
    assert_eq!(p.szi.to_string(), "0.0335");
}

#[test]
fn fills_and_rate_limit() {
    let fills: Vec<Fill> =
        serde_json::from_str(include_str!("fixtures/userFillsByTime.json")).unwrap();
    assert_eq!(fills[0].side, Side::A);
    assert!(!fills[0].side.is_buy());
    assert!(fills[0].builder_fee.is_none());
    let rl: UserRateLimit =
        serde_json::from_str(include_str!("fixtures/userRateLimit.json")).unwrap();
    assert_eq!(rl.n_requests_cap, 10_000);
}

/// DOC-INFO userFills example including the optional builderFee.
#[test]
fn fill_with_builder_fee() {
    let json = r#"{"closedPnl":"0.0","coin":"AVAX","crossed":false,"dir":"Open Long","hash":"0xa1","oid":90542681,"px":"18.435","side":"B","startPosition":"26.86","sz":"93.53","time":1681222254710,"fee":"0.01","feeToken":"USDC","builderFee":"0.01","tid":118906512037719}"#;
    let fill: Fill = serde_json::from_str(json).unwrap();
    assert_eq!(fill.builder_fee.unwrap().to_string(), "0.01");
    assert!(fill.side.is_buy());
}

/// DOC-INFO frontendOpenOrders example.
#[test]
fn open_orders() {
    let json = r#"[{"coin":"BTC","isPositionTpsl":false,"isTrigger":false,"limitPx":"29792.0","oid":91490942,"orderType":"Limit","origSz":"5.0","reduceOnly":false,"side":"A","sz":"5.0","timestamp":1681247412573,"triggerCondition":"N/A","triggerPx":"0.0"}]"#;
    let orders: Vec<OpenOrder> = serde_json::from_str(json).unwrap();
    assert_eq!(orders[0].oid, 91490942);
    assert!(orders[0].cloid.is_none());
}

#[test]
fn predicted_fundings_pick_hyperliquid() {
    let rows: Vec<PredictedFundings> =
        serde_json::from_str(include_str!("fixtures/predictedFundings.json")).unwrap();
    let btc = rows.iter().find(|r| r.coin() == "BTC").unwrap();
    let hl = btc.hyperliquid().unwrap();
    assert_eq!(hl.funding_interval_hours, Some(1));
    assert!(hl.next_funding_time > 0);
}

#[test]
fn order_status_unknown() {
    let s: OrderStatusResponse = serde_json::from_str(r#"{"status":"unknownOid"}"#).unwrap();
    assert_eq!(s, OrderStatusResponse::UnknownOid);
}
