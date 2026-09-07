#![cfg(test)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use oppen_hl::Action;
use oppen_hl::wire::{OrderType, OrderWire, Tif};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, Default)]
pub(super) enum Behavior {
    #[default]
    Resting,
    Rejected,
    AppliedMalformed,
}

pub(super) struct Venue {
    port: u16,
    state: Arc<Mutex<Book>>,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Venue {
    pub(super) async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let state = Arc::new(Mutex::new(Book::default()));
        let router = Router::new()
            .route("/info", post(info))
            .route("/exchange", post(exchange))
            .fallback(unsupported_route)
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .expect("fixture server");
        });
        Self {
            port,
            state,
            stop,
            task: Some(task),
        }
    }

    pub(super) fn port(&self) -> u16 {
        self.port
    }

    pub(super) fn submissions(&self) -> Vec<Value> {
        self.state.lock().expect("fixture lock").submissions.clone()
    }

    pub(super) fn info_count(&self) -> usize {
        self.state.lock().expect("fixture lock").info_count
    }

    pub(super) fn hold_info(&self, kind: &str) -> Arc<InfoGate> {
        let gate = Arc::new(InfoGate::default());
        self.state.lock().unwrap().info_gate = Some((kind.to_owned(), gate.clone()));
        gate
    }

    pub(super) fn next_response(&self, behavior: Behavior) {
        self.state
            .lock()
            .expect("fixture lock")
            .behaviors
            .push_back(behavior);
    }

    /// `size` is the incremental fill quantity, not cumulative filled size.
    pub(super) fn fill(&self, cloid: &str, size: Decimal) {
        self.fill_with_fee(cloid, size, Decimal::new(1, 2));
    }

    pub(super) fn fill_with_fee(&self, cloid: &str, size: Decimal, fee: Decimal) {
        let mut book = self.state.lock().expect("fixture lock");
        let oid = book
            .orders
            .values()
            .find(|order| order.wire.c.as_ref().is_some_and(|c| c.as_str() == cloid))
            .map(|order| order.oid)
            .expect("fill names a known cloid");
        book.fill_with_fee(oid, size, fee)
            .expect("valid fixture fill");
    }

    pub(super) async fn shutdown(mut self) {
        self.stop.cancel();
        let mut task = self.task.take().expect("fixture task");
        match tokio::time::timeout(std::time::Duration::from_secs(2), &mut task).await {
            Ok(result) => result.expect("fixture task panicked"),
            Err(_) => {
                task.abort();
                let _ = task.await;
                panic!("fixture shutdown timed out");
            }
        }
        let book = self.state.lock().expect("fixture lock");
        assert!(
            book.failures.is_empty(),
            "fixture failures: {:?}",
            book.failures
        );
        assert!(
            book.behaviors.is_empty(),
            "unused fixture response behaviors"
        );
    }
}

impl Drop for Venue {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Default)]
struct Book {
    info_count: usize,
    info_gate: Option<(String, Arc<InfoGate>)>,
    orders: BTreeMap<u64, Order>,
    position: Decimal,
    fees_paid: Decimal,
    fills: Vec<Value>,
    submissions: Vec<Value>,
    behaviors: VecDeque<Behavior>,
    failures: Vec<String>,
    account: Option<String>,
}

#[derive(Default)]
pub(super) struct InfoGate {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

struct Order {
    wire: OrderWire,
    oid: u64,
    original: Decimal,
    remaining: Decimal,
    created: u64,
    updated: u64,
    status: &'static str,
}

impl Order {
    fn view(&self) -> Value {
        json!({
            "coin": "TEST", "side": if self.wire.b { "B" } else { "A" },
            "limitPx": self.wire.p.as_str(), "sz": self.remaining.to_string(),
            "origSz": self.original.to_string(), "oid": self.oid,
            "timestamp": self.created, "cloid": self.wire.c,
            "orderType": "Limit", "reduceOnly": self.wire.r,
            "isTrigger": false, "triggerPx": null, "triggerCondition": null,
            "isPositionTpsl": false
        })
    }

    fn status(&self) -> Value {
        json!({ "status": "order", "order": {
            "order": self.view(), "status": self.status, "statusTimestamp": self.updated
        } })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

fn meta() -> Value {
    json!({ "universe": [{ "name": "TEST", "szDecimals": 2, "maxLeverage": 10,
        "marginTableId": 0, "isDelisted": false, "onlyIsolated": false }] })
}

fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {field}: {value}"))
}

impl Book {
    fn equity(&self) -> Decimal {
        Decimal::from(100) - self.fees_paid
    }

    fn info(&mut self, request: &Value) -> Result<Value, String> {
        self.info_count += 1;
        let kind = required(request, "type")?;
        if !matches!(kind, "meta" | "metaAndAssetCtxs") {
            let user = required(request, "user")?.to_ascii_lowercase();
            user.parse::<oppen_hl::Address>()
                .map_err(|e| e.to_string())?;
            match &self.account {
                Some(account) if account != &user => {
                    return Err("fixture supports one account".into());
                }
                None => self.account = Some(user),
                _ => {}
            }
        }
        let now = now_ms();
        match kind {
            "meta" => Ok(meta()),
            "metaAndAssetCtxs" => Ok(json!([meta(), [{
                "funding": "0", "openInterest": "1", "prevDayPx": "100",
                "dayNtlVlm": "1000", "premium": "0", "oraclePx": "100",
                "markPx": "100", "midPx": "100", "impactPxs": ["100", "100"]
            }]])),
            "clearinghouseState" => {
                let notional = self.position.abs() * Decimal::from(100);
                let margin = notional / Decimal::from(10);
                let summary = json!({ "accountValue": self.equity().to_string(),
                    "totalNtlPos": notional.to_string(), "totalRawUsd": self.equity().to_string(),
                    "totalMarginUsed": margin.to_string() });
                let positions = if self.position.is_zero() {
                    vec![]
                } else {
                    vec![json!({
                        "type": "oneWay", "position": {
                            "coin": "TEST", "szi": self.position.to_string(), "entryPx": "100",
                            "positionValue": notional.to_string(), "unrealizedPnl": "0",
                            "returnOnEquity": "0", "liquidationPx": null,
                            "marginUsed": margin.to_string(), "maxLeverage": 10,
                            "leverage": {"type": "cross", "value": 10, "rawUsd": null},
                            "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                        }
                    })]
                };
                Ok(
                    json!({ "marginSummary": summary, "crossMarginSummary": summary,
                    "crossMaintenanceMarginUsed": "0", "assetPositions": positions,
                    "withdrawable": (self.equity() - margin).max(Decimal::ZERO).to_string(), "time": now }),
                )
            }
            "spotClearinghouseState" => Ok(json!({ "balances": [] })),
            "frontendOpenOrders" => Ok(Value::Array(
                self.orders
                    .values()
                    .filter(|order| order.status == "open")
                    .map(Order::view)
                    .collect(),
            )),
            "userFillsByTime" => {
                let start = request["startTime"].as_u64().ok_or("missing startTime")?;
                let end = match request.get("endTime") {
                    Some(value) => value.as_u64().ok_or("invalid endTime")?,
                    None => u64::MAX,
                };
                Ok(Value::Array(
                    self.fills
                        .iter()
                        .filter(|fill| {
                            let time = fill["time"].as_u64().expect("fixture fill time");
                            time >= start && time <= end
                        })
                        .take(2000)
                        .cloned()
                        .collect(),
                ))
            }
            "portfolio" => Ok(json!([["day", {
                "accountValueHistory": [[now.saturating_sub(86_400_000), "100"], [now, self.equity().to_string()]],
                "pnlHistory": [[now, (self.equity() - Decimal::from(100)).to_string()]], "vlm": "0"
            }]])),
            "orderStatus" => {
                let query = request.get("oid").ok_or("missing oid")?;
                let order = if let Some(oid) = query.as_u64() {
                    self.orders.get(&oid)
                } else if let Some(cloid) = query.as_str() {
                    oppen_hl::wire::Cloid::parse(cloid).map_err(|e| e.to_string())?;
                    self.orders
                        .values()
                        .find(|order| order.wire.c.as_ref().is_some_and(|c| c.as_str() == cloid))
                } else {
                    return Err("invalid orderStatus oid".into());
                };
                Ok(order
                    .map(Order::status)
                    .unwrap_or_else(|| json!({"status": "unknownOid"})))
            }
            _ => Err(format!("unsupported info request: {request}")),
        }
    }

    fn fill(&mut self, oid: u64, requested: Decimal) -> Result<Decimal, String> {
        self.fill_with_fee(oid, requested, Decimal::new(1, 2))
    }

    fn fill_with_fee(
        &mut self,
        oid: u64,
        requested: Decimal,
        fee: Decimal,
    ) -> Result<Decimal, String> {
        let order = self.orders.get_mut(&oid).ok_or("unknown fill oid")?;
        if order.status != "open" || requested <= Decimal::ZERO || requested > order.remaining {
            return Err("fill must be positive and within an open order's remaining size".into());
        }
        let size = if order.wire.r {
            if self.position.is_zero() || self.position.is_sign_positive() == order.wire.b {
                return Err("reduce-only fill would increase position".into());
            }
            requested.min(self.position.abs())
        } else {
            requested
        };
        let start = self.position;
        self.position += if order.wire.b { size } else { -size };
        order.remaining -= size;
        order.updated = now_ms();
        if order.remaining.is_zero() {
            order.status = "filled";
        } else if order.wire.r && self.position.is_zero() {
            order.status = "canceled";
        }
        let tid = self.fills.len() as u64 + 1;
        self.fees_paid += fee;
        self.fills.push(json!({
            "coin": "TEST", "px": "100", "sz": size.to_string(),
            "side": if order.wire.b {"B"} else {"A"}, "time": order.updated,
            "startPosition": start.to_string(), "dir": match (order.wire.b, start < Decimal::ZERO, start > Decimal::ZERO) {
                (true, true, _) => "Close Short", (false, _, true) => "Close Long",
                (true, _, _) => "Open Long", _ => "Open Short"
            },
            "closedPnl": "0", "hash": format!("0x{tid:064x}"), "oid": oid,
            "crossed": true, "fee": fee.to_string(), "feeToken": "USDC", "builderFee": "0",
            "tid": tid, "cloid": order.wire.c
        }));
        Ok(size)
    }

    fn cancel(&mut self, oid: u64) -> Value {
        match self.orders.get_mut(&oid) {
            Some(order) if order.status == "open" => {
                order.status = "canceled";
                order.updated = now_ms();
                json!("success")
            }
            _ => json!({"error": "Order was never placed, already canceled, or filled."}),
        }
    }

    fn exchange(&mut self, request: Value) -> Result<(Value, bool), String> {
        self.submissions.push(request.clone());
        if request["nonce"].as_u64().is_none() || !request["signature"].is_object() {
            return Err("expected signed exchange envelope".into());
        }
        let action: Action =
            serde_json::from_value(request["action"].clone()).map_err(|e| e.to_string())?;
        if !matches!(
            action,
            Action::Order { .. } | Action::Cancel { .. } | Action::CancelByCloid { .. }
        ) {
            return Err(format!("unsupported fixture action: {action:?}"));
        }
        let behavior = self.behaviors.pop_front().unwrap_or_default();
        if matches!(behavior, Behavior::Rejected) {
            return Ok((
                json!({"status": "err", "response": "fixture rejected request"}),
                false,
            ));
        }
        let (kind, statuses) = match action {
            Action::Order { orders, .. } => {
                let mut statuses = Vec::new();
                for wire in orders {
                    if wire.a != 0 {
                        return Err("fixture asset must be TEST (0)".into());
                    }
                    let OrderType::Limit { tif } = wire.t else {
                        return Err("fixture supports limit/IOC orders only".into());
                    };
                    let size: Decimal =
                        wire.s.as_str().parse().map_err(|e| format!("size: {e}"))?;
                    let px: Decimal = wire.p.as_str().parse().map_err(|e| format!("price: {e}"))?;
                    if size <= Decimal::ZERO || px <= Decimal::ZERO {
                        return Err("nonpositive fixture order".into());
                    }
                    if wire.c.as_ref().is_some_and(|cloid| {
                        self.orders
                            .values()
                            .any(|o| o.wire.c.as_ref() == Some(cloid))
                    }) {
                        statuses.push(json!({"error": "Duplicate cloid"}));
                        continue;
                    }
                    let oid = self.orders.len() as u64 + 1;
                    let now = now_ms();
                    self.orders.insert(
                        oid,
                        Order {
                            wire,
                            oid,
                            original: size,
                            remaining: size,
                            created: now,
                            updated: now,
                            status: "open",
                        },
                    );
                    if tif == Tif::Ioc {
                        let buy = self.orders[&oid].wire.b;
                        if (buy && px < Decimal::from(100)) || (!buy && px > Decimal::from(100)) {
                            self.cancel(oid);
                            statuses.push(json!({"error": "IOC could not immediately match"}));
                        } else {
                            match self.fill(oid, size) {
                                Ok(filled) => statuses.push(json!({"filled": {"oid": oid, "totalSz": filled.to_string(), "avgPx": "100"}})),
                                Err(message) => {
                                    self.orders.get_mut(&oid).expect("inserted order").status = "rejected";
                                    statuses.push(json!({"error": message}));
                                }
                            }
                        }
                    } else {
                        statuses.push(json!({"resting": {"oid": oid}}));
                    }
                }
                ("order", statuses)
            }
            Action::Cancel { cancels } => {
                let mut statuses = Vec::new();
                for cancel in cancels {
                    if cancel.a != 0 {
                        return Err("fixture cancel asset must be 0".into());
                    }
                    statuses.push(self.cancel(cancel.o));
                }
                ("cancel", statuses)
            }
            Action::CancelByCloid { cancels } => {
                let mut statuses = Vec::new();
                for cancel in cancels {
                    if cancel.asset != 0 {
                        return Err("fixture cancel asset must be 0".into());
                    }
                    let oid = self
                        .orders
                        .values()
                        .find(|o| o.wire.c.as_ref() == Some(&cancel.cloid))
                        .map(|o| o.oid);
                    statuses.push(self.cancel(oid.unwrap_or(0)));
                }
                ("cancel", statuses)
            }
            _ => return Err("unsupported fixture action".into()),
        };
        Ok((
            json!({"status": "ok", "response": {"type": kind, "data": {"statuses": statuses}}}),
            matches!(behavior, Behavior::AppliedMalformed),
        ))
    }
}

fn failure(book: &mut Book, message: String) -> Response {
    book.failures.push(message.clone());
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("venue fixture failure: {message}"),
    )
        .into_response()
}

async fn info(State(state): State<Arc<Mutex<Book>>>, Json(request): Json<Value>) -> Response {
    let (response, gate) = {
        let mut book = state.lock().expect("fixture lock");
        let response = match book.info(&request) {
            Ok(value) => Json(value).into_response(),
            Err(message) => failure(&mut book, message),
        };
        let gate = if book
            .info_gate
            .as_ref()
            .is_some_and(|(kind, _)| request["type"] == *kind)
        {
            book.info_gate.take().map(|(_, gate)| gate)
        } else {
            None
        };
        (response, gate)
    };
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.release.notified().await;
    }
    response
}

async fn exchange(State(state): State<Arc<Mutex<Book>>>, Json(request): Json<Value>) -> Response {
    let mut book = state.lock().expect("fixture lock");
    match book.exchange(request) {
        Ok((_, true)) => (StatusCode::OK, "{malformed").into_response(),
        Ok((value, false)) => Json(value).into_response(),
        Err(message) => failure(&mut book, message),
    }
}

async fn unsupported_route(State(state): State<Arc<Mutex<Book>>>) -> Response {
    failure(
        &mut state.lock().expect("fixture lock"),
        "unsupported fixture route".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::{Fill, OrderStatusResponse};

    const USER: &str = "0x1111111111111111111111111111111111111111";
    const CLOID: &str = "0x00000000000000000000000000000001";

    // Only envelope parsing is tested here; the parent's lifecycle test signs
    // with its test wallet and asserts the real submitted envelope.
    fn envelope(action: Value) -> Value {
        json!({"action": action, "nonce": 1, "signature": {"r": "0x1", "s": "0x1", "v": 27},
            "vaultAddress": null, "expiresAfter": null})
    }

    fn order(cloid: &str, buy: bool, size: &str, reduce: bool, tif: &str) -> Value {
        envelope(json!({"type": "order", "grouping": "na", "orders": [{
            "a": 0, "b": buy, "p": "100", "s": size, "r": reduce,
            "t": {"limit": {"tif": tif}}, "c": cloid
        }]}))
    }

    #[tokio::test]
    async fn info_shapes_parse_through_the_real_loopback_client() {
        let venue = Venue::start().await;
        let client = oppen_hl::InfoClient::loopback_fixture(venue.port()).expect("client");
        let user = USER.parse().expect("user");
        assert_eq!(client.meta().await.expect("meta").universe[0].name, "TEST");
        assert_eq!(
            client
                .meta_and_asset_ctxs()
                .await
                .expect("contexts")
                .reference_pxs()
                .get("TEST"),
            Some(Decimal::from(100))
        );
        assert_eq!(
            client
                .clearinghouse_state(user)
                .await
                .expect("perps")
                .margin_summary
                .account_value,
            Decimal::from(100)
        );
        assert!(
            client
                .spot_clearinghouse_state(user)
                .await
                .expect("spot")
                .balances
                .is_empty()
        );
        assert!(
            client
                .frontend_open_orders(user)
                .await
                .expect("orders")
                .is_empty()
        );
        assert!(
            client
                .user_fills_by_time(user, 0, None)
                .await
                .expect("fills")
                .is_empty()
        );
        assert_eq!(
            client
                .portfolio(user)
                .await
                .expect("portfolio")
                .window("day")
                .expect("day")
                .peak_account_value(),
            Some(Decimal::from(100))
        );
        assert_eq!(
            client
                .order_status(user, oppen_hl::info::OrderRef::Oid(999))
                .await
                .expect("status"),
            OrderStatusResponse::UnknownOid
        );
        venue.shutdown().await;
    }

    #[test]
    fn partial_fills_and_ioc_close_preserve_history_and_terminal_status() {
        let mut book = Book::default();
        let (reply, malformed) = book
            .exchange(order(CLOID, true, "1", false, "Gtc"))
            .expect("order");
        assert!(!malformed);
        assert!(matches!(
            oppen_hl::ExchangeResponse::parse(&reply.to_string())
                .expect("reply")
                .statuses
                .as_slice(),
            [oppen_hl::Status::Resting { oid: 1 }]
        ));
        book.fill(1, Decimal::new(4, 1)).expect("partial");
        assert_eq!(book.position, Decimal::new(4, 1));
        assert_eq!(book.orders[&1].remaining, Decimal::new(6, 1));
        book.fill(1, Decimal::new(6, 1)).expect("full");
        assert_eq!(book.orders[&1].status, "filled");
        let close = "0x00000000000000000000000000000002";
        let (reply, _) = book
            .exchange(order(close, false, "1", true, "Ioc"))
            .expect("close");
        assert!(
            matches!(oppen_hl::ExchangeResponse::parse(&reply.to_string()).expect("reply")
            .statuses.as_slice(), [oppen_hl::Status::Filled { total_sz, .. }] if *total_sz == Decimal::ONE)
        );
        assert_eq!(book.position, Decimal::ZERO);
        for query in [json!(CLOID), json!(1)] {
            let status = book
                .info(&json!({"type": "orderStatus", "user": USER, "oid": query}))
                .expect("status");
            let parsed: OrderStatusResponse = serde_json::from_value(status).expect("status shape");
            assert!(
                matches!(parsed, OrderStatusResponse::Order { order } if order.status == "filled")
            );
        }
        let fills: Vec<Fill> = serde_json::from_value(
            book.info(&json!({
                "type": "userFillsByTime", "user": USER, "startTime": 0
            }))
            .expect("history"),
        )
        .expect("fill shape");
        assert_eq!(
            fills.iter().map(|fill| fill.tid).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(fills[0].cloid.as_ref().expect("cloid").as_str(), CLOID);
        assert_eq!(book.equity(), Decimal::new(9997, 2));
        let before = fills.iter().map(|fill| fill.time).min().expect("fills") - 1;
        assert_eq!(
            book.info(&json!({"type": "userFillsByTime", "user": USER,
            "startTime": 0, "endTime": before}))
                .expect("window"),
            json!([])
        );
    }

    #[test]
    fn rejection_malformed_application_and_both_cancel_forms_are_distinct() {
        let mut book = Book::default();
        book.behaviors.push_back(Behavior::Rejected);
        let (reply, _) = book
            .exchange(order(CLOID, true, "1", false, "Gtc"))
            .expect("reject");
        assert!(matches!(
            oppen_hl::ExchangeResponse::parse(&reply.to_string()),
            Err(oppen_hl::Error::ExchangeRejected { .. })
        ));
        assert!(book.orders.is_empty());
        book.behaviors.push_back(Behavior::AppliedMalformed);
        let (_, malformed) = book
            .exchange(order(CLOID, true, "1", false, "Gtc"))
            .expect("apply");
        assert!(malformed);
        assert_eq!(book.orders[&1].status, "open");
        for (cloid, action) in [
            (
                CLOID,
                json!({"type": "cancelByCloid", "cancels": [{"asset": 0, "cloid": CLOID}]}),
            ),
            (
                "0x00000000000000000000000000000002",
                json!({"type": "cancel", "cancels": [{"a": 0, "o": 2}]}),
            ),
        ] {
            if cloid != CLOID {
                book.exchange(order(cloid, false, "1", false, "Gtc"))
                    .expect("second order");
            }
            let (reply, _) = book.exchange(envelope(action)).expect("cancel");
            assert!(matches!(
                oppen_hl::ExchangeResponse::parse(&reply.to_string())
                    .expect("cancel shape")
                    .statuses
                    .as_slice(),
                [oppen_hl::Status::Success]
            ));
            let status = book
                .info(&json!({"type": "orderStatus", "user": USER, "oid": cloid}))
                .expect("status");
            assert_eq!(status["order"]["status"], "canceled");
        }
        assert!(
            book.info(&json!({"type": "unexpected", "user": USER}))
                .is_err()
        );
        assert!(
            book.exchange(envelope(json!({"type": "scheduleCancel"})))
                .is_err()
        );
    }
}
