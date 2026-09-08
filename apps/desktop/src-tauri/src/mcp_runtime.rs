//! Explicit testnet supervision using existing authority. Never order activation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use oppen_core::alert::AlertStore;
use oppen_core::features::quotes::QuoteCache;
use oppen_core::feed::FeedSession;
use oppen_core::feed::pump::FeedPump;
use oppen_core::guardrail::GuardrailEngine;
use oppen_core::journal::Journal;
use oppen_core::keys::{KeyStore, KeychainKeyStore};
use oppen_core::ledger::{
    EventViews, Ledger, PairingJournal, PilotJournal, PolicyJournal, RegistryJournal,
};
use oppen_core::reconcile::{ReconcileSource, VenueSource};
use oppen_hl::Network;
use oppen_hl::ws::{Subscription, WsPool, WsPoolConfig};
use oppen_mcp::auth::{Binding, TokenStore};
use oppen_mcp::server::{BoundServer, Pairings};
use oppen_mcp::tools::Gateway;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::operator_halt::{HaltControl, HaltStatus};

const PORT: u16 = 7433;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum McpPhase {
    Idle,
    Starting,
    Listening,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct McpStatus {
    pub phase: McpPhase,
    pub network: Network,
    pub agent: Option<String>,
    pub account: Option<String>,
    pub listener: Option<String>,
    pub reconciled: Option<bool>,
    pub account_feeds_ready: Option<bool>,
    pub supervision_last_completed_ms: Option<u64>,
    pub supervision_in_progress: bool,
    pub supervision_error: Option<String>,
    pub orders_inhibited: bool,
    pub halt: HaltStatus,
    pub detail: Option<String>,
}

impl McpStatus {
    pub(crate) fn idle() -> Self {
        Self {
            phase: McpPhase::Idle,
            network: Network::Testnet,
            agent: None,
            account: None,
            listener: None,
            reconciled: None,
            account_feeds_ready: None,
            supervision_last_completed_ms: None,
            supervision_in_progress: false,
            supervision_error: None,
            orders_inhibited: true,
            halt: HaltStatus::default(),
            detail: None,
        }
    }

    pub(crate) fn starting(binding: &Binding) -> Self {
        Self {
            phase: McpPhase::Starting,
            agent: Some(binding.agent.to_string()),
            account: Some(binding.account.to_string()),
            ..Self::idle()
        }
    }
}

pub(crate) type SharedStatus = Arc<Mutex<McpStatus>>;

pub(crate) fn status_lock(status: &SharedStatus) -> std::sync::MutexGuard<'_, McpStatus> {
    status.lock().unwrap_or_else(|poison| poison.into_inner())
}

struct Prepared {
    binding: Binding,
    gateway: Gateway,
    pairings: Pairings,
    ledger: Arc<Ledger>,
    feed: Arc<FeedSession>,
    alerts: Arc<AlertStore>,
    quotes: Arc<QuoteCache>,
    engine: Arc<GuardrailEngine>,
}

impl Prepared {
    fn open(dir: &Path, binding: Binding, keys: Arc<dyn KeyStore>) -> Result<Self, String> {
        if keys.network() != Network::Testnet {
            return Err("MCP requires testnet keys".into());
        }
        let ledger = Arc::new(
            Ledger::open_existing(dir, Network::Testnet).map_err(|error| error.to_string())?,
        );
        let hmac = Arc::new(
            keys.load_hmac_key()
                .map_err(|error| error.to_string())?
                .ok_or("existing testnet authentication key required")?,
        );
        let registry = Arc::new(
            RegistryJournal::open(ledger.clone(), hmac.clone())
                .map_err(|error| error.to_string())?,
        );
        let route = registry
            .route_for_agent(&binding.agent)
            .map_err(|error| error.to_string())?;
        if route.network != Network::Testnet
            || route.binding.agent != binding.agent
            || route.binding.container != binding.account
        {
            return Err("requested identity differs from the authorized route".into());
        }
        let policy = Arc::new(PolicyJournal::new(registry.clone()));
        let current = policy.current().map_err(|error| error.to_string())?;
        if !current.state.guardrails.contains_key(&binding.agent) {
            return Err("verified policy does not contain the requested agent".into());
        }
        // Authorization is never inferred from flat venue state or pairings.
        let pilot = PilotJournal::new(registry)
            .status(binding.account)
            .map_err(|error| error.to_string())?
            .ok_or("existing pilot authorization required")?;
        if pilot.agent != binding.agent
            || pilot.account != binding.account
            || pilot.halt.is_some()
            || !matches!(
                pilot.accounting,
                oppen_core::ledger::PilotAccounting::Known { .. }
            )
        {
            return Err("pilot must match the requested identity and have no halt".into());
        }
        let pairings = TokenStore::open(
            PairingJournal::open(ledger.clone(), hmac).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if !pairings.supports_binding(&binding) {
            return Err(
                "retained pairings must be nonempty, available, and match the requested identity"
                    .into(),
            );
        }
        // The private engine starts inhibited. No caller receives an activation capability.
        let engine = Arc::new(
            GuardrailEngine::new_supervised_alpha(policy, keys)
                .map_err(|error| error.to_string())?,
        );
        let feed = Arc::new(FeedSession::new());
        let alerts = Arc::new(
            AlertStore::open(dir.join("alerts-testnet.db")).map_err(|error| error.to_string())?,
        );
        let quotes = Arc::new(QuoteCache::new());
        let gateway = Gateway::new(
            Network::Testnet,
            engine.clone(),
            EventViews::new(ledger.clone()),
            Arc::new(
                Journal::open(dir.join("journal-testnet.db")).map_err(|error| error.to_string())?,
            ),
            feed.clone(),
            alerts.clone(),
            quotes.clone(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            binding,
            gateway,
            pairings: Arc::new(RwLock::new(pairings)),
            ledger,
            feed,
            alerts,
            quotes,
            engine,
        })
    }
}

/// The handle stays in Runtime until the actual supervisor has joined.
pub(crate) struct OwnedMcp {
    stop: CancellationToken,
    task: Option<tauri::async_runtime::JoinHandle<Result<(), String>>>,
    completed: Option<Result<(), String>>,
    halt: Option<Arc<HaltControl>>,
}

impl Drop for OwnedMcp {
    fn drop(&mut self) {
        // Panic fallback only; successful ownership shutdown always awaits the task.
        self.stop.cancel();
    }
}

impl OwnedMcp {
    pub(crate) async fn start(
        dir: PathBuf,
        binding: Binding,
        status: SharedStatus,
        stop: CancellationToken,
    ) -> Result<Self, String> {
        let prepared = tauri::async_runtime::spawn_blocking(move || {
            Prepared::open(
                &dir,
                binding,
                Arc::new(KeychainKeyStore::new(Network::Testnet)),
            )
        })
        .await
        .map_err(|error| format!("MCP authority startup task: {error}"))??;
        if stop.is_cancelled() {
            status_lock(&status).phase = McpPhase::Stopped;
            return Ok(Self {
                stop,
                task: None,
                completed: Some(Ok(())),
                halt: None,
            });
        }
        let source = VenueSource::new(Network::Testnet).map_err(|error| error.to_string())?;
        Self::launch(
            prepared,
            PORT,
            source,
            || {
                WsPool::new(WsPoolConfig {
                    network: Network::Testnet,
                    ..WsPoolConfig::default()
                })
            },
            status,
            stop,
        )
        .await
    }

    async fn launch<S, F>(
        prepared: Prepared,
        port: u16,
        source: S,
        pool: F,
        status: SharedStatus,
        stop: CancellationToken,
    ) -> Result<Self, String>
    where
        S: ReconcileSource + Send + 'static,
        F: FnOnce() -> Result<
                (WsPool, tokio::sync::mpsc::Receiver<oppen_hl::ws::WsEvent>),
                oppen_hl::ws::PoolError,
            > + Send
            + 'static,
    {
        let (ready, observing) = oneshot::channel();
        let stopping = stop.clone();
        let task = tauri::async_runtime::spawn(async move {
            let result = run(
                prepared,
                port,
                source,
                pool,
                status.clone(),
                stopping,
                ready,
            )
            .await;
            let mut status = status_lock(&status);
            status.phase = if result.is_ok() {
                McpPhase::Stopped
            } else {
                McpPhase::Failed
            };
            status.listener = None;
            status.reconciled = None;
            status.account_feeds_ready = None;
            status.supervision_in_progress = false;
            status.detail = result.as_ref().err().cloned();
            result
        });
        let mut owned = Self {
            stop,
            task: Some(task),
            completed: None,
            halt: None,
        };
        match observing.await {
            Ok(halt) => owned.halt = Some(halt),
            Err(_) => owned.shutdown_and_drain().await?,
        }
        Ok(owned)
    }

    pub(crate) fn request_halt(&self, binding: &Binding) -> Result<(), String> {
        self.halt
            .as_ref()
            .ok_or("MCP supervisor is not available")?
            .request(binding)
    }

    pub(crate) async fn shutdown_and_drain(&mut self) -> Result<(), String> {
        self.stop.cancel();
        if let Some(result) = &self.completed {
            return result.clone();
        }
        let result = match self.task.as_mut() {
            Some(task) => task
                .await
                .map_err(|error| format!("MCP owner task: {error}"))
                .and_then(|result| result),
            None => Ok(()),
        };
        self.task = None;
        self.halt = None;
        self.completed = Some(result.clone());
        result
    }
}

async fn run<S, F>(
    prepared: Prepared,
    port: u16,
    source: S,
    pool: F,
    status: SharedStatus,
    stop: CancellationToken,
    ready: oneshot::Sender<Arc<HaltControl>>,
) -> Result<(), String>
where
    S: ReconcileSource + Send + 'static,
    F: FnOnce() -> Result<
        (WsPool, tokio::sync::mpsc::Receiver<oppen_hl::ws::WsEvent>),
        oppen_hl::ws::PoolError,
    >,
{
    let bound = BoundServer::bind(port, &prepared.gateway, &prepared.pairings)
        .await
        .map_err(|error| error.to_string())?;
    if stop.is_cancelled() {
        return Ok(());
    }
    let address = bound.local_addr();
    let supervision = bound.supervision_status();
    let halt = HaltControl::new(
        prepared.engine.clone(),
        prepared.binding.clone(),
        prepared.pairings.clone(),
        bound.supervision_control(),
        status.clone(),
    );
    let (pool, mut events) = pool().map_err(|error| error.to_string())?;
    let pool = Arc::new(pool);
    let (pump_stop, stopping) = oneshot::channel();
    let (quiesce, quiescing) = oneshot::channel();
    let pump_pool = pool.clone();
    let ledger = prepared.ledger.clone();
    let feed = prepared.feed.clone();
    let alerts = prepared.alerts.clone();
    let quotes = prepared.quotes.clone();
    let account = prepared.binding.account;
    let executor = tokio::runtime::Handle::current();
    // FeedPump performs synchronous ledger work; keep it off async workers.
    let pump = tauri::async_runtime::spawn_blocking(move || {
        executor.block_on(async move {
            let pump = FeedPump::new(
                &feed,
                &ledger,
                account,
                source,
                &alerts,
                &quotes,
                pump_pool.as_ref(),
            )
            .map_err(|error| error.to_string())?;
            pump.run_until_shutdown(&mut events, quiescing, stopping)
                .await;
            if let Some(failure) = feed.state().failure {
                return Err(failure);
            }
            Ok::<(), String>(())
        })
    });
    let mut failure = None;
    for subscription in [
        Subscription::UserFills { user: account },
        Subscription::OrderUpdates { user: account },
    ] {
        if let Err(error) = pool.subscribe(subscription) {
            failure = Some(error.to_string());
            break;
        }
    }
    let server_stop = CancellationToken::new();
    let server = if failure.is_none() && !stop.is_cancelled() {
        let stopping = server_stop.clone();
        let gateway = prepared.gateway.clone();
        let pairings = prepared.pairings.clone();
        Some(tauri::async_runtime::spawn(async move {
            bound.serve(gateway, pairings, stopping).await
        }))
    } else {
        None
    };
    if let Some(server) = server.as_ref() {
        {
            let mut status = status_lock(&status);
            status.phase = if stop.is_cancelled() {
                McpPhase::Stopping
            } else {
                McpPhase::Listening
            };
            status.listener = Some(address.to_string());
        }
        let server_finished = server.inner().abort_handle();
        let pump_finished = pump.inner().abort_handle();
        let observed_feed = prepared.feed.clone();
        let observed_pool = pool.clone();
        let observed_status = status.clone();
        let observed_halt = halt.clone();
        // Monitoring can fail independently. The parent keeps every actual
        // task handle and still drains them if this observer panics.
        let monitor = tauri::async_runtime::spawn(async move {
            let mut refresh = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => return None,
                    _ = refresh.tick() => {
                        let sweep = supervision.borrow().clone();
                        observed_halt.observe(&sweep);
                        let feed = observed_feed.state();
                        let fresh = observed_pool.stale_feeds(&[
                            Subscription::UserFills { user: account },
                            Subscription::OrderUpdates { user: account }
                        ]).is_empty();
                        {
                            let mut status = status_lock(&observed_status);
                            status.supervision_last_completed_ms = sweep.last_completed_ms;
                            status.supervision_in_progress = sweep.in_progress;
                            status.supervision_error = sweep.last_error;
                            status.reconciled = Some(feed.reconciled);
                            status.account_feeds_ready = Some(fresh);
                        }
                        if let Some(detail) = feed.failure { return Some(detail); }
                        if pump_finished.is_finished() || server_finished.is_finished() {
                            return Some("MCP listener or reconciliation task ended unexpectedly".into());
                        }
                    }
                }
            }
        });
        let _ = ready.send(halt.clone());
        failure = match monitor.await {
            Ok(failure) => failure,
            Err(error) => Some(format!("MCP monitor task: {error}")),
        };
    }
    status_lock(&status).phase = McpPhase::Stopping;
    // Keep the existing cancellation supervisor alive through every admitted
    // mutation and its first subsequent sweep attempt, even after IPC drop.
    if let Err(error) = halt.close_and_drain().await {
        failure.get_or_insert(error);
    }
    server_stop.cancel();
    if let Some(server) = server {
        match server.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                failure.get_or_insert(error.to_string());
            }
            Err(error) => {
                failure.get_or_insert(format!("MCP server task: {error}"));
            }
        }
    }
    halt.supervision_ended();
    let (acknowledge, acknowledged) = oneshot::channel();
    if quiesce.send(acknowledge).is_err() {
        failure.get_or_insert("MCP pump ended before quiescence".into());
    }
    if acknowledged.await.is_err() {
        failure.get_or_insert("MCP pump did not acknowledge quiescence".into());
    }
    if let Err(error) = pool.shutdown_and_drain().await {
        failure.get_or_insert(error.to_string());
    }
    // No socket producer can publish after this point. Close admission to the
    // event receiver, then consume every queued event before releasing journals.
    let _ = pump_stop.send(());
    match pump.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            failure.get_or_insert(error);
        }
        Err(error) => {
            failure.get_or_insert(format!("MCP pump task: {error}"));
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator_halt::{CancellationPhase, HaltPhase};
    use oppen_core::guardrail::{AgentGuardrails, AgentId, LegacyPolicyReview, PersistedState};
    use oppen_core::keys::{AgentWallet, EntryName, HmacKey, KeyStoreError, SecretText};
    use oppen_core::ledger::{RegistryBinding, now_ms};
    use oppen_hl::Address;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    struct FixtureKeys;

    impl KeyStore for FixtureKeys {
        fn network(&self) -> Network {
            Network::Testnet
        }
        fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
            assert_eq!(
                format!("{entry:?}"),
                r#"EntryName { service: "xyz.oppen.testnet", account: "guardrail-hmac" }"#,
                "fixture must not load a signing key"
            );
            Ok(Some(SecretText::new("0b".repeat(32))))
        }
        fn write(&self, _: &EntryName, _: &str) -> Result<(), KeyStoreError> {
            panic!("startup must not provision credentials");
        }
        fn remove(&self, _: &EntryName) -> Result<(), KeyStoreError> {
            panic!("startup must not mutate credentials");
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        binding: Binding,
        token: Option<String>,
    }

    struct FixtureSource;

    struct BlockedSource {
        entered: Mutex<Option<oneshot::Sender<()>>>,
        release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
        panic: bool,
    }

    impl ReconcileSource for BlockedSource {
        fn network(&self) -> Network {
            Network::Testnet
        }
        async fn user_fills_by_time(
            &self,
            _: Address,
            _: u64,
            _: Option<u64>,
        ) -> Result<Vec<oppen_hl::types::Fill>, oppen_hl::Error> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let release = self.release.lock().await.take();
            if let Some(release) = release {
                let _ = release.await;
            }
            assert!(!self.panic, "controlled reconcile task panic");
            Err(oppen_hl::Error::Venue {
                status: 503,
                message: "controlled reconcile failure".into(),
            })
        }
        async fn frontend_open_orders(
            &self,
            _: Address,
        ) -> Result<Vec<oppen_hl::types::OpenOrder>, oppen_hl::Error> {
            Ok(Vec::new())
        }
        async fn order_status(
            &self,
            _: Address,
            _: oppen_hl::OrderRef,
        ) -> Result<oppen_hl::types::OrderStatusResponse, oppen_hl::Error> {
            panic!("blocked source has no orders");
        }
    }

    impl ReconcileSource for FixtureSource {
        fn network(&self) -> Network {
            Network::Testnet
        }
        async fn user_fills_by_time(
            &self,
            _: Address,
            _: u64,
            _: Option<u64>,
        ) -> Result<Vec<oppen_hl::types::Fill>, oppen_hl::Error> {
            Ok(Vec::new())
        }
        async fn frontend_open_orders(
            &self,
            _: Address,
        ) -> Result<Vec<oppen_hl::types::OpenOrder>, oppen_hl::Error> {
            Ok(Vec::new())
        }
        async fn order_status(
            &self,
            _: Address,
            _: oppen_hl::OrderRef,
        ) -> Result<oppen_hl::types::OrderStatusResponse, oppen_hl::Error> {
            Ok(serde_json::from_value(serde_json::json!({"status": "unknownOid"})).unwrap())
        }
    }

    struct LocalVenue {
        port: u16,
        requests: Arc<Mutex<Vec<LocalRequest>>>,
        stop: CancellationToken,
        task: Option<tokio::task::JoinHandle<()>>,
    }

    struct LocalRequest {
        path: String,
        body: Vec<u8>,
    }

    impl LocalVenue {
        async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = CancellationToken::new();
            let stopping = stop.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let task = tokio::spawn(async move {
                let mut clients = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        () = stopping.cancelled() => break,
                        done = clients.join_next(), if !clients.is_empty() => { done.unwrap().unwrap(); },
                        client = listener.accept() => {
                            let (socket, _) = client.unwrap();
                            let recorded = recorded.clone();
                            clients.spawn(async move {
                                let mut socket = BufReader::new(socket);
                                let mut line = String::new();
                                if socket.read_line(&mut line).await.unwrap() == 0 {
                                    // A canceled client may close before sending a request.
                                    return;
                                }
                                let websocket = line.starts_with("GET /");
                                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                                let mut length = 0;
                                loop {
                                    line.clear();
                                    assert!(socket.read_line(&mut line).await.unwrap() > 0);
                                    if line == "\r\n" { break; }
                                    let (name, value) = line.split_once(':').unwrap();
                                    if name.eq_ignore_ascii_case("content-length") { length = value.trim().parse::<usize>().unwrap(); }
                                }
                                assert!(length < 65_536);
                                let mut body = vec![0; length];
                                socket.read_exact(&mut body).await.unwrap();
                                recorded.lock().unwrap().push(LocalRequest { path: path.clone(), body: body.clone() });
                                let (code, response) = if websocket {
                                    // No public venue, and no fictional healthy account socket.
                                    ("503 Service Unavailable", String::new())
                                } else {
                                    assert_eq!(path, "/info", "unexpected venue operation");
                                    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                                    ("200 OK", Self::info(request).to_string())
                                };
                                let response = format!("HTTP/1.1 {code}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}", response.len());
                                let _ = socket.write_all(response.as_bytes()).await;
                            });
                        }
                    }
                }
                clients.abort_all();
                while let Some(done) = clients.join_next().await {
                    if let Err(error) = done {
                        assert!(error.is_cancelled(), "fixture handler panicked: {error}");
                    }
                }
            });
            Self {
                port,
                requests,
                stop,
                task: Some(task),
            }
        }

        fn info(request: serde_json::Value) -> serde_json::Value {
            use serde_json::json;
            let now = u64::try_from(now_ms()).unwrap();
            let meta = json!({"universe":[{"name":"TEST","szDecimals":2,"maxLeverage":10,
                "marginTableId":0,"isDelisted":false,"onlyIsolated":false}]});
            match request["type"].as_str().unwrap() {
                "meta" => meta,
                "metaAndAssetCtxs" => json!([meta, [{
                    "funding":"0","openInterest":"1","prevDayPx":"100","dayNtlVlm":"1000",
                    "premium":"0","oraclePx":"100","markPx":"100","midPx":"100","impactPxs":["100","100"]
                }]]),
                "clearinghouseState" => {
                    let summary = json!({"accountValue":"100","totalNtlPos":"0","totalRawUsd":"100","totalMarginUsed":"0"});
                    json!({"marginSummary":summary,"crossMarginSummary":summary,
                        "crossMaintenanceMarginUsed":"0","assetPositions":[],"withdrawable":"100","time":now})
                }
                "spotClearinghouseState" => json!({"balances":[]}),
                "frontendOpenOrders" | "userFillsByTime" => json!([]),
                "portfolio" => json!([["day", {
                    "accountValueHistory":[[now.saturating_sub(86_400_000),"100"],[now,"100"]],
                    "pnlHistory":[[now,"0"]],"vlm":"0"
                }]]),
                _ => panic!("unexpected venue read: {request}"),
            }
        }

        async fn shutdown(mut self) {
            self.stop.cancel();
            self.task.take().unwrap().await.unwrap();
        }
    }

    impl Drop for LocalVenue {
        fn drop(&mut self) {
            self.stop.cancel();
            if let Some(task) = &self.task {
                task.abort();
            }
        }
    }

    #[tokio::test]
    async fn local_venue_accepts_connection_closed_before_request() {
        let venue = LocalVenue::start().await;
        let mut socket = TcpStream::connect(("127.0.0.1", venue.port)).await.unwrap();
        socket.shutdown().await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.is_empty());
        assert!(venue.requests.lock().unwrap().is_empty());
        // Joining also observes any handler panic, rather than aborting the
        // handler before it has consumed the connection's EOF.
        venue.shutdown().await;
    }

    async fn rpc(
        address: &str,
        token: &str,
        session: Option<&str>,
        body: serde_json::Value,
    ) -> (u16, Option<String>, String) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut socket = TcpStream::connect(address).await.unwrap();
            let body = serde_json::to_string(&body).unwrap();
            let session = session.map(|session| format!("Mcp-Session-Id: {session}\r\n")).unwrap_or_default();
            let request = format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Protocol-Version: 2025-06-18\r\n{session}Connection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            socket.write_all(request.as_bytes()).await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let status = line.split_whitespace().nth(1).unwrap().parse::<u16>().unwrap();
            let mut session = None;
            let mut chunked = false;
            loop {
                line.clear();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" { break; }
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("mcp-session-id") { session = Some(value.trim().to_owned()); }
                if name.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = value.split(',').any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"));
                }
            }
            let mut body = Vec::new();
            socket.read_to_end(&mut body).await.unwrap();
            (status, session, decode_rpc_body(&body, chunked))
        }).await.expect("bounded loopback MCP response")
    }

    fn decode_rpc_body(mut bytes: &[u8], chunked: bool) -> String {
        if !chunked {
            return String::from_utf8(bytes.to_vec()).unwrap();
        }
        let mut body = Vec::new();
        loop {
            let end = bytes.windows(2).position(|pair| pair == b"\r\n").unwrap();
            let header = std::str::from_utf8(&bytes[..end]).unwrap();
            let size = usize::from_str_radix(header.split(';').next().unwrap(), 16).unwrap();
            bytes = &bytes[end + 2..];
            if size == 0 {
                return String::from_utf8(body).unwrap();
            }
            body.extend_from_slice(&bytes[..size]);
            assert_eq!(&bytes[size..size + 2], b"\r\n");
            bytes = &bytes[size + 2..];
        }
    }

    #[test]
    fn rpc_body_decodes_chunks_inside_an_sse_json_line() {
        let body = b"data: {\"result\":true}\n\n";
        let mut wire = Vec::new();
        for chunk in body.chunks(3) {
            wire.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\n\r\n");
        assert_eq!(rpc_reply(&decode_rpc_body(&wire, true))["result"], true);
        assert_eq!(decode_rpc_body(body, false).as_bytes(), body);
    }

    fn rpc_reply(bytes: &str) -> serde_json::Value {
        serde_json::from_str(
            bytes
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                // rmcp sends an empty SSE priming event before the RPC result.
                .find(|data| !data.trim().is_empty())
                .unwrap_or(bytes.trim()),
        )
        .unwrap_or_else(|error| panic!("actual JSON-RPC response: {error}; bytes={bytes:?}"))
    }

    fn predecision_busy(reply: &serde_json::Value) -> bool {
        reply.get("result").is_none()
            && ["registry reader busy", "decision worker busy"]
                .iter()
                .any(|what| {
                    reply["error"]
                        == serde_json::json!({
                            "code": -32603,
                            "message": format!("{what} is unavailable: no permits available"),
                            "data": {"contract_version":0,"code":"unavailable","retryable":true,
                                "detail":"no permits available","cloid":null}
                        })
                })
    }

    async fn retry_predecision_busy<F, Fut>(mut request: F) -> serde_json::Value
    where
        F: FnMut(usize) -> Fut,
        Fut: std::future::Future<Output = serde_json::Value>,
    {
        tokio::time::timeout(Duration::from_secs(5), async {
            for attempt in 0..64 {
                let reply = request(attempt).await;
                if !predecision_busy(&reply) {
                    return reply;
                }
                // These exact refusals occur before evaluate/sign/submit, not
                // after an ambiguous HTTP exchange. Never retry other errors.
                assert!(
                    attempt < 63,
                    "predecision contention retry budget exhausted: {reply}"
                );
                tokio::task::yield_now().await;
            }
            unreachable!("last busy response exhausts the retry budget")
        })
        .await
        .expect("predecision contention deadline exceeded")
    }

    #[tokio::test]
    async fn fixture_retries_only_exact_predecision_busy_responses() {
        use serde_json::json;
        let busy = |what: &str| {
            json!({"error":{
                "code":-32603,"message":format!("{what} is unavailable: no permits available"),
                "data":{"contract_version":0,"code":"unavailable","retryable":true,"detail":"no permits available","cloid":null}
            }})
        };
        let final_reply = json!({"result":{"status":"rejected"}});
        let responses = [
            busy("registry reader busy"),
            busy("decision worker busy"),
            final_reply.clone(),
        ];
        let calls = std::cell::Cell::new(0);
        let result = retry_predecision_busy(|attempt| {
            calls.set(calls.get() + 1);
            std::future::ready(responses[attempt].clone())
        })
        .await;
        assert_eq!(result, final_reply);
        assert_eq!(calls.get(), 3);

        let mut unsafe_replies = vec![busy("registry read timeout"), busy("other unavailable")];
        let mut ambiguous = busy("registry reader busy");
        ambiguous["error"]["data"]["code"] = json!("timeout_unknown_outcome");
        unsafe_replies.push(ambiguous);
        let mut nonretryable = busy("registry reader busy");
        nonretryable["error"]["data"]["retryable"] = json!(false);
        unsafe_replies.push(nonretryable);
        let mut cloid = busy("registry reader busy");
        cloid["error"]["data"]["cloid"] = json!("0x77777777777777777777777777777777");
        unsafe_replies.push(cloid);
        for reply in unsafe_replies {
            calls.set(0);
            let observed = retry_predecision_busy(|_| {
                calls.set(calls.get() + 1);
                std::future::ready(reply.clone())
            })
            .await;
            assert_eq!(observed, reply);
            assert_eq!(calls.get(), 1);
        }
    }

    impl Fixture {
        fn binding() -> Binding {
            Binding {
                agent: AgentId::new("fixture-agent"),
                account: Address::from_bytes([7; 20]),
            }
        }

        fn authorized() -> Self {
            Self::new(true, Some(Self::binding()), false)
        }

        fn new(pilot: bool, paired: Option<Binding>, revoke: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let binding = Binding {
                agent: AgentId::new("fixture-agent"),
                account: Address::from_bytes([7; 20]),
            };
            let ledger = Arc::new(
                Ledger::open_at(
                    &dir.path().join(oppen_core::db_file_name(Network::Testnet)),
                    Network::Testnet,
                )
                .unwrap(),
            );
            let hmac = Arc::new(HmacKey::from_bytes([11; 32]));
            let registry = Arc::new(RegistryJournal::open(ledger.clone(), hmac.clone()).unwrap());
            let at = u64::try_from(now_ms()).unwrap();
            registry
                .grant(
                    RegistryBinding {
                        agent: binding.agent.clone(),
                        container: binding.account,
                        vault_address: None,
                        wallet: AgentWallet {
                            generation: 0,
                            address: Address::from_bytes([9; 20]),
                            approved_at_ms: at,
                            valid_until_ms: at + 86_400_000,
                        },
                    },
                    at,
                )
                .unwrap();
            let policy = PolicyJournal::new(registry.clone());
            let review =
                LegacyPolicyReview::open(dir.path().join("legacy.db"), Network::Testnet, at)
                    .unwrap();
            let mut state = PersistedState::paused(at);
            state
                .guardrails
                .insert(binding.agent.clone(), AgentGuardrails::default());
            policy.initialize(&review, state, at).unwrap();
            if pilot {
                PilotJournal::new(registry)
                    .authorize(binding.agent.clone(), binding.account, at)
                    .unwrap();
            }
            let mut pairings =
                TokenStore::open(PairingJournal::open(ledger, hmac).unwrap()).unwrap();
            let token = paired.map(|binding| {
                let token = pairings.issue(binding).unwrap();
                if revoke {
                    pairings.revoke(token.id).unwrap();
                }
                token.reveal().to_owned()
            });
            Self {
                dir,
                binding,
                token,
            }
        }

        fn prepare(&self) -> Result<Prepared, String> {
            Prepared::open(self.dir.path(), self.binding.clone(), Arc::new(FixtureKeys))
        }

        fn assert_pairing_owner_released(&self) {
            let ledger = Arc::new(
                Ledger::open_at(
                    &self
                        .dir
                        .path()
                        .join(oppen_core::db_file_name(Network::Testnet)),
                    Network::Testnet,
                )
                .unwrap(),
            );
            let store = TokenStore::open(
                PairingJournal::open(ledger, Arc::new(HmacKey::from_bytes([11; 32]))).unwrap(),
            )
            .unwrap();
            assert!(store.supports_binding(&self.binding));
        }
    }

    #[test]
    fn startup_refuses_missing_anchor_without_adopting_existing_authority() {
        let fixture = Fixture::authorized();
        let path = fixture
            .dir
            .path()
            .join(oppen_core::db_file_name(Network::Testnet));
        let anchor = oppen_core::ledger::FileAnchor::beside(&path);
        std::fs::remove_file(anchor.path()).unwrap();
        assert!(fixture.prepare().is_err());
        assert!(
            !anchor.path().exists(),
            "startup must not adopt an unanchored ledger"
        );
    }

    #[test]
    fn startup_refuses_missing_database_without_creating_authority() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Prepared::open(dir.path(), Fixture::binding(), Arc::new(FixtureKeys)).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn missing_pilot_refuses_before_any_cleanup_runtime_can_be_created() {
        let fixture = Fixture::new(false, None, false);
        assert!(
            matches!(fixture.prepare(), Err(detail) if detail == "existing pilot authorization required")
        );
    }

    #[test]
    fn foreign_retained_pairing_refuses_even_when_revoked() {
        for revoked in [false, true] {
            let foreign = Binding {
                agent: AgentId::new("other-agent"),
                account: Address::from_bytes([8; 20]),
            };
            let fixture = Fixture::new(true, Some(foreign), revoked);
            assert!(
                matches!(fixture.prepare(), Err(detail) if detail.contains("retained pairings"))
            );
        }
    }

    #[test]
    fn existing_authority_opens_without_issuing_or_acknowledging() {
        let fixture = Fixture::authorized();
        assert!(fixture.token.is_some());
        let before = Ledger::open_at(
            &fixture
                .dir
                .path()
                .join(oppen_core::db_file_name(Network::Testnet)),
            Network::Testnet,
        )
        .unwrap()
        .chain_head()
        .unwrap();
        let prepared = fixture.prepare().unwrap();
        assert!(prepared.engine.policy_status().admission_inhibited);
        assert!(
            prepared
                .pairings
                .read()
                .unwrap()
                .supports_binding(&fixture.binding)
        );
        assert_eq!(prepared.ledger.chain_head().unwrap(), before);
        drop(prepared);
        let reopened = fixture.prepare().unwrap();
        assert_eq!(reopened.ledger.chain_head().unwrap(), before);
    }

    #[tokio::test]
    async fn occupied_port_fails_before_creating_any_venue_pool() {
        let fixture = Fixture::authorized();
        let prepared = fixture.prepare().unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let status = Arc::new(Mutex::new(McpStatus::starting(&fixture.binding)));
        let failed = OwnedMcp::launch(
            prepared,
            listener.local_addr().unwrap().port(),
            FixtureSource,
            || panic!("occupied port must fail before any venue subscription"),
            status.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(status_lock(&status).phase, McpPhase::Failed);
        assert!(status_lock(&status).listener.is_none());
        // The failed startup released the exclusive pairing owner.
        fixture.prepare().unwrap();
    }

    #[tokio::test]
    async fn subscription_failure_joins_pump_and_releases_authority() {
        let fixture = Fixture::authorized();
        let status = Arc::new(Mutex::new(McpStatus::starting(&fixture.binding)));
        let failed = OwnedMcp::launch(
            fixture.prepare().unwrap(),
            0,
            FixtureSource,
            || {
                let (pool, events) = WsPool::loopback_fixture(1)?;
                pool.shutdown();
                Ok((pool, events))
            },
            status.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(status_lock(&status).phase, McpPhase::Failed);
        fixture.prepare().unwrap();
    }

    #[tokio::test]
    async fn real_mcp_listener_refuses_order_before_exchange_and_drains_all_owners() {
        let fixture = Fixture::authorized();
        let venue = LocalVenue::start().await;
        let mut prepared = fixture.prepare().unwrap();
        assert!(prepared.engine.policy_status().admission_inhibited);
        let evidence = prepared.ledger.clone();
        prepared.gateway = prepared.gateway.with_loopback_fixture(venue.port).unwrap();
        let runtime = crate::runtime::Runtime::new(fixture.dir.path().to_owned());
        let port = venue.port;
        let observing = runtime
            .launch_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
                move |_, _, status, stop| async move {
                    OwnedMcp::launch(
                        prepared,
                        0,
                        FixtureSource,
                        move || WsPool::loopback_fixture(port),
                        status,
                        stop,
                    )
                    .await
                },
            )
            .unwrap();
        drop(observing);
        let address = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = runtime.mcp_status();
                assert_ne!(status.phase, McpPhase::Failed, "{:?}", status.detail);
                if let Some(address) = status.listener {
                    break address;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Admission is independent of the command observer. The same running
        // engine must persist the exact scope before the real MCP refusal.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match runtime.halt_mcp(
                    fixture.binding.agent.to_string(),
                    fixture.binding.account.to_string(),
                ) {
                    Ok(admitted) => {
                        assert!(matches!(
                            admitted.halt.phase,
                            HaltPhase::Persisting | HaltPhase::Persisted
                        ));
                        break;
                    }
                    Err(crate::runtime::RuntimeError::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("halt admission: {error}"),
                }
            }
        })
        .await
        .unwrap();
        let halted = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = runtime.mcp_status();
                if status.halt.cancellation == CancellationPhase::Acknowledged {
                    break status;
                }
                assert_ne!(status.halt.phase, HaltPhase::Uncertain, "{status:?}");
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(halted.halt.phase, HaltPhase::Persisted);
        assert_eq!(halted.halt.durable_revision, None);
        let duplicate = runtime
            .halt_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
            )
            .unwrap();
        assert_eq!(duplicate.halt.requested_at_ms, halted.halt.requested_at_ms);
        let token = fixture.token.as_deref().unwrap();
        let (code, session, body) = rpc(&address, token, None, serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"desktop-owner-fixture","version":"0"}}
        })).await;
        assert_eq!(code, 200);
        let initialized = rpc_reply(&body);
        assert!(initialized.get("error").is_none(), "{initialized}");
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
        let session = session.unwrap();
        assert_eq!(
            rpc(
                &address,
                token,
                Some(&session),
                serde_json::json!({
                    "jsonrpc":"2.0","method":"notifications/initialized"
                })
            )
            .await
            .0,
            202
        );
        let (code, _, body) = rpc(&address, token, Some(&session), serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_events","arguments":{}}
        })).await;
        assert_eq!(code, 200);
        let events_rpc = rpc_reply(&body);
        assert!(events_rpc.get("error").is_none(), "{events_rpc}");
        let page: serde_json::Value =
            serde_json::from_str(events_rpc["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(page["contract_version"], 0);
        assert!(page["events"].is_array());
        let before = evidence.chain_head().unwrap().seq;
        let rpc_reply = retry_predecision_busy(|attempt| {
            let address = &address;
            let session = &session;
            async move {
                let (code, _, bytes) = rpc(address, token, Some(session), serde_json::json!({
            "jsonrpc":"2.0","id":3 + attempt,"method":"tools/call","params":{
                "name":"place","arguments":{"symbol":"TEST","is_buy":true,"size":"0.15",
                    "order_type":"limit","limit_px":"100","reason":"fixture must remain inhibited",
                    "cloid":"0x77777777777777777777777777777777"}
            }
        })).await;
                assert_eq!(code, 200);
                rpc_reply(&bytes)
            }
        })
        .await;
        assert!(rpc_reply.get("error").is_none(), "{rpc_reply}");
        let reply: serde_json::Value =
            serde_json::from_str(rpc_reply["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(reply["status"], "rejected");
        assert_eq!(reply["code"], "trading_paused");
        assert_eq!(reply["refusal"]["refusal"], "trading_paused");
        let events = evidence.get_events(before, 100).unwrap().events;
        assert!(events.iter().any(|event| {
            event.kind == oppen_core::ledger::EventKind::Refusal
                && event
                    .payload
                    .as_ref()
                    .is_some_and(|payload| payload["refusal_detail"]["refusal"] == "trading_paused")
        }));
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            oppen_core::ledger::EventKind::OrderIntent
                | oppen_core::ledger::EventKind::SubmissionStarted
        )));
        {
            let requests = venue.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.path == "/exchange")
                    .count(),
                0
            );
            assert!(
                requests
                    .iter()
                    .filter(|request| request.path == "/info")
                    .any(
                        |request| serde_json::from_slice::<serde_json::Value>(&request.body)
                            .unwrap()["type"]
                            == "clearinghouseState"
                    )
            );
        }
        assert!(runtime.mcp_status().orders_inhibited);
        assert_ne!(runtime.mcp_status().account_feeds_ready, Some(true));
        tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runtime.mcp_status().phase, McpPhase::Stopped);
        assert!(TcpStream::connect(&address).await.is_err());
        drop(evidence);
        let reopened = fixture.prepare().unwrap();
        let kill = serde_json::to_value(reopened.engine.kill_switch()).unwrap();
        assert_eq!(
            kill["agents"][fixture.binding.agent.as_str()]["reason"]["reason"],
            "operator"
        );
        assert_eq!(
            kill["agents"][fixture.binding.agent.as_str()]["engaged_at_ms"],
            halted.halt.requested_at_ms.unwrap()
        );
        assert!(reopened.engine.cancellation_needed(&fixture.binding.agent));
        drop(reopened);
        venue.shutdown().await;
    }

    #[tokio::test]
    async fn halt_stop_retains_blocked_mutation_and_rejects_foreign_scope() {
        let fixture = Fixture::authorized();
        let runtime = crate::runtime::Runtime::new(fixture.dir.path().to_owned());
        assert!(
            runtime
                .halt_mcp(
                    fixture.binding.agent.to_string(),
                    fixture.binding.account.to_string()
                )
                .is_err()
        );
        let venue = LocalVenue::start().await;
        let mut prepared = fixture.prepare().unwrap();
        let engine = prepared.engine.clone();
        prepared.gateway = prepared.gateway.with_loopback_fixture(venue.port).unwrap();
        let port = venue.port;
        runtime
            .launch_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
                move |_, _, status, stop| async move {
                    OwnedMcp::launch(
                        prepared,
                        0,
                        FixtureSource,
                        move || WsPool::loopback_fixture(port),
                        status,
                        stop,
                    )
                    .await
                },
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert!(
            runtime
                .halt_mcp("foreign-agent".into(), fixture.binding.account.to_string())
                .is_err()
        );
        assert!(
            runtime
                .halt_mcp(
                    fixture.binding.agent.to_string(),
                    Address::from_bytes([8; 20]).to_string()
                )
                .is_err()
        );
        assert_eq!(runtime.mcp_status().halt.phase, HaltPhase::Idle);
        let mut path = fixture
            .dir
            .path()
            .join(oppen_core::db_file_name(Network::Testnet))
            .into_os_string();
        path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        lock.lock().unwrap();
        let admitted = runtime
            .halt_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
            )
            .unwrap();
        assert_eq!(admitted.halt.phase, HaltPhase::Persisting);
        drop(admitted);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let kill = serde_json::to_value(engine.kill_switch()).unwrap();
                if kill["agents"].get(fixture.binding.agent.as_str()).is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), runtime.shutdown())
                .await
                .is_err()
        );
        assert_eq!(runtime.mcp_status().halt.phase, HaltPhase::Persisting);
        assert!(
            runtime
                .halt_mcp(
                    fixture.binding.agent.to_string(),
                    fixture.binding.account.to_string()
                )
                .is_err()
        );
        lock.unlock().unwrap();
        tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime.mcp_status().halt.cancellation,
            CancellationPhase::Acknowledged
        );
        assert_eq!(runtime.mcp_status().phase, McpPhase::Stopped);
        // Refresh cannot erase the per-agent emergency overlay.
        engine.policy_observation().unwrap();
        let kill = serde_json::to_value(engine.kill_switch()).unwrap();
        assert!(kill["agents"].get(fixture.binding.agent.as_str()).is_some());
        drop(engine);
        fixture.assert_pairing_owner_released();
        venue.shutdown().await;
    }

    #[tokio::test]
    async fn halt_failed_sweep_retries_on_existing_supervisor_without_another_mutation() {
        let fixture = Fixture::authorized();
        let venue = LocalVenue::start().await;
        let mut prepared = fixture.prepare().unwrap();
        let pairings = prepared.pairings.clone();
        let engine = prepared.engine.clone();
        prepared.gateway = prepared.gateway.with_loopback_fixture(venue.port).unwrap();
        let runtime = crate::runtime::Runtime::new(fixture.dir.path().to_owned());
        let port = venue.port;
        runtime
            .launch_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
                move |_, _, status, stop| async move {
                    OwnedMcp::launch(
                        prepared,
                        0,
                        FixtureSource,
                        move || WsPool::loopback_fixture(port),
                        status,
                        stop,
                    )
                    .await
                },
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        // The real sweep cannot acquire a binding snapshot. Its completed error
        // must not be mistaken for target cancellation acknowledgments.
        let (holding, held) = oneshot::channel();
        let (release, releasing) = std::sync::mpsc::channel();
        let locked_pairings = pairings.clone();
        let holder = tokio::task::spawn_blocking(move || {
            let _held = locked_pairings.write().unwrap();
            holding.send(()).unwrap();
            let _ = releasing.recv();
        });
        held.await.unwrap();
        runtime
            .halt_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
            )
            .unwrap();
        let retrying = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = runtime.mcp_status();
                assert_ne!(status.halt.cancellation, CancellationPhase::Acknowledged);
                if status.halt.cancellation == CancellationPhase::Retrying {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(retrying.halt.phase, HaltPhase::Persisted);
        assert!(retrying.halt.cancellation_error.is_some());
        let revision = engine.policy_status().cached_revision;
        release.send(()).unwrap();
        holder.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if runtime.mcp_status().halt.cancellation == CancellationPhase::Acknowledged {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(engine.policy_status().cached_revision, revision);
        assert_eq!(
            runtime.mcp_status().halt.requested_at_ms,
            retrying.halt.requested_at_ms
        );
        runtime.shutdown().await.unwrap();
        drop(engine);
        drop(pairings);
        fixture.assert_pairing_owner_released();
        venue.shutdown().await;
    }

    #[tokio::test]
    async fn halt_after_registry_regrant_persists_agent_stop_without_canceling_replacement() {
        let fixture = Fixture::authorized();
        let venue = LocalVenue::start().await;
        let mut prepared = fixture.prepare().unwrap();
        let ledger = prepared.ledger.clone();
        let engine = prepared.engine.clone();
        prepared.gateway = prepared.gateway.with_loopback_fixture(venue.port).unwrap();
        let runtime = crate::runtime::Runtime::new(fixture.dir.path().to_owned());
        let port = venue.port;
        runtime
            .launch_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
                move |_, _, status, stop| async move {
                    OwnedMcp::launch(
                        prepared,
                        0,
                        FixtureSource,
                        move || WsPool::loopback_fixture(port),
                        status,
                        stop,
                    )
                    .await
                },
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let registry = Arc::new(
            RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([11; 32]))).unwrap(),
        );
        let old = registry.route_for_agent(&fixture.binding.agent).unwrap();
        let at = u64::try_from(now_ms()).unwrap();
        assert!(registry.retire(&old, at).unwrap());
        let replacement_account = Address::from_bytes([8; 20]);
        let mut replacement = old.binding.clone();
        replacement.container = replacement_account;
        replacement.vault_address = None;
        replacement.wallet = AgentWallet {
            generation: 1,
            address: Address::from_bytes([10; 20]),
            approved_at_ms: at,
            valid_until_ms: at + 86_400_000,
        };
        let replacement = registry.grant(replacement, at).unwrap();
        assert_eq!(
            engine.route_for_agent(&fixture.binding.agent).unwrap(),
            replacement
        );

        runtime
            .halt_mcp(
                fixture.binding.agent.to_string(),
                fixture.binding.account.to_string(),
            )
            .unwrap();
        let halted = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let observed = runtime.mcp_status();
                assert_ne!(observed.halt.cancellation, CancellationPhase::Acknowledged);
                if observed.halt.cancellation == CancellationPhase::Retrying {
                    break observed.halt;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(halted.phase, HaltPhase::Persisted);
        assert!(halted.cancellation_error.is_some());
        assert!(engine.cancellation_needed(&fixture.binding.agent));
        let policy = PolicyJournal::new(registry.clone()).current().unwrap();
        let kill = serde_json::to_value(policy.state.kill).unwrap();
        assert_eq!(
            kill["agents"][fixture.binding.agent.as_str()]["reason"]["reason"],
            "operator"
        );
        assert_eq!(
            kill["agents"][fixture.binding.agent.as_str()]["engaged_at_ms"],
            halted.requested_at_ms.unwrap()
        );
        assert_eq!(
            registry
                .route_for_agent(&fixture.binding.agent)
                .unwrap()
                .binding
                .container,
            replacement_account
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
                .await
                .unwrap()
                .is_err()
        );
        assert_ne!(
            runtime.mcp_status().halt.cancellation,
            CancellationPhase::Acknowledged
        );
        assert!(
            venue
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.path != "/exchange")
        );
        ledger.verify().unwrap();
        drop(engine);
        drop(registry);
        drop(ledger);
        fixture.assert_pairing_owner_released();
        let reopened = Arc::new(
            Ledger::open_at(
                &fixture
                    .dir
                    .path()
                    .join(oppen_core::db_file_name(Network::Testnet)),
                Network::Testnet,
            )
            .unwrap(),
        );
        let registry = Arc::new(
            RegistryJournal::open(reopened, Arc::new(HmacKey::from_bytes([11; 32]))).unwrap(),
        );
        assert_eq!(
            registry.route_for_agent(&fixture.binding.agent).unwrap(),
            replacement
        );
        let persisted = PolicyJournal::new(registry).current().unwrap();
        assert_eq!(serde_json::to_value(persisted.state.kill).unwrap(), kill);
        venue.shutdown().await;
    }

    #[tokio::test]
    async fn halt_dropped_drain_waiter_retains_actual_mutation() {
        let fixture = Fixture::authorized();
        let prepared = fixture.prepare().unwrap();
        let bound = BoundServer::bind(0, &prepared.gateway, &prepared.pairings)
            .await
            .unwrap();
        let status = Arc::new(Mutex::new(McpStatus::starting(&fixture.binding)));
        let halt = HaltControl::new(
            prepared.engine.clone(),
            fixture.binding.clone(),
            prepared.pairings.clone(),
            bound.supervision_control(),
            status.clone(),
        );
        let mut path = fixture
            .dir
            .path()
            .join(oppen_core::db_file_name(Network::Testnet))
            .into_os_string();
        path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        lock.lock().unwrap();
        halt.request(&fixture.binding).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), halt.close_and_drain())
                .await
                .is_err()
        );
        // A second waiter must still await the same retained blocking worker.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), halt.close_and_drain())
                .await
                .is_err()
        );
        assert_eq!(status_lock(&status).halt.phase, HaltPhase::Persisting);
        assert!(halt.request(&fixture.binding).is_err());
        lock.unlock().unwrap();
        drop(bound);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), halt.close_and_drain())
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(status_lock(&status).halt.phase, HaltPhase::Persisted);
        assert_eq!(
            status_lock(&status).halt.cancellation,
            CancellationPhase::Unavailable
        );
        drop(halt);
        drop(prepared);
        fixture.assert_pairing_owner_released();
    }

    #[tokio::test]
    async fn halt_without_live_sweep_is_unavailable_even_with_startup_inhibition() {
        for fail_persistence in [false, true] {
            let fixture = Fixture::authorized();
            let prepared = fixture.prepare().unwrap();
            let bound = BoundServer::bind(0, &prepared.gateway, &prepared.pairings)
                .await
                .unwrap();
            let status = Arc::new(Mutex::new(McpStatus::starting(&fixture.binding)));
            let halt = HaltControl::new(
                prepared.engine.clone(),
                fixture.binding.clone(),
                prepared.pairings.clone(),
                bound.supervision_control(),
                status.clone(),
            );
            // This is an actual filesystem failure, not a substituted engine:
            // a directory cannot be opened as the ledger coordination file.
            let mut lock_path = fixture
                .dir
                .path()
                .join(oppen_core::db_file_name(Network::Testnet))
                .into_os_string();
            lock_path.push(".lock");
            let lock_path = PathBuf::from(lock_path);
            if fail_persistence {
                std::fs::remove_file(&lock_path).unwrap();
                std::fs::create_dir(&lock_path).unwrap();
            }
            halt.request(&fixture.binding).unwrap();
            drop(bound);
            assert!(
                tokio::time::timeout(Duration::from_secs(5), halt.close_and_drain())
                    .await
                    .unwrap()
                    .is_err()
            );
            let observed = status_lock(&status).halt.clone();
            assert_eq!(
                observed.phase,
                if fail_persistence {
                    HaltPhase::Uncertain
                } else {
                    HaltPhase::Persisted
                }
            );
            assert_eq!(observed.cancellation, CancellationPhase::Unavailable);
            assert!(observed.cancellation_error.is_some());
            assert_eq!(observed.error.is_some(), fail_persistence);
            let kill = serde_json::to_value(prepared.engine.kill_switch()).unwrap();
            assert_eq!(
                kill["agents"][fixture.binding.agent.as_str()]["reason"]["reason"],
                "operator"
            );
            if fail_persistence {
                std::fs::remove_dir(&lock_path).unwrap();
                prepared.engine.policy_observation().unwrap();
                let refreshed = serde_json::to_value(prepared.engine.kill_switch()).unwrap();
                assert_eq!(refreshed["agents"], kill["agents"]);
            }
        }
    }

    #[test]
    fn empty_pairings_cannot_claim_account_supervision() {
        let fixture = Fixture::new(true, None, false);
        assert!(matches!(fixture.prepare(), Err(detail) if detail.contains("retained pairings")));
    }

    #[tokio::test]
    async fn stop_waits_actual_pump_and_distinguishes_retryable_failure_from_panic() {
        for panic in [false, true] {
            let fixture = Fixture::authorized();
            let venue = LocalVenue::start().await;
            let mut prepared = fixture.prepare().unwrap();
            let feed = prepared.feed.clone();
            prepared.gateway = prepared.gateway.with_loopback_fixture(venue.port).unwrap();
            let (entered, observing) = oneshot::channel();
            let (release, waiting) = oneshot::channel();
            let source = BlockedSource {
                entered: Mutex::new(Some(entered)),
                release: tokio::sync::Mutex::new(Some(waiting)),
                panic,
            };
            let runtime = crate::runtime::Runtime::new(fixture.dir.path().to_owned());
            let port = venue.port;
            let reply = runtime
                .launch_mcp(
                    fixture.binding.agent.to_string(),
                    fixture.binding.account.to_string(),
                    move |_, _, status, stop| async move {
                        OwnedMcp::launch(
                            prepared,
                            0,
                            source,
                            move || WsPool::loopback_fixture(port),
                            status,
                            stop,
                        )
                        .await
                    },
                )
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), observing)
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), reply)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), runtime.shutdown())
                    .await
                    .is_err()
            );
            assert_eq!(
                runtime.status().phase,
                crate::runtime::RuntimePhase::Stopping
            );
            let _ = release.send(());
            let result = tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
                .await
                .unwrap();
            if panic {
                assert!(result.is_err());
                assert_eq!(runtime.mcp_status().phase, McpPhase::Failed);
                assert_eq!(
                    runtime.status().phase,
                    crate::runtime::RuntimePhase::StoppedWithError
                );
            } else {
                assert!(result.is_ok());
                assert_eq!(runtime.mcp_status().phase, McpPhase::Stopped);
                assert_eq!(
                    runtime.status().phase,
                    crate::runtime::RuntimePhase::Stopped
                );
                assert!(!feed.state().reconciled);
                assert!(feed.state().failure.is_none());
            }
            fixture.assert_pairing_owner_released();
            venue.shutdown().await;
        }
    }
}
