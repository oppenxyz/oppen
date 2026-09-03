//! `POST /info` client: read-only venue queries (`docs/spec.md` items 9–11).

use std::collections::HashMap;

use reqwest::Client;
use rust_decimal::Decimal;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::{
    Candle, ClearinghouseState, Fill, L2Book, Meta, MetaAndAssetCtxs, OpenOrder,
    OrderStatusResponse, PredictedFundings, SubAccount, UserRateLimit,
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

    pub async fn all_mids(&self) -> Result<HashMap<String, Decimal>, Error> {
        self.post(serde_json::json!({ "type": "allMids" })).await
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
