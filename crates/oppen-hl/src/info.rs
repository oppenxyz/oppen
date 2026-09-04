//! `POST /info` client: read-only venue queries (`docs/spec.md` items 9–11).

use std::collections::HashMap;

use reqwest::Client;
use rust_decimal::Decimal;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::{
    Candle, ClearinghouseState, Fill, FundingHistoryPage, FundingHistoryRow, L2Book, Meta,
    MetaAndAssetCtxs, OpenOrder, OrderStatusResponse, PredictedFundings, SubAccount, UserRateLimit,
};
use crate::wire::Cloid;
use crate::{Address, Error, Network};

/// Query an order by exchange id or client id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum OrderRef {
    Oid(u64),
    Cloid(Cloid),
}

#[derive(Debug, Clone)]
pub struct InfoClient {
    http: Client,
    url: String,
}

impl InfoClient {
    pub fn new(network: Network) -> Result<Self, Error> {
        Ok(Self::with_client(network, Client::builder().build()?))
    }

    pub fn with_client(network: Network, http: Client) -> Self {
        InfoClient {
            http,
            url: format!("{}/info", network.api_url()),
        }
    }

    async fn post<T: DeserializeOwned>(&self, body: serde_json::Value) -> Result<T, Error> {
        let response = self.http.post(&self.url).json(&body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Venue {
                status: status.as_u16(),
                message: text,
            });
        }
        Ok(response.json::<T>().await?)
    }

    pub async fn meta(&self) -> Result<Meta, Error> {
        self.post(serde_json::json!({ "type": "meta" })).await
    }

    pub async fn meta_and_asset_ctxs(&self) -> Result<MetaAndAssetCtxs, Error> {
        self.post(serde_json::json!({ "type": "metaAndAssetCtxs" }))
            .await
    }

    /// Server-side aggregation: `n_sig_figs` in `2..=5`, `None` for the
    /// full-precision book. See `feedback: deep book needs nSigFigs`.
    pub async fn l2_book(&self, coin: &str, n_sig_figs: Option<u8>) -> Result<L2Book, Error> {
        let mut body = serde_json::json!({ "type": "l2Book", "coin": coin });
        if let Some(n) = n_sig_figs {
            body["nSigFigs"] = serde_json::json!(n);
        }
        self.post(body).await
    }

    pub async fn candles(
        &self,
        coin: &str,
        interval: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<Vec<Candle>, Error> {
        self.post(serde_json::json!({
            "type": "candleSnapshot",
            "req": { "coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms },
        }))
        .await
    }

    /// Last-print prices per coin.
    ///
    /// **Never use this to fill a null [`crate::types::AssetCtx::mid_px`].**
    /// `docs/specs/fair-value.md` §14.4 correction 3: `allMids` answers for
    /// all 56 mainnet assets whose `midPx` is `null`, and the value it
    /// answers with is `markPx` — a frozen last print, not a mid. Measured
    /// 2026-09-03, FRIEND reads `4.72` here against an `oraclePx` of
    /// `0.47734`: a **9.9× stale price** that would look like a live quote
    /// to everything downstream. A missing component is dropped and `β`
    /// renormalized over the survivors (§4.3); it is never defaulted.
    ///
    /// What this is good for is a coarse last-price lookup where staleness is
    /// acceptable and stated — not the `micro` component, not the sampler's
    /// target mid, not a fair-value input.
    pub async fn all_mids(&self) -> Result<HashMap<String, Decimal>, Error> {
        self.post(serde_json::json!({ "type": "allMids" })).await
    }

    /// One page of `fundingHistory` from `start_ms` forward.
    ///
    /// Rows carry the **uncensored** hour-average `premium` alongside the
    /// censored `fundingRate`, which `docs/specs/fair-value.md` §14.3 calls
    /// the most valuable finding of the live audit — see
    /// [`FundingHistoryRow`].
    ///
    /// Pagination is forward and capped at
    /// [`crate::types::FUNDING_HISTORY_PAGE_LIMIT`] rows: ask for the next
    /// page with [`FundingHistoryPage::next_start_ms`] and dedupe, because
    /// `startTime` is inclusive. A window wider than the cap truncates
    /// silently from the **newest** end and returns no cursor, so the caller
    /// must page rather than widen (§14.4 correction 12).
    ///
    /// The return type keeps the venue's two empty answers apart. A `null`
    /// body — served with a deterministic HTTP 500 — means the coin is not
    /// listed and is reported as [`FundingHistoryPage::UnlistedCoin`];
    /// genuine no-data is `[]` with HTTP 200 and is
    /// [`FundingHistoryPage::Rows`] holding nothing. Collapsing the two lets
    /// a backfill walk a misspelled coin through all of history recording
    /// nothing and reporting success.
    pub async fn funding_history(
        &self,
        coin: &str,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> Result<FundingHistoryPage, Error> {
        let mut body =
            serde_json::json!({ "type": "fundingHistory", "coin": coin, "startTime": start_ms });
        if let Some(end) = end_ms {
            body["endTime"] = serde_json::json!(end);
        }

        // Read the body before judging the status: the unlisted-coin answer
        // is a `null` payload delivered with an error status, and the
        // distinction it draws is worth more than the status code.
        let response = self.http.post(&self.url).json(&body).send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        parse_funding_history(status, &text)
    }

    pub async fn predicted_fundings(&self) -> Result<Vec<PredictedFundings>, Error> {
        self.post(serde_json::json!({ "type": "predictedFundings" }))
            .await
    }

    pub async fn clearinghouse_state(&self, user: Address) -> Result<ClearinghouseState, Error> {
        self.post(serde_json::json!({ "type": "clearinghouseState", "user": user }))
            .await
    }

    pub async fn frontend_open_orders(&self, user: Address) -> Result<Vec<OpenOrder>, Error> {
        self.post(serde_json::json!({ "type": "frontendOpenOrders", "user": user }))
            .await
    }

    /// Fills in `[start_ms, end_ms]`; the venue caps the window at 2000
    /// most recent fills, so P2's reconcile pages by `start_ms`.
    pub async fn user_fills_by_time(
        &self,
        user: Address,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> Result<Vec<Fill>, Error> {
        let mut body =
            serde_json::json!({ "type": "userFillsByTime", "user": user, "startTime": start_ms });
        if let Some(end) = end_ms {
            body["endTime"] = serde_json::json!(end);
        }
        self.post(body).await
    }

    /// The only safe move after `timeout_unknown_outcome` (`docs/spec.md` item 19).
    pub async fn order_status(
        &self,
        user: Address,
        order: OrderRef,
    ) -> Result<OrderStatusResponse, Error> {
        self.post(serde_json::json!({ "type": "orderStatus", "user": user, "oid": order }))
            .await
    }

    pub async fn user_rate_limit(&self, user: Address) -> Result<UserRateLimit, Error> {
        self.post(serde_json::json!({ "type": "userRateLimit", "user": user }))
            .await
    }

    /// `null` from the venue means no sub-accounts.
    pub async fn sub_accounts(&self, user: Address) -> Result<Vec<SubAccount>, Error> {
        let subs: Option<Vec<SubAccount>> = self
            .post(serde_json::json!({ "type": "subAccounts", "user": user }))
            .await?;
        Ok(subs.unwrap_or_default())
    }
}

/// Classify a `fundingHistory` body. Split out from the request so the one
/// distinction that matters — `null` (unlisted coin) versus `[]` (no data) —
/// is testable without a network.
fn parse_funding_history(status: u16, text: &str) -> Result<FundingHistoryPage, Error> {
    match serde_json::from_str::<Option<Vec<FundingHistoryRow>>>(text) {
        Ok(None) => Ok(FundingHistoryPage::UnlistedCoin),
        Ok(Some(rows)) if (200..300).contains(&status) => Ok(FundingHistoryPage::Rows(rows)),
        _ => Err(Error::Venue {
            status,
            message: text.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FUNDING_HISTORY_PAGE_LIMIT;

    /// Bodies captured verbatim from mainnet on 2026-09-03.
    const UNLISTED_BODY: &str = "null";
    const NO_DATA_BODY: &str = "[]";
    const ONE_ROW_BODY: &str = r#"[{"coin":"BTC","fundingRate":"0.0000125","premium":"0.0001446433","time":1787626800017}]"#;

    /// `docs/specs/fair-value.md` §14.4 correction 12. `coin:"NOTACOIN"`
    /// answers HTTP 500 with `null`; `coin:"BTC"` with a future `startTime`
    /// answers HTTP 200 with `[]`. The two must never collapse.
    #[test]
    fn null_body_is_an_unlisted_coin_not_an_empty_page() {
        assert_eq!(
            parse_funding_history(500, UNLISTED_BODY).expect("null is not an error"),
            FundingHistoryPage::UnlistedCoin
        );
        let empty = parse_funding_history(200, NO_DATA_BODY).expect("empty page");
        assert_eq!(empty, FundingHistoryPage::Rows(Vec::new()));
        assert_ne!(empty, FundingHistoryPage::UnlistedCoin);
        // Neither advances a backfill cursor, but for different reasons.
        assert_eq!(empty.next_start_ms(), None);
        assert_eq!(FundingHistoryPage::UnlistedCoin.next_start_ms(), None);
    }

    #[test]
    fn a_short_page_ends_the_walk() {
        let page = parse_funding_history(200, ONE_ROW_BODY).expect("one row");
        assert_eq!(page.rows().len(), 1);
        assert_eq!(page.rows()[0].premium.to_string(), "0.0001446433");
        assert!(page.rows().len() < FUNDING_HISTORY_PAGE_LIMIT);
        assert_eq!(page.next_start_ms(), None);
    }

    /// Any other failure stays a typed venue error rather than being read as
    /// "no funding here".
    #[test]
    fn other_failures_are_venue_errors() {
        let err = parse_funding_history(429, "rate limited").expect_err("must not parse");
        assert!(matches!(err, Error::Venue { status: 429, .. }));
        let err = parse_funding_history(200, "{not json").expect_err("must not parse");
        assert!(matches!(err, Error::Venue { status: 200, .. }));
        // A 500 whose body is not `null` is a real failure, not an unlisted coin.
        let err = parse_funding_history(500, r#"{"error":"boom"}"#).expect_err("must not parse");
        assert!(matches!(err, Error::Venue { status: 500, .. }));
    }

    /// Proves §14.4 correction 2 against the live venue: 56 of 233 mainnet
    /// assets null `premium`, `midPx` and `impactPxs`, and the response must
    /// still deserialize whole. Read-only info endpoints need no key, which
    /// §14.2 says explicitly does not conflict with the testnet-default
    /// trading decision.
    #[tokio::test]
    #[ignore = "hits the public mainnet info endpoint"]
    async fn live_mainnet_null_ctx_fields() {
        let info = InfoClient::new(crate::Network::Mainnet).expect("client");
        let response = info
            .meta_and_asset_ctxs()
            .await
            .expect("a nulled asset must not fail the whole response");

        assert!(response.is_aligned());
        let total = response.ctxs().len();
        assert!(total > 200, "mainnet universe was {total}");

        let nulled = response
            .ctxs()
            .iter()
            .filter(|ctx| ctx.premium.is_none())
            .count();
        assert!(nulled > 0, "the audit found 56 of 233; found none");

        for (id, info, ctx) in response.iter() {
            // Co-nullity is not guaranteed (testnet SAGA), but a nulled
            // premium always means the §3.1 inputs are gone.
            if ctx.premium.is_none() {
                assert!(
                    ctx.impact_pxs.is_none(),
                    "{} (id {id}) has a premium-less book",
                    info.name
                );
            }
            // The invariant is open interest, not isDelisted.
            if !ctx.has_book() {
                assert!(
                    ctx.premium.is_none(),
                    "{} (id {id}) has zero open interest but a live premium",
                    info.name
                );
            }
        }

        // On mainnet the bookless set and the nulled set coincide (§14.4
        // correction 2, 233/233). They do not on testnet — SAGA has open
        // interest with a null premium — so a failure here is a real
        // divergence worth reading, not a flaky assertion.
        let tradable = response.tradable().count();
        assert_eq!(
            tradable + nulled,
            total,
            "the bookless set and the nulled set have diverged on mainnet"
        );
    }

    /// §14.4 correction 3, proven live: `allMids` answers for a nulled asset,
    /// and its answer is `markPx`, not a mid.
    #[tokio::test]
    #[ignore = "hits the public mainnet info endpoint"]
    async fn live_all_mids_is_a_stale_mark_not_a_mid() {
        let info = InfoClient::new(crate::Network::Mainnet).expect("client");
        let response = info.meta_and_asset_ctxs().await.expect("ctxs");
        let mids = info.all_mids().await.expect("allMids");

        let mut checked = 0usize;
        for (_, asset, ctx) in response.iter() {
            if ctx.mid_px_no_fallback().is_some() {
                continue;
            }
            let Some(mid) = mids.get(&asset.name) else {
                continue;
            };
            assert_eq!(
                *mid, ctx.mark_px,
                "{}: allMids is markPx, so backfilling midPx from it is a lie",
                asset.name
            );
            checked += 1;
        }
        assert!(checked > 0, "no nulled asset appeared in allMids");
    }

    /// §14.4 correction 12 and §14.3, proven live against mainnet.
    #[tokio::test]
    #[ignore = "hits the public mainnet info endpoint"]
    async fn live_funding_history_paginates_and_flags_unlisted_coins() {
        let info = InfoClient::new(crate::Network::Mainnet).expect("client");

        // A coin that does not exist: `null` body, HTTP 500.
        let unlisted = info
            .funding_history("NOTACOIN", 1_788_000_000_000, None)
            .await
            .expect("null is a classification, not a transport error");
        assert_eq!(unlisted, FundingHistoryPage::UnlistedCoin);
        assert_eq!(unlisted.next_start_ms(), None);

        // Genuine no-data: `[]`, HTTP 200.
        let future = info
            .funding_history("BTC", 1_999_000_000_000, None)
            .await
            .expect("empty page");
        assert_eq!(future, FundingHistoryPage::Rows(Vec::new()));

        // A window wider than the cap truncates from the newest end.
        let first = info
            .funding_history("BTC", 1_783_000_000_000, None)
            .await
            .expect("first page");
        let rows = first.rows();
        assert_eq!(rows.len(), FUNDING_HISTORY_PAGE_LIMIT);
        assert!(
            rows.windows(2).all(|w| w[0].time <= w[1].time),
            "oldest first"
        );
        assert!(rows.iter().all(|r| r.coin == "BTC"));

        // Forward pagination, with the inclusive-bound overlap.
        let cursor = first.next_start_ms().expect("a full page has a cursor");
        assert_eq!(cursor, rows[FUNDING_HISTORY_PAGE_LIMIT - 1].time);
        let second = info
            .funding_history("BTC", cursor, None)
            .await
            .expect("second page");
        let next = second.rows();
        assert!(!next.is_empty());
        assert_eq!(next[0].time, cursor, "startTime is inclusive: dedupe");
        assert!(next[next.len() - 1].time > cursor, "the walk advanced");

        // The uncensored premium is what the engine reads; the rate is mostly
        // the pinned mechanism constant.
        let pinned = rows
            .iter()
            .filter(|r| r.funding_rate.to_string() == "0.0000125")
            .count();
        assert!(
            pinned * 2 > rows.len(),
            "expected most prints pinned, got {pinned}/{}",
            rows.len()
        );
    }

    /// The `bbo` payload shape, proven against the live mainnet socket.
    #[tokio::test]
    #[ignore = "hits the public mainnet websocket"]
    async fn live_bbo_payload_shape() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (mut socket, _) = tokio_tungstenite::connect_async(crate::Network::Mainnet.ws_url())
            .await
            .expect("ws connect");
        socket
            .send(Message::Text(
                r#"{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}"#.into(),
            ))
            .await
            .expect("subscribe");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no bbo frame within 30s");
            let Ok(Some(Ok(message))) = tokio::time::timeout(remaining, socket.next()).await else {
                continue;
            };
            let Message::Text(text) = message else {
                continue;
            };
            let frame: serde_json::Value = serde_json::from_str(&text).expect("json frame");
            if frame["channel"] != "bbo" {
                continue;
            }
            let bbo: crate::types::Bbo =
                serde_json::from_value(frame["data"].clone()).expect("bbo payload shape");
            assert_eq!(bbo.coin, "BTC");
            assert!(bbo.time > 1_700_000_000_000, "bbo carries a timestamp");
            let bid = bbo.bid().expect("BTC has a bid");
            let ask = bbo.ask().expect("BTC has an ask");
            assert!(bid.px < ask.px);
            assert!(bid.sz > rust_decimal::Decimal::ZERO && bid.n > 0);
            return;
        }
    }
}
