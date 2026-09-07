//! `POST /info` client: read-only venue queries (`docs/spec.md` items 9–11).

use std::collections::HashMap;

use reqwest::Client;
use rust_decimal::Decimal;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::{
    Candle, ClearinghouseState, Fill, FundingHistoryPage, FundingHistoryRow, L2Book, Meta,
    MetaAndAssetCtxs, OpenOrder, OrderStatusResponse, Portfolio, PredictedFundings,
    SpotClearinghouseState, SubAccount, UserRateLimit, ValidatorDexCoin,
};
use crate::wire::Cloid;
use crate::{Address, Error, Network, Universe};

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
        let response = self
            .http
            .post(&self.url)
            .timeout(crate::REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await?;
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
    ///
    /// `universe` is the caller's current view of what is listed, and it is
    /// required rather than optional because it turns that classification
    /// into a checkable claim: a `null` for a coin the universe **knows**
    /// is a venue failure, not a fact about the coin, and comes back as
    /// [`Error::Venue`] for the caller to retry. Under D-d the background
    /// backfill walks backwards until the venue serves nothing older and
    /// under D-e the UI states the date from which history is provably
    /// complete, so one transient `null` swallowed here becomes a truncated
    /// history and a wrong date on screen with no error raised anywhere.
    /// Pass `&Universe::default()` to say, honestly, that there is nothing to
    /// cross-check against.
    ///
    /// `coin` is a [`ValidatorDexCoin`]: `docs/specs/fair-value.md` §14.4
    /// correction 15 fences v1 to the validator-operated dex, and a HIP-3
    /// coin is refused before the request rather than returning rows whose
    /// documented reconstruction property is false (see
    /// [`FundingHistoryRow`]).
    pub async fn funding_history(
        &self,
        coin: &ValidatorDexCoin,
        universe: &Universe,
        start_ms: u64,
        end_ms: Option<u64>,
    ) -> Result<FundingHistoryPage, Error> {
        let mut body = serde_json::json!({
            "type": "fundingHistory",
            "coin": coin.as_str(),
            "startTime": start_ms,
        });
        if let Some(end) = end_ms {
            body["endTime"] = serde_json::json!(end);
        }

        // Read the body before judging the status: the unlisted-coin answer
        // is a `null` payload delivered with an error status, and the
        // distinction it draws is worth more than the status code.
        let response = self
            .http
            .post(&self.url)
            .timeout(crate::REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        parse_funding_history(status, &text, universe.get(coin.as_str()).is_ok())
    }

    pub async fn predicted_fundings(&self) -> Result<Vec<PredictedFundings>, Error> {
        self.post(serde_json::json!({ "type": "predictedFundings" }))
            .await
    }

    pub async fn clearinghouse_state(&self, user: Address) -> Result<ClearinghouseState, Error> {
        self.post(serde_json::json!({ "type": "clearinghouseState", "user": user }))
            .await
    }

    /// Account value and PnL over the venue's own windows (`day`, `week`, …).
    ///
    /// The source for the drawdown high-water mark: nothing local can
    /// reconstruct a peak that predates oppen's first run.
    pub async fn portfolio(&self, user: Address) -> Result<Portfolio, Error> {
        self.post(serde_json::json!({ "type": "portfolio", "user": user }))
            .await
    }

    /// Spot balances. Required for equity under unified margin — see
    /// [`crate::types::MarginSummary::account_value`].
    pub async fn spot_clearinghouse_state(
        &self,
        user: Address,
    ) -> Result<SpotClearinghouseState, Error> {
        self.post(serde_json::json!({ "type": "spotClearinghouseState", "user": user }))
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
///
/// The venue offers a three-way split and this reproduces all three:
/// HTTP 200 with an array, the deterministic HTTP 500 with `null` that means
/// "not listed on this network", and everything else. A `null` on any other
/// status is a transport or upstream failure wearing the unlisted answer's
/// clothes — a 503 from a proxy, a 429 — and classifying it as unlisted
/// permanently retires a real coin's backfill on one bad second. So is a
/// `null` for a coin `coin_is_listed` says exists: the venue is contradicting
/// the universe it just served, which is a retryable anomaly and not a fact.
fn parse_funding_history(
    status: u16,
    text: &str,
    coin_is_listed: bool,
) -> Result<FundingHistoryPage, Error> {
    match serde_json::from_str::<Option<Vec<FundingHistoryRow>>>(text) {
        Ok(None) if status == 500 && !coin_is_listed => Ok(FundingHistoryPage::UnlistedCoin),
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

    #[tokio::test]
    async fn requests_cannot_hold_an_execution_lock_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            socket.writable().await.unwrap();
            let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n{";
            assert_eq!(socket.try_write(partial).unwrap(), partial.len());
            std::future::pending::<()>().await;
            drop(socket);
        });
        let mut info = InfoClient::with_client(Network::Testnet, Client::new());
        info.url = format!("http://{address}/info");
        let result = tokio::time::timeout(
            crate::REQUEST_TIMEOUT + std::time::Duration::from_secs(3),
            info.meta(),
        )
        .await;
        peer.abort();
        let _ = peer.await;
        let error = result
            .expect("the client deadline must release the request")
            .unwrap_err();
        assert!(
            matches!(error, Error::Http(ref error) if error.is_timeout()),
            "{error:?}"
        );
    }
    use crate::types::{FUNDING_HISTORY_PAGE_LIMIT, ScopeError};

    /// Bodies captured verbatim from mainnet on 2026-09-03.
    const UNLISTED_BODY: &str = "null";
    const NO_DATA_BODY: &str = "[]";
    const ONE_ROW_BODY: &str = r#"[{"coin":"BTC","fundingRate":"0.0000125","premium":"0.0001446433","time":1787626800017}]"#;

    /// Nothing in the local universe, so an unlisted classification stands.
    const UNKNOWN: bool = false;
    /// The coin is in the local universe, so the venue is contradicting it.
    const LISTED: bool = true;

    /// `docs/specs/fair-value.md` §14.4 correction 12. `coin:"NOTACOIN"`
    /// answers HTTP 500 with `null`; `coin:"BTC"` with a future `startTime`
    /// answers HTTP 200 with `[]`. The two must never collapse.
    #[test]
    fn null_body_is_an_unlisted_coin_not_an_empty_page() {
        assert_eq!(
            parse_funding_history(500, UNLISTED_BODY, UNKNOWN).expect("null is not an error"),
            FundingHistoryPage::UnlistedCoin
        );
        let empty = parse_funding_history(200, NO_DATA_BODY, LISTED).expect("empty page");
        assert_eq!(empty, FundingHistoryPage::Rows(Vec::new()));
        assert_ne!(empty, FundingHistoryPage::UnlistedCoin);
        // Neither advances a backfill cursor, but for different reasons.
        assert_eq!(empty.next_start_ms(), None);
        assert_eq!(FundingHistoryPage::UnlistedCoin.next_start_ms(), None);
    }

    /// A `null` is the unlisted answer **only** on the deterministic HTTP 500
    /// the venue serves for it. On any other status it is an upstream failure
    /// wearing that answer's clothes, and reading it as "this coin is not
    /// listed on this network" permanently truncates a real backfill on one
    /// bad second — D-d walks backwards until the venue serves nothing older,
    /// D-e then states a completeness date that is wrong.
    #[test]
    fn a_null_on_any_other_status_is_a_retryable_venue_error() {
        for status in [200u16, 429, 500, 502, 503] {
            let result = parse_funding_history(status, UNLISTED_BODY, UNKNOWN);
            if status == 500 {
                assert_eq!(
                    result.expect("500 + null is the unlisted answer"),
                    FundingHistoryPage::UnlistedCoin
                );
                continue;
            }
            let err = result.expect_err("a null on {status} must not classify a coin");
            assert!(
                matches!(err, Error::Venue { status: s, .. } if s == status),
                "status {status} produced {err}"
            );
        }
    }

    /// And a `null` for a coin the caller's own universe lists is the venue
    /// contradicting the meta it just served: retryable, not a verdict.
    #[test]
    fn a_null_for_a_listed_coin_is_not_an_unlisted_coin() {
        let err = parse_funding_history(500, UNLISTED_BODY, LISTED)
            .expect_err("BTC is listed; a null here is a failure");
        assert!(matches!(err, Error::Venue { status: 500, .. }));
    }

    #[test]
    fn a_short_page_ends_the_walk() {
        let page = parse_funding_history(200, ONE_ROW_BODY, LISTED).expect("one row");
        let rows = page.rows().expect("a listed coin has rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].premium.to_string(), "0.0001446433");
        assert!(rows.len() < FUNDING_HISTORY_PAGE_LIMIT);
        assert_eq!(page.next_start_ms(), None);
    }

    /// §14.4 correction 15, at the client boundary: a HIP-3 coin cannot be
    /// spelled as an argument to [`InfoClient::funding_history`], and a HIP-3
    /// body cannot be parsed into a page even if one arrived.
    #[test]
    fn a_hip3_coin_never_reaches_the_endpoint() {
        assert_eq!(
            ValidatorDexCoin::new("xyz:TSLA"),
            Err(ScopeError::OutOfScopeDex("xyz:TSLA".to_owned())),
        );
        assert!(ValidatorDexCoin::new("hyna:BTC").is_err());
        assert!(ValidatorDexCoin::new("BTC").is_ok());

        // The rows a HIP-3 request returns deserialize against every field
        // except the coin, which is the whole reason the fence is on the type.
        let hip3 = r#"[{"coin":"xyz:TSLA","fundingRate":"0.0000125","premium":"0.0001446433","time":1787626800017}]"#;
        let err = parse_funding_history(200, hip3, LISTED).expect_err("out of scope");
        assert!(matches!(err, Error::Venue { status: 200, .. }));
    }

    /// Any other failure stays a typed venue error rather than being read as
    /// "no funding here".
    #[test]
    fn other_failures_are_venue_errors() {
        let err = parse_funding_history(429, "rate limited", LISTED).expect_err("must not parse");
        assert!(matches!(err, Error::Venue { status: 429, .. }));
        let err = parse_funding_history(200, "{not json", LISTED).expect_err("must not parse");
        assert!(matches!(err, Error::Venue { status: 200, .. }));
        // A 500 whose body is not `null` is a real failure, not an unlisted coin.
        let err =
            parse_funding_history(500, r#"{"error":"boom"}"#, UNKNOWN).expect_err("must not parse");
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

        // `metaAndAssetCtxs` without a `dex` is the validator dex, so this is
        // the one place the array position is the on-chain asset id.
        for (id, info, ctx) in response.iter_on(crate::types::PerpDex::Validator) {
            // Co-nullity is not guaranteed (testnet SAGA), but a nulled
            // premium always means the §3.1 inputs are gone.
            if ctx.premium.is_none() {
                assert!(
                    !ctx.can_build_carry(),
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
        let with_book = response.with_book().count();
        assert_eq!(
            with_book + nulled,
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
        for (asset, ctx) in response.iter() {
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
        let universe =
            Universe::from_meta(&info.meta().await.expect("meta")).expect("validator dex");
        let btc = ValidatorDexCoin::new("BTC").expect("BTC");
        let nope = ValidatorDexCoin::new("NOTACOIN").expect("well-formed, just not listed");

        // A coin that does not exist: `null` body, HTTP 500, and the universe
        // agrees it is not listed.
        let unlisted = info
            .funding_history(&nope, &universe, 1_788_000_000_000, None)
            .await
            .expect("null is a classification, not a transport error");
        assert_eq!(unlisted, FundingHistoryPage::UnlistedCoin);
        assert_eq!(unlisted.next_start_ms(), None);
        assert_eq!(unlisted.rows(), None, "not an empty page");

        // Genuine no-data: `[]`, HTTP 200.
        let future = info
            .funding_history(&btc, &universe, 1_999_000_000_000, None)
            .await
            .expect("empty page");
        assert_eq!(future, FundingHistoryPage::Rows(Vec::new()));

        // A window wider than the cap truncates from the newest end.
        let first = info
            .funding_history(&btc, &universe, 1_783_000_000_000, None)
            .await
            .expect("first page");
        let rows = first.rows().expect("BTC is listed");
        assert_eq!(rows.len(), FUNDING_HISTORY_PAGE_LIMIT);
        assert!(
            rows.windows(2).all(|w| w[0].time <= w[1].time),
            "oldest first"
        );
        assert!(rows.iter().all(|r| r.coin == *"BTC"));

        // Forward pagination, with the inclusive-bound overlap.
        let cursor = first.next_start_ms().expect("a full page has a cursor");
        assert_eq!(cursor, rows[FUNDING_HISTORY_PAGE_LIMIT - 1].time);
        let second = info
            .funding_history(&btc, &universe, cursor, None)
            .await
            .expect("second page");
        let next = second.rows().expect("BTC is listed");
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

    /// §3.1/§3.2's `g`, the reconstruction [`FundingHistoryRow`] documents:
    /// `F₈ₕ = P̄ + clamp(i − P̄, −5e-4, +5e-4)` with `i = 1e-4`, charged
    /// hourly at `F₈ₕ / 8`.
    fn g_validator_dex(premium: Decimal) -> Decimal {
        use std::str::FromStr;
        let i = Decimal::from_str("0.0001").expect("literal");
        let clamp = Decimal::from_str("0.0005").expect("literal");
        (premium + (i - premium).clamp(-clamp, clamp)) / Decimal::from(8)
    }

    /// §14.4 correction 15, measured rather than asserted. The reconstruction
    /// holds on the validator dex and fails on HIP-3, which is why the scope
    /// fence is a type and not a doc comment.
    ///
    /// Measured 2026-09-04 from `startTime` 1787000000000: BTC 0/200
    /// mismatches at 5e-11; `xyz:TSLA` **200/200**, worst |err| 9.35e-5
    /// (9.3 bp per hour); `hyna:BTC` 53/200, worst |err| 2.50e-5.
    #[tokio::test]
    #[ignore = "hits the public mainnet info endpoint"]
    async fn live_hip3_funding_does_not_satisfy_the_validator_dex_formula() {
        use std::str::FromStr;

        let info = InfoClient::new(crate::Network::Mainnet).expect("client");
        let universe =
            Universe::from_meta(&info.meta().await.expect("meta")).expect("validator dex");
        let tolerance = Decimal::from_str("0.00000000005").expect("5e-11");

        // The validator dex: `g(premium)` reproduces `fundingRate`.
        let btc = ValidatorDexCoin::new("BTC").expect("BTC");
        let page = info
            .funding_history(&btc, &universe, 1_787_000_000_000, None)
            .await
            .expect("BTC page");
        let rows = page.rows().expect("BTC is listed");
        assert!(!rows.is_empty());
        for row in rows {
            let err = (g_validator_dex(row.premium) - row.funding_rate).abs();
            assert!(err <= tolerance, "BTC row at {} missed by {err}", row.time);
        }

        // HIP-3: the same request shape, a body that parses as JSON, and a
        // reconstruction that misses on every single row.
        let body = serde_json::json!({
            "type": "fundingHistory",
            "coin": "xyz:TSLA",
            "startTime": 1_787_000_000_000u64,
        });
        let text = reqwest::Client::new()
            .post(format!("{}/info", crate::Network::Mainnet.api_url()))
            .json(&body)
            .send()
            .await
            .expect("request")
            .text()
            .await
            .expect("body");

        // It is a well-formed funding-history payload in every respect
        // except the one this crate refuses to represent.
        let raw: Vec<serde_json::Value> = serde_json::from_str(&text).expect("an array of rows");
        assert!(!raw.is_empty(), "xyz:TSLA served no rows");
        assert!(
            parse_funding_history(200, &text, false).is_err(),
            "a HIP-3 body must not become a FundingHistoryPage"
        );

        let mut mismatches = 0usize;
        let mut worst = Decimal::ZERO;
        for row in &raw {
            let premium =
                Decimal::from_str(row["premium"].as_str().expect("premium string")).expect("parse");
            let rate = Decimal::from_str(row["fundingRate"].as_str().expect("rate string"))
                .expect("parse");
            let err = (g_validator_dex(premium) - rate).abs();
            if err > tolerance {
                mismatches += 1;
            }
            worst = worst.max(err);
        }
        assert_eq!(
            mismatches,
            raw.len(),
            "the validator-dex formula reproduced {} of {} HIP-3 rows",
            raw.len() - mismatches,
            raw.len()
        );
        assert!(
            worst > Decimal::from_str("0.00001").expect("1e-5"),
            "worst HIP-3 error was only {worst}"
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
