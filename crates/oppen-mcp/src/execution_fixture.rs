//! Actual MCP dispatch, guarded signing, HTTP exchange and durable recovery.

use super::*;
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use oppen_core::feed::pump::{FeedPump, FeedSubscriber};
use oppen_core::guardrail::{
    AgentGuardrails, AgentId, KillScope, LegacyPolicyReview, PersistedState,
};
use oppen_core::keys::{EntryName, KeyStore, KeyStoreError, SecretText};
use oppen_core::ledger::{EventKind, Ledger, PolicyJournal};
use oppen_core::reconcile::ReconcileSource;
use oppen_hl::types::{Fill, OpenOrder};
use oppen_hl::ws::{PoolError, Subscription};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{RwLock, Weak};
use std::time::Duration;
use tower::ServiceExt;

#[path = "execution_fixture/approvals.rs"]
mod approvals;
#[path = "execution_fixture/decision.rs"]
mod decision;
#[path = "execution_fixture/pilot.rs"]
mod pilot;
#[path = "execution_fixture/policy.rs"]
mod policy;
#[path = "execution_fixture/registry.rs"]
mod registry;
#[path = "execution_fixture/http.rs"]
mod transport;
#[path = "execution_fixture/venue.rs"]
mod venue;
use venue::{Behavior, Venue};

#[derive(Default)]
struct FixtureKeys {
    values: Mutex<HashMap<EntryName, String>>,
    ledger: Mutex<Weak<Ledger>>,
    read_heads: Mutex<Vec<EventKind>>,
    read_gate: Mutex<Option<KeyReadGate>>,
}

#[derive(Debug)]
struct KeyReadGate {
    entered: Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::Receiver<()>,
}

impl KeyStore for FixtureKeys {
    fn network(&self) -> Network {
        Network::Testnet
    }
    fn write(&self, entry: &EntryName, secret: &str) -> Result<(), KeyStoreError> {
        self.values
            .lock()
            .unwrap()
            .insert(entry.clone(), secret.into());
        Ok(())
    }
    fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
        let gate = self.read_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            let _ = gate.release.recv_timeout(Duration::from_secs(15));
        }
        if let Some(ledger) = self.ledger.lock().unwrap().upgrade() {
            let head = ledger.chain_head().unwrap().seq;
            if head > 0 {
                let events = ledger.get_events(head - 1, 1).unwrap();
                self.read_heads.lock().unwrap().push(events.events[0].kind);
            }
        }
        Ok(self
            .values
            .lock()
            .unwrap()
            .get(entry)
            .cloned()
            .map(SecretText::new))
    }
    fn remove(&self, entry: &EntryName) -> Result<(), KeyStoreError> {
        self.values.lock().unwrap().remove(entry);
        Ok(())
    }
}

struct HttpSource(InfoClient);

impl ReconcileSource for HttpSource {
    fn network(&self) -> Network {
        Network::Testnet
    }
    async fn user_fills_by_time(
        &self,
        user: Address,
        start: u64,
        end: Option<u64>,
    ) -> Result<Vec<Fill>, oppen_hl::Error> {
        self.0.user_fills_by_time(user, start, end).await
    }
    async fn frontend_open_orders(&self, user: Address) -> Result<Vec<OpenOrder>, oppen_hl::Error> {
        self.0.frontend_open_orders(user).await
    }
    async fn order_status(
        &self,
        user: Address,
        order: OrderRef,
    ) -> Result<OrderStatusResponse, oppen_hl::Error> {
        self.0.order_status(user, order).await
    }
}

struct NoSocket;
impl FeedSubscriber for NoSocket {
    fn subscribe(&self, _: Subscription) -> Result<(), PoolError> {
        panic!("fixture must not subscribe a live socket")
    }
    fn unsubscribe(&self, _: &Subscription) -> Result<(), PoolError> {
        panic!("fixture has no live socket")
    }
}

struct Runtime {
    gateway: Gateway,
    ledger: Arc<Ledger>,
    app: Router,
    pairings: crate::server::Pairings,
    registry: Arc<oppen_core::ledger::RegistryJournal>,
    token: String,
    session: String,
    account: Address,
    port: u16,
}

impl Runtime {
    fn tracker(&self) -> ExecutionTracker {
        ExecutionTracker::supervision(&tokio::sync::watch::channel(()).0, self.pairings.clone())
    }

    async fn open(path: &Path, port: u16, keys: Arc<FixtureKeys>) -> Self {
        Self::open_with_anchor(path, port, keys, None).await
    }

    async fn open_with_anchor(
        path: &Path,
        port: u16,
        keys: Arc<FixtureKeys>,
        anchor: Option<Box<dyn oppen_core::ledger::HeadAnchor>>,
    ) -> Self {
        Self::open_policy_fixture(path, port, keys, anchor, true).await
    }

    async fn open_policy_fixture(
        path: &Path,
        port: u16,
        keys: Arc<FixtureKeys>,
        anchor: Option<Box<dyn oppen_core::ledger::HeadAnchor>>,
        initialize_policy: bool,
    ) -> Self {
        let account = Address::from_bytes([9; 20]);
        let agent = AgentId::new("fixture-agent");
        // This constant is a disposable test vector, never a machine credential.
        if keys.agent_wallet(&agent).unwrap().is_none() {
            keys.create_agent_key(
                &agent,
                SecretText::new(format!("{:064x}", 1)),
                now_ms() + 86_400_000,
                now_ms(),
            )
            .unwrap();
        }
        let first_open = !path.join("ledger.db").exists();
        let ledger = Arc::new(
            match anchor {
                Some(anchor) => {
                    Ledger::open_anchored(&path.join("ledger.db"), Network::Testnet, Some(anchor))
                }
                None => Ledger::open_at(&path.join("ledger.db"), Network::Testnet),
            }
            .unwrap(),
        );
        let hmac = Arc::new(oppen_core::keys::HmacKey::from_bytes([77; 32]));
        let registry = Arc::new(
            oppen_core::ledger::RegistryJournal::open(ledger.clone(), hmac.clone()).unwrap(),
        );
        if first_open {
            registry
                .grant(
                    oppen_core::ledger::RegistryBinding {
                        agent: agent.clone(),
                        container: account,
                        vault_address: None,
                        wallet: keys.agent_wallet(&agent).unwrap().unwrap(),
                    },
                    now_ms(),
                )
                .unwrap();
        }
        let route = registry
            .route_for_agent(&agent)
            .expect("restart must replay an existing active route");
        assert_eq!(route.binding.container, account);
        let policy_journal = Arc::new(PolicyJournal::new(registry.clone()));
        if first_open && initialize_policy {
            let at = now_ms();
            let review =
                LegacyPolicyReview::open(path.join("policy.db"), Network::Testnet, at).unwrap();
            let mut replacement = PersistedState::paused(at);
            let mut policy = AgentGuardrails::default();
            policy.symbols.insert("TEST".into());
            policy.approval_required = false;
            policy.max_order_usd = Decimal::from(15);
            policy.max_position_usd = Decimal::from(100);
            policy.risk.max_open_exposure_usd = Some(Decimal::from(25));
            policy.risk.max_leverage = 1;
            policy.order_rate.count = 100;
            replacement.guardrails.insert(agent.clone(), policy);
            policy_journal.initialize(&review, replacement, at).unwrap();
        }
        let engine = Arc::new(GuardrailEngine::new(policy_journal, keys.clone()).unwrap());
        let mut gateway = Gateway::new(
            Network::Testnet,
            engine,
            EventViews::new(ledger.clone()),
            Arc::new(Journal::open(path.join("notes.db")).unwrap()),
            Arc::new(FeedSession::new()),
            Arc::new(AlertStore::open(":memory:").unwrap()),
            Arc::new(QuoteCache::new()),
        )
        .unwrap();
        let inner = Arc::get_mut(&mut gateway.inner).unwrap();
        inner.info = InfoClient::loopback_fixture(port).unwrap();
        inner.exchange = ExchangeClient::loopback_fixture(port).unwrap();
        *keys.ledger.lock().unwrap() = Arc::downgrade(&ledger);
        let mut pairings = crate::auth::TokenStore::open(
            oppen_core::ledger::PairingJournal::open(ledger.clone(), hmac).unwrap(),
        )
        .unwrap();
        let token = pairings
            .issue(Binding { agent, account })
            .unwrap()
            .reveal()
            .to_owned();
        let pairings = Arc::new(RwLock::new(pairings));
        let app = crate::server::router(gateway.clone(), pairings.clone());
        let mut runtime = Self {
            gateway,
            ledger,
            app,
            pairings,
            registry,
            token,
            session: String::new(),
            account,
            port,
        };
        let response = runtime.request("POST", Some(json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"execution-fixture","version":"0"}}
        }))).await;
        assert_eq!(response.status(), 200);
        runtime.session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .into();
        let _ = response.into_body().collect().await.unwrap();
        assert_eq!(
            runtime
                .request(
                    "POST",
                    Some(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                )
                .await
                .status(),
            202
        );
        runtime
    }

    async fn request(&self, method: &str, value: Option<Value>) -> axum::response::Response {
        let mut request = Request::builder()
            .method(method)
            .uri(crate::server::MCP_PATH)
            .header("host", "127.0.0.1:7433")
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if !self.session.is_empty() {
            request = request
                .header("mcp-session-id", &self.session)
                .header("mcp-protocol-version", "2025-06-18");
        }
        tokio::time::timeout(
            Duration::from_secs(15),
            self.app.clone().oneshot(
                request
                    .body(
                        value
                            .map(|v| Body::from(v.to_string()))
                            .unwrap_or_else(Body::empty),
                    )
                    .unwrap(),
            ),
        )
        .await
        .expect("MCP request deadline")
        .unwrap()
    }

    async fn call(&self, name: &str, arguments: Value) -> Value {
        let response = self.request("POST", Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}))).await;
        assert_eq!(response.status(), 200);
        let bytes = tokio::time::timeout(Duration::from_secs(15), response.into_body().collect())
            .await
            .expect("MCP response deadline")
            .unwrap()
            .to_bytes();
        let text = std::str::from_utf8(&bytes).unwrap();
        let reply: Value = if text.starts_with('{') {
            serde_json::from_str(text).unwrap()
        } else {
            text.lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|value| value.get("id") == Some(&json!(2)))
                .expect("MCP SSE result")
        };
        if let Some(error) = reply.get("error") {
            return json!({"protocol_error": error});
        }
        serde_json::from_str(
            reply["result"]["content"][0]["text"]
                .as_str()
                .expect("tool text"),
        )
        .unwrap()
    }

    async fn activate_orders(&self) {
        self.reconcile().await;
        let engine = &self.gateway.inner.engine;
        engine
            .operator_release_kill(&KillScope::Global, now_ms())
            .unwrap();
        self.acknowledge_policy();
    }

    fn acknowledge_policy(&self) {
        let engine = &self.gateway.inner.engine;
        engine
            .operator_acknowledge_policy(engine.policy_observation().unwrap(), now_ms())
            .unwrap();
    }

    async fn reconcile(&self) {
        let inner = &self.gateway.inner;
        let pump = FeedPump::new(
            &inner.feed,
            &self.ledger,
            self.account,
            HttpSource(InfoClient::loopback_fixture(self.port).unwrap()),
            &inner.alerts,
            &inner.quotes,
            &NoSocket,
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), pump.run(&mut rx))
            .await
            .unwrap();
        assert!(
            inner.feed.state().reconciled,
            "real startup reconciliation must succeed"
        );
        assert!(self.ledger.verify().unwrap().is_intact());
    }

    async fn shutdown(self) {
        assert!(self.request("DELETE", None).await.status().is_success());
        let weak = Arc::downgrade(&self.ledger);
        drop(self);
        tokio::time::timeout(Duration::from_secs(5), async {
            while weak.strong_count() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP session must release the physical ledger before restart");
    }
}

fn place(cloid: &str, size: &str) -> Value {
    json!({"symbol":"TEST","is_buy":true,"size":size,"order_type":"limit","limit_px":"100","tif":"gtc","cloid":cloid,"reason":"fixture order"})
}

#[tokio::test]
async fn same_cloid_limit_retry_retains_original_approval_after_new_quote_observation() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    let agent = AgentId::new("fixture-agent");
    let engine = &runtime.gateway.inner.engine;
    let mut policy = engine.guardrails(&agent).unwrap();
    policy.approval_required = true;
    engine
        .operator_set_guardrails(&agent, policy, now_ms())
        .unwrap();
    runtime.activate_orders().await;
    let cloid = Cloid::from_bytes([31; 16]).as_str().to_owned();
    let first = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(first["status"], "pending_approval", "{first}");
    let original = engine.pending_proposals(now_ms()).unwrap();
    assert_eq!(original.len(), 1);
    let observed_at = original[0]
        .order_intent()
        .unwrap()
        .original
        .as_ref()
        .unwrap()
        .reference_at_ms;
    tokio::time::timeout(Duration::from_secs(1), async {
        while now_ms() <= observed_at {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let retry = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(retry["status"], "pending_approval", "{retry}");
    assert_eq!(retry["approval_id"], first["approval_id"]);
    assert_eq!(retry["expires_at_ms"], first["expires_at_ms"]);
    assert_eq!(engine.pending_proposals(now_ms()).unwrap(), original);
    assert_eq!(
        runtime
            .ledger
            .get_events(0, 100)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == EventKind::ApprovalProposed)
            .count(),
        1
    );
    assert!(venue.submissions().is_empty());
    assert!(keys.read_heads.lock().unwrap().is_empty());
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn operator_halt_requested_sweep_retries_real_cancellation_without_closing_position() {
    use crate::server::{BoundServer, SupervisionControl, SupervisionStatus};
    use oppen_core::guardrail::KillReason;
    use tokio_util::sync::CancellationToken;

    async fn completed_after(control: &SupervisionControl, baseline: u64) -> SupervisionStatus {
        let mut observer = control.status();
        loop {
            let status = observer.borrow_and_update().clone();
            if status.completed_sequence > baseline {
                return status;
            }
            observer.changed().await.expect("supervision still owned");
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    runtime.activate_orders().await;
    let first = Cloid::from_bytes([111; 16]);
    let second = Cloid::from_bytes([112; 16]);
    for cloid in [&first, &second] {
        let placed = runtime.call("place", place(cloid.as_str(), "0.12")).await;
        assert_eq!(placed["status"], "resting", "{placed}");
    }
    venue.fill(first.as_str(), Decimal::new(5, 2));
    runtime.reconcile().await;
    let before = runtime
        .gateway
        .inner
        .info
        .clearinghouse_state(runtime.account)
        .await
        .unwrap();
    assert_eq!(before.asset_positions.len(), 1);

    // An earlier global stop gives the pre-request sweep genuine cancellation
    // work. The new operator action below adds a durable agent-specific halt.
    runtime
        .gateway
        .inner
        .engine
        .operator_engage_kill(KillScope::Global, KillReason::Operator, now_ms())
        .unwrap();
    venue.next_response(Behavior::Rejected);
    venue.next_response(Behavior::Rejected);
    let old_gate = venue.hold_info("frontendOpenOrders");
    let bound = BoundServer::bind(0, &runtime.gateway, &runtime.pairings)
        .await
        .unwrap();
    let control = bound.supervision_control();
    let shutdown = CancellationToken::new();
    let _shutdown_on_drop = shutdown.clone().drop_guard();
    let mut serving = tokio::spawn(bound.serve(
        runtime.gateway.clone(),
        runtime.pairings.clone(),
        shutdown.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(5), old_gate.entered.notified())
        .await
        .unwrap();
    let old = control.status().borrow().clone();
    assert!(old.in_progress);
    assert!(old.started_sequence > old.completed_sequence);
    let agent = AgentId::new("fixture-agent");
    runtime
        .gateway
        .inner
        .engine
        .operator_engage_kill(
            KillScope::Agent {
                agent: agent.clone(),
            },
            KillReason::Operator,
            now_ms(),
        )
        .unwrap();
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .paused_agents()
            .contains(&agent)
    );
    let baseline = control.request_sweep();
    assert_eq!(baseline, old.started_sequence);
    let requested_gate = venue.hold_info("frontendOpenOrders");
    old_gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), requested_gate.entered.notified())
        .await
        .unwrap();
    let active = control.status().borrow().clone();
    assert_eq!(
        active.completed_sequence, baseline,
        "old in-flight completion is not a receipt for the new halt"
    );
    assert!(active.started_sequence > baseline);
    assert!(active.in_progress);
    assert!(
        active.last_error.is_some(),
        "real rejected cancellation must remain visible"
    );

    // Dropping a timed-out observation cannot release the gated cancellation,
    // advance completion, or manufacture success in a later observer.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            completed_after(&control, baseline)
        )
        .await
        .is_err()
    );
    drop(control.status());
    let retained = control.status().borrow().clone();
    assert_eq!(retained.completed_sequence, baseline);
    assert!(retained.in_progress);
    assert_eq!(
        runtime
            .gateway
            .inner
            .info
            .frontend_open_orders(runtime.account)
            .await
            .unwrap()
            .len(),
        2
    );

    let retry_gate = venue.hold_info("frontendOpenOrders");
    let retry_baseline = control.request_sweep();
    assert_eq!(retry_baseline, active.started_sequence);
    requested_gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), retry_gate.entered.notified())
        .await
        .unwrap();
    let rejected = control.status().borrow().clone();
    assert_eq!(rejected.completed_sequence, retry_baseline);
    assert!(
        rejected.completed_sequence > baseline,
        "post-request failure is completion, not successful cancellation"
    );
    assert!(rejected.last_error.is_some());
    assert_eq!(
        runtime
            .gateway
            .inner
            .info
            .frontend_open_orders(runtime.account)
            .await
            .unwrap()
            .len(),
        2,
        "failed cancellation retains both actual resting orders"
    );
    assert!(
        runtime
            .gateway
            .runtime_cancellation_needed(&Binding {
                agent: agent.clone(),
                account: runtime.account
            })
            .await
            .unwrap()
    );
    retry_gate.release.notify_one();
    let success = tokio::time::timeout(
        Duration::from_secs(5),
        completed_after(&control, retry_baseline),
    )
    .await
    .unwrap();
    assert!(success.last_error.is_none(), "{success:?}");
    assert!(success.completed_sequence > retry_baseline);
    assert!(
        runtime
            .gateway
            .inner
            .info
            .frontend_open_orders(runtime.account)
            .await
            .unwrap()
            .is_empty()
    );
    let after = runtime
        .gateway
        .inner
        .info
        .clearinghouse_state(runtime.account)
        .await
        .unwrap();
    assert_eq!(
        after.asset_positions, before.asset_positions,
        "operator halt cancels resting orders, never closes the partial-fill position"
    );
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .paused_agents()
            .contains(&agent)
    );
    let submissions = venue.submissions();
    assert_eq!(
        submissions.len(),
        5,
        "two orders and three actual cancel attempts"
    );
    for cancel in &submissions[2..] {
        assert_eq!(cancel["action"]["type"], "cancel");
        assert_eq!(cancel["action"]["cancels"].as_array().unwrap().len(), 2);
        assert!(cancel["signature"].is_object());
    }
    assert!(runtime.ledger.verify().unwrap().is_intact());
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), &mut serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(control);
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn wrong_exchange_identity_or_extra_status_remains_unknown_over_real_http() {
    for behavior in [
        Behavior::WrongKind,
        Behavior::WrongKindError,
        Behavior::ExtraStatus,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let runtime =
            Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
        runtime.activate_orders().await;
        let cloid = Cloid::from_bytes([153; 16]);
        venue.next_response(behavior);
        let reply = runtime.call("place", place(cloid.as_str(), "0.12")).await;
        let error = &reply["protocol_error"]["data"];
        assert_eq!(
            error["code"], "timeout_unknown_outcome",
            "{behavior:?}: {reply}"
        );
        assert_eq!(error["retryable"], false);
        assert_eq!(error["cloid"], cloid.as_str());
        let pending = runtime
            .gateway
            .inner
            .submissions
            .state(runtime.account)
            .unwrap()
            .pending
            .unwrap();
        assert_eq!(pending.cloid(), &cloid);
        assert_eq!(venue.submissions().len(), 1);
        let events = runtime.ledger.get_events(0, 1000).unwrap().events;
        assert!(
            !events
                .iter()
                .any(|event| event.kind == EventKind::SubmissionResolved)
        );
        // The fixture actually applied the request. Only a subsequent venue
        // query, not the mismatched synchronous reply, can establish its state.
        let observed = runtime
            .call("get_order_status", json!({"cloid":cloid.as_str()}))
            .await;
        assert_eq!(observed["status"], "open", "{observed}");
        venue.next_response(Behavior::WrongKind);
        let canceled = runtime
            .call(
                "cancel",
                json!({"cloid":cloid.as_str(),"reason":"identity regression"}),
            )
            .await;
        assert_eq!(
            canceled["protocol_error"]["data"]["code"], "timeout_unknown_outcome",
            "{canceled}"
        );
        assert_eq!(canceled["protocol_error"]["data"]["retryable"], false);
        assert_eq!(venue.submissions().len(), 2);
        let observed = runtime
            .call("get_order_status", json!({"cloid":cloid.as_str()}))
            .await;
        assert_eq!(observed["status"], "canceled", "{observed}");
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn real_mcp_signs_reconciles_partial_fill_cancels_and_closes() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    let cloid = Cloid::from_bytes([7; 16]).as_str().to_owned();
    let blocked = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(blocked["status"], "rejected", "{blocked}");
    assert!(venue.submissions().is_empty());
    assert!(keys.read_heads.lock().unwrap().is_empty());
    runtime.activate_orders().await;
    let placed = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(placed["status"], "resting", "{placed}");
    let signed = venue.submissions();
    assert_eq!(signed.len(), 1);
    assert_eq!(signed[0]["action"]["orders"][0]["s"], "0.12");
    assert_eq!(signed[0]["action"]["orders"][0]["c"], cloid);
    assert!(matches!(
        signed[0]["signature"]["v"].as_u64(),
        Some(27 | 28)
    ));
    assert!(!keys.read_heads.lock().unwrap().is_empty());
    assert!(
        keys.read_heads
            .lock()
            .unwrap()
            .iter()
            .all(|kind| *kind == EventKind::SubmissionStarted)
    );
    let rejected = runtime
        .call("place", place(Cloid::from_bytes([8; 16]).as_str(), "0.14"))
        .await;
    assert_eq!(rejected["status"], "rejected", "{rejected}");
    assert_eq!(rejected["refusal"]["refusal"], "open_exposure");
    assert_eq!(rejected["refusal"]["observed_usd"], "26.00");
    assert_eq!(
        venue.submissions().len(),
        1,
        "resting order must consume headroom"
    );

    venue.fill(&cloid, Decimal::new(5, 2));
    runtime.reconcile().await;
    runtime.reconcile().await;
    let events = runtime.ledger.get_events(0, 100).unwrap().events;
    let fills: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::Fill)
        .collect();
    assert_eq!(fills.len(), 1, "repeated reconcile must dedupe fills");
    assert_eq!(fills[0].agent_id.as_deref(), Some("fixture-agent"));
    let canceled = runtime
        .call("cancel", json!({"cloid":cloid,"reason":"fixture cancel"}))
        .await;
    assert_eq!(canceled["status"], "canceled", "{canceled}");
    let closed = runtime
        .call(
            "close_position",
            json!({"symbol":"TEST","reason":"fixture close"}),
        )
        .await;
    assert_eq!(closed["status"], "filled", "{closed}");
    runtime.reconcile().await;
    let state = runtime
        .gateway
        .inner
        .info
        .clearinghouse_state(runtime.account)
        .await
        .unwrap();
    assert!(state.asset_positions.is_empty());
    let signed = venue.submissions();
    assert_eq!(signed.len(), 3);
    assert_eq!(signed[2]["action"]["orders"][0]["r"], true);
    assert!(
        signed
            .windows(2)
            .all(|pair| pair[0]["nonce"].as_u64() < pair[1]["nonce"].as_u64())
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn applied_but_malformed_response_survives_physical_restart() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    runtime.activate_orders().await;
    let cloid = Cloid::from_bytes([10; 16]).as_str().to_owned();
    venue.next_response(Behavior::AppliedMalformed);
    let outcome = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(
        outcome["protocol_error"]["data"]["code"], "timeout_unknown_outcome",
        "{outcome}"
    );
    assert!(
        runtime
            .gateway
            .inner
            .submissions
            .state(runtime.account)
            .unwrap()
            .pending
            .is_some()
    );
    runtime.shutdown().await;
    let restarted = Runtime::open(dir.path(), venue.port(), keys).await;
    assert!(
        restarted
            .gateway
            .inner
            .submissions
            .state(restarted.account)
            .unwrap()
            .pending
            .is_some()
    );
    restarted.activate_orders().await;
    let refused = restarted
        .call("place", place(Cloid::from_bytes([11; 16]).as_str(), "0.14"))
        .await;
    assert_eq!(refused["status"], "rejected", "{refused}");
    assert_eq!(refused["refusal"]["refusal"], "open_exposure");
    assert_eq!(
        venue.submissions().len(),
        1,
        "restart must not double-spend headroom or resubmit"
    );
    assert!(
        restarted
            .gateway
            .inner
            .submissions
            .state(restarted.account)
            .unwrap()
            .pending
            .is_none()
    );
    restarted.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn authoritative_exchange_rejection_releases_account_but_not_cloid_identity() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys).await;
    runtime.activate_orders().await;
    let cloid = Cloid::from_bytes([13; 16]).as_str().to_owned();
    venue.next_response(Behavior::Rejected);
    let rejected = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(
        rejected["protocol_error"]["data"]["code"], "venue_error",
        "{rejected}"
    );
    assert!(
        runtime
            .gateway
            .inner
            .submissions
            .state(runtime.account)
            .unwrap()
            .pending
            .is_none()
    );
    let next = runtime
        .call("place", place(Cloid::from_bytes([14; 16]).as_str(), "0.12"))
        .await;
    assert_eq!(next["status"], "resting", "{next}");
    let replay = runtime.call("place", place(&cloid, "0.12")).await;
    assert_eq!(
        replay["protocol_error"]["data"]["code"], "invalid_params",
        "{replay}"
    );
    assert_eq!(
        venue.submissions().len(),
        2,
        "used cloid must never reach the exchange again"
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}
