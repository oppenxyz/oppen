//! Live testnet smoke test for the info client. Ignored by default so CI
//! stays hermetic; run with `cargo test -p oppen-hl --test testnet_info -- --ignored`.

use oppen_hl::info::OrderRef;
use oppen_hl::types::OrderStatusResponse;
use oppen_hl::{Address, InfoClient, Network, Universe};

#[tokio::test]
#[ignore = "hits the public testnet"]
async fn info_endpoints_round_trip() {
    let info = InfoClient::new(Network::Testnet).unwrap();
    let user = Address::parse("0x0000000000000000000000000000000000000001").unwrap();

    let meta = info.meta().await.unwrap();
    let universe = Universe::from_meta(&meta);
    let btc = universe.get("BTC").unwrap();
    assert_eq!(btc.sz_decimals(), 5);

    let ctxs = info.meta_and_asset_ctxs().await.unwrap();
    assert_eq!(ctxs.0.universe.len(), ctxs.1.len());

    let book = info.l2_book("BTC", Some(5)).await.unwrap();
    assert!(!book.bids().is_empty() && !book.asks().is_empty());
    assert!(book.bids()[0].px < book.asks()[0].px);

    let mids = info.all_mids().await.unwrap();
    assert!(mids.contains_key("BTC"));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let candles = info
        .candles("BTC", "1m", now - 5 * 60_000, now)
        .await
        .unwrap();
    assert!(!candles.is_empty());

    let fundings = info.predicted_fundings().await.unwrap();
    assert!(
        fundings
            .iter()
            .any(|f| f.coin() == "BTC" && f.hyperliquid().is_some())
    );

    let state = info.clearinghouse_state(user).await.unwrap();
    assert!(state.time > 0);
    let _ = info.frontend_open_orders(user).await.unwrap();
    let _ = info.user_fills_by_time(user, 0, None).await.unwrap();
    let rl = info.user_rate_limit(user).await.unwrap();
    assert!(rl.n_requests_cap > 0);
    let subs = info.sub_accounts(user).await.unwrap();
    assert!(subs.is_empty());
    let status = info.order_status(user, OrderRef::Oid(1)).await.unwrap();
    assert_eq!(status, OrderStatusResponse::UnknownOid);
}
