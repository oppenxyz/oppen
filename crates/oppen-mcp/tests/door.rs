//! The gateway door, exercised through the real router rather than through the
//! guard's pure functions (`docs/spec.md` item 14).
//!
//! These tests exist because the unit tests in `guard` prove the *predicates*
//! and prove nothing about whether the middleware is wired in front of the
//! transport. A guard that is correct and unreachable is the failure mode this
//! file is here to catch.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use oppen_core::feed::FeedSession;
use oppen_core::guardrail::{AgentId, GuardrailEngine};
use oppen_core::journal::Journal;
use oppen_core::keys::{EntryName, HmacKey, KeyStore, KeyStoreError, SecretText};
use oppen_core::ledger::{EventViews, Ledger, PairingJournal, PolicyJournal, RegistryJournal};
use oppen_mcp::Network;
use oppen_mcp::auth::{Binding, TokenStore};
use oppen_mcp::server::{GatewayHandler, MCP_PATH, router, serve};
use oppen_mcp::tools::Gateway;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tower::ServiceExt;

#[path = "../src/execution_fixture/http.rs"]
mod http_fixture;
use http_fixture::{http_request, send_http_request};

/// The funded testnet account these tests report on. No request in this file
/// reaches the venue: tool execution uses a local fake handler, so the address
/// only has to parse.
fn binding(agent: &str) -> Binding {
    Binding {
        agent: AgentId::new(agent),
        account: account(),
    }
}

fn account() -> oppen_hl::Address {
    "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2"
        .parse()
        .expect("address")
}

/// A router plus a token that authenticates against it.
fn fixture() -> (Fixture, axum::Router, String) {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let issued = store.issue(binding("agent-alpha")).expect("entropy");
    let token = issued.reveal().to_owned();
    let app = router(fixture.gateway.clone(), Arc::new(RwLock::new(store)));
    (fixture, app, token)
}

struct NoKeys(Network);

impl KeyStore for NoKeys {
    fn network(&self) -> Network {
        self.0
    }
    fn read(&self, _: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
        panic!("door tests must not read credentials")
    }
    fn write(&self, _: &EntryName, _: &str) -> Result<(), KeyStoreError> {
        panic!("door tests must not write credentials")
    }
    fn remove(&self, _: &EntryName) -> Result<(), KeyStoreError> {
        panic!("door tests must not remove credentials")
    }
}

struct Fixture {
    gateway: Gateway,
    ledger: Arc<Ledger>,
    hmac: Arc<HmacKey>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(network: Network) -> Self {
        Self::with_anchor(network, None)
    }

    fn with_anchor(
        network: Network,
        anchor: Option<Box<dyn oppen_core::ledger::HeadAnchor>>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.db");
        let ledger = Arc::new(
            match anchor {
                Some(anchor) => Ledger::open_anchored(&path, network, Some(anchor)),
                None => Ledger::open_at(&path, network),
            }
            .expect("ledger"),
        );
        let engine = Arc::new(
            GuardrailEngine::new(
                Arc::new(PolicyJournal::new(Arc::new(
                    RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([42; 32])))
                        .expect("registry"),
                ))),
                Arc::new(NoKeys(network)),
            )
            .expect("engine"),
        );
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).expect("journal"));
        let feed = Arc::new(FeedSession::new());
        let gateway = Gateway::new(
            network,
            engine,
            EventViews::new(ledger.clone()),
            journal,
            feed,
            std::sync::Arc::new(oppen_core::alert::AlertStore::open(":memory:").expect("alerts")),
            std::sync::Arc::new(oppen_core::features::quotes::QuoteCache::new()),
        )
        .expect("gateway");
        Self {
            gateway,
            ledger,
            hmac: Arc::new(HmacKey::from_bytes([42; 32])),
            _dir: dir,
        }
    }

    fn store(&self) -> TokenStore {
        TokenStore::open(
            PairingJournal::open(self.ledger.clone(), self.hmac.clone()).expect("journal owner"),
        )
        .expect("pairing replay")
    }
}

/// A syntactically valid MCP initialize call, so that anything reaching the
/// transport gets a real answer rather than a parse error.
fn initialize_body() -> Body {
    Body::from(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "door-test", "version": "0" }
            }
        })
        .to_string(),
    )
}

fn post(host: &str, origin: Option<&str>, auth: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(MCP_PATH)
        .header("host", host)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    if let Some(auth) = auth {
        builder = builder.header("authorization", auth);
    }
    builder.body(initialize_body()).expect("request")
}

async fn status(router: axum::Router, request: Request<Body>) -> StatusCode {
    router.oneshot(request).await.expect("response").status()
}

#[tokio::test]
async fn a_request_with_no_authorization_is_refused() {
    let (_fixture, router, _token) = fixture();
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, None)).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused() {
    let (_fixture, router, _token) = fixture();
    let wrong = format!("Bearer {}", "0".repeat(64));
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, Some(&wrong))).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_rebinding_host_is_refused_even_with_a_valid_token() {
    let (_fixture, router, token) = fixture();
    let auth = format!("Bearer {token}");
    assert_eq!(
        status(router, post("evil.example", None, Some(&auth))).await,
        StatusCode::FORBIDDEN,
        "a page that rebound its hostname to loopback got past the door"
    );
}

#[tokio::test]
async fn a_foreign_origin_is_refused_even_with_a_valid_token() {
    let (_fixture, router, token) = fixture();
    let auth = format!("Bearer {token}");
    assert_eq!(
        status(
            router,
            post("127.0.0.1:7433", Some("http://evil.example"), Some(&auth))
        )
        .await,
        StatusCode::FORBIDDEN,
        "a browser page on another origin got past the door"
    );
}

#[tokio::test]
async fn a_paired_agent_reaches_the_transport() {
    let (_fixture, router, token) = fixture();
    let auth = format!("Bearer {token}");
    let status = status(router, post("127.0.0.1:7433", None, Some(&auth))).await;
    assert!(
        status.is_success(),
        "a paired agent was refused at the door with {status}; the guard is too strict or the \
         transport is not mounted"
    );
}

#[tokio::test]
async fn a_revoked_pairing_is_refused_at_the_door() {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let issued = store.issue(binding("agent-alpha")).expect("entropy");
    let token = issued.reveal().to_owned();
    store.revoke(issued.id).expect("durable revocation");

    let router = router(fixture.gateway.clone(), Arc::new(RwLock::new(store)));
    let auth = format!("Bearer {token}");
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, Some(&auth))).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn wrong_network_fails_closed_in_router_and_before_binding() {
    let gateway = Fixture::new(Network::Testnet);
    let authority = Fixture::new(Network::Mainnet);
    let mut store = authority.store();
    let token = store.issue(binding("agent-alpha")).unwrap();
    let pairings = Arc::new(RwLock::new(store));
    assert_eq!(
        status(
            router(gateway.gateway.clone(), pairings.clone()),
            post(
                "127.0.0.1:7433",
                None,
                Some(&format!("Bearer {}", token.reveal()))
            )
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    // An occupied port distinguishes the authority check from a later bind failure.
    let error = serve(
        listener.local_addr().unwrap().port(),
        gateway.gateway.clone(),
        pairings,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[derive(Debug, Default)]
struct MutationAnchor {
    head: std::sync::Mutex<Option<oppen_core::ledger::Anchor>>,
    wait: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    entered: tokio::sync::Notify,
}

#[derive(Debug)]
struct MutationAnchorHandle(Arc<MutationAnchor>);

impl oppen_core::ledger::HeadAnchor for MutationAnchorHandle {
    fn load(&self) -> Result<Option<oppen_core::ledger::Anchor>, oppen_core::ledger::LedgerError> {
        Ok(self.0.head.lock().unwrap().clone())
    }

    fn store(
        &self,
        anchor: &oppen_core::ledger::Anchor,
    ) -> Result<(), oppen_core::ledger::LedgerError> {
        if let Some(wait) = self.0.wait.lock().unwrap().take() {
            self.0.entered.notify_one();
            // A disconnected sender also releases the writer if the test fails.
            let _ = wait.recv_timeout(Duration::from_secs(15));
        }
        *self.0.head.lock().unwrap() = Some(anchor.clone());
        Ok(())
    }
}

#[tokio::test]
async fn durable_writer_contention_refuses_http_and_startup_without_holding_shutdown() {
    let anchor = Arc::new(MutationAnchor::default());
    let fixture = Fixture::with_anchor(
        Network::Testnet,
        Some(Box::new(MutationAnchorHandle(anchor.clone()))),
    );
    // Isolate the transport's pairing lock from tracked policy work. A sweep
    // already holding a binding snapshot may legitimately wait on a shared
    // ledger and must drain before shutdown; the blocked-supervisor policy
    // refresh regression in execution_fixture::decision covers that case.
    let gateway_fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let token = store.issue(binding("agent-alpha")).unwrap();
    let pairings = Arc::new(RwLock::new(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(serve(
        addr.port(),
        gateway_fixture.gateway.clone(),
        pairings.clone(),
        shutdown.clone(),
    ));
    timeout(Duration::from_secs(2), async {
        while TcpStream::connect(addr).await.is_err() {
            assert!(!server.is_finished());
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.wait.lock().unwrap() = Some(wait);
    let writer_pairings = pairings.clone();
    let id = token.id;
    let (finished, mut outcome) = tokio::sync::oneshot::channel();
    let writer = std::thread::spawn(move || {
        let result = writer_pairings.write().unwrap().revoke(id);
        finished.send(result).unwrap();
    });
    timeout(Duration::from_secs(2), anchor.entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        pairings.try_read(),
        Err(std::sync::TryLockError::WouldBlock)
    ));

    let error = timeout(
        Duration::from_millis(500),
        serve(
            0,
            gateway_fixture.gateway.clone(),
            pairings.clone(),
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await
    .expect("startup waited on durable mutation")
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    let request = post(
        &addr.to_string(),
        None,
        Some(&format!("Bearer {}", token.reveal())),
    );
    let (status, _, _) = timeout(Duration::from_millis(500), http_request(addr, request))
        .await
        .expect("HTTP waited on durable mutation");
    assert_eq!(status, 401);

    // Cross a complete sweep interval while the actual revocation is blocked.
    // The sweep must skip, not park this single async worker on std::RwLock.
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "sweep blocked the async worker"
    );
    assert!(matches!(
        outcome.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    shutdown.cancel();
    timeout(Duration::from_millis(500), server)
        .await
        .expect("shutdown waited for the pairing writer")
        .unwrap()
        .unwrap();
    assert!(matches!(
        outcome.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        PairingJournal::open(fixture.ledger.clone(), fixture.hmac.clone()),
        Err(oppen_core::ledger::PairingError::AlreadyOwned)
    ));

    release.send(()).unwrap();
    assert!(
        timeout(Duration::from_secs(2), outcome)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    writer.join().unwrap();
    assert!(
        pairings
            .read()
            .unwrap()
            .authenticate(token.reveal())
            .is_err()
    );
    drop(pairings);
    let reopened = fixture.store();
    assert!(matches!(
        reopened.authenticate(token.reveal()),
        Err(oppen_mcp::auth::AuthError::Revoked)
    ));
}

#[tokio::test]
async fn revocation_interrupts_a_request_waiting_for_its_body() {
    struct PendingBody(Option<tokio::sync::oneshot::Sender<()>>);

    impl tokio::io::AsyncRead for PendingBody {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(started) = self.0.take() {
                started.send(()).expect("request observer");
            }
            std::task::Poll::Pending
        }
    }

    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let app = router(fixture.gateway.clone(), pairings.clone());
    let auth = format!("Bearer {}", issued.reveal());
    let mut request = post("127.0.0.1:7433", None, Some(&auth));
    let (started, reading) = tokio::sync::oneshot::channel();
    *request.body_mut() = Body::from_stream(tokio_util::io::ReaderStream::new(PendingBody(Some(
        started,
    ))));
    let response = tokio::spawn(app.oneshot(request));
    timeout(Duration::from_secs(1), reading)
        .await
        .expect("transport never read the authenticated request")
        .expect("request started");
    assert!(
        pairings
            .write()
            .expect("pairings")
            .revoke(issued.id)
            .unwrap()
    );
    assert_eq!(
        timeout(Duration::from_secs(1), response)
            .await
            .expect("revocation did not interrupt the pending request")
            .expect("request task")
            .expect("response")
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn revocation_before_the_first_response_body_poll_discards_the_response() {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let auth = format!("Bearer {}", issued.reveal());
    let response = router(fixture.gateway.clone(), pairings.clone())
        .oneshot(post("127.0.0.1:7433", None, Some(&auth)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        pairings
            .write()
            .expect("pairings")
            .revoke(issued.id)
            .unwrap()
    );
    let body = timeout(
        Duration::from_secs(1),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("revocation missed the response handoff")
    .expect("body");
    assert!(body.is_empty(), "revoked response leaked buffered data");
}

async fn initialize_http_session(
    addr: std::net::SocketAddr,
    token: &str,
) -> axum::http::HeaderValue {
    let auth = format!("Bearer {token}");
    let (status, headers, mut response) =
        http_request(addr, post(&addr.to_string(), None, Some(&auth))).await;
    assert_eq!(status, StatusCode::OK);
    let session = headers.get("mcp-session-id").expect("MCP session");
    let mut body = String::new();
    response
        .read_to_string(&mut body)
        .await
        .expect("initialize");
    assert!(body.contains("protocolVersion"), "{body}");

    let request = Request::builder()
        .method("POST")
        .uri(MCP_PATH)
        .header("host", addr.to_string())
        .header("authorization", &auth)
        .header("mcp-session-id", session)
        .header("mcp-protocol-version", "2025-06-18")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ))
        .expect("initialized request");
    assert_eq!(http_request(addr, request).await.0, StatusCode::ACCEPTED);
    session.clone()
}

async fn open_http_stream(addr: std::net::SocketAddr, token: &str) -> BufReader<TcpStream> {
    let session = initialize_http_session(addr, token).await;
    let request = Request::builder()
        .uri(MCP_PATH)
        .header("host", addr.to_string())
        .header("authorization", format!("Bearer {token}"))
        .header("mcp-session-id", session)
        .header("mcp-protocol-version", "2025-06-18")
        .header("accept", "text/event-stream")
        .body(Body::empty())
        .expect("SSE request");
    let (status, headers, response) = http_request(addr, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    response
}

#[tokio::test]
async fn revocation_closes_an_open_http_stream_without_closing_another_pairing() {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let alpha = store.issue(binding("agent-alpha")).expect("alpha token");
    // Even another token for the same agent has an independent lifetime.
    let beta = store.issue(binding("agent-alpha")).expect("second token");
    let pairings = Arc::new(RwLock::new(store));
    let app = router(fixture.gateway.clone(), pairings.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let addr = listener.local_addr().expect("address");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    timeout(Duration::from_secs(5), async {
        let mut alpha_stream = open_http_stream(addr, alpha.reveal()).await;
        let mut beta_stream = open_http_stream(addr, beta.reveal()).await;
        let mut alpha_body = Vec::new();
        let mut beta_body = Vec::new();
        assert!(
            timeout(
                Duration::from_millis(50),
                alpha_stream.read_to_end(&mut alpha_body)
            )
            .await
            .is_err()
        );
        assert!(
            timeout(
                Duration::from_millis(50),
                beta_stream.read_to_end(&mut beta_body)
            )
            .await
            .is_err()
        );

        assert!(
            pairings
                .write()
                .expect("pairings")
                .revoke(alpha.id)
                .unwrap()
        );
        timeout(
            Duration::from_secs(1),
            alpha_stream.read_to_end(&mut alpha_body),
        )
        .await
        .expect("revocation did not close the live HTTP stream")
        .expect("stream EOF");
        assert!(
            timeout(
                Duration::from_millis(50),
                beta_stream.read_to_end(&mut beta_body)
            )
            .await
            .is_err()
        );

        for (token, expected) in [
            (alpha.reveal(), StatusCode::UNAUTHORIZED),
            (beta.reveal(), StatusCode::OK),
        ] {
            let auth = format!("Bearer {token}");
            assert_eq!(
                http_request(addr, post(&addr.to_string(), None, Some(&auth)))
                    .await
                    .0,
                expected
            );
        }
        assert!(pairings.write().expect("pairings").revoke(beta.id).unwrap());
        timeout(
            Duration::from_secs(1),
            beta_stream.read_to_end(&mut beta_body),
        )
        .await
        .expect("second revocation did not close its HTTP stream")
        .expect("stream EOF");
    })
    .await
    .expect("HTTP regression timed out");
    server.abort();
    assert!(server.await.expect_err("server aborted").is_cancelled());
}

#[tokio::test]
async fn serve_shutdown_does_not_wait_for_an_open_http_stream() {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(serve(
        addr.port(),
        fixture.gateway.clone(),
        pairings.clone(),
        shutdown.clone(),
    ));
    timeout(Duration::from_secs(5), async {
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            assert!(!server.is_finished(), "server failed to start");
            tokio::task::yield_now().await;
        }
        let mut partial = TcpStream::connect(addr)
            .await
            .expect("partial request connection");
        partial
            .write_all(b"POST /mcp HTTP/1.1\r\nHost:")
            .await
            .expect("partial headers");
        let mut stream = open_http_stream(addr, issued.reveal()).await;
        let mut body = Vec::new();
        assert!(
            timeout(Duration::from_millis(50), stream.read_to_end(&mut body))
                .await
                .is_err()
        );
        shutdown.cancel();
        timeout(Duration::from_secs(1), server)
            .await
            .expect("shutdown waited for the open HTTP stream")
            .expect("server task")
            .expect("server shutdown");
        timeout(Duration::from_secs(1), stream.read_to_end(&mut body))
            .await
            .expect("stream cleanup")
            .expect("stream EOF");
        timeout(Duration::from_secs(1), partial.read_to_end(&mut Vec::new()))
            .await
            .expect("partial request survived shutdown")
            .expect("partial request EOF");
    })
    .await
    .expect("serve lifecycle timed out");
}

#[derive(Clone)]
struct InFlightHandler {
    gateway: Gateway,
    execution: Arc<tokio::sync::Mutex<()>>,
    started: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
    continued: Arc<std::sync::atomic::AtomicBool>,
    drain: Option<Arc<DrainBarrier>>,
}

struct DrainBarrier {
    entered: tokio::sync::Notify,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

struct DrainOnDrop(Option<Arc<DrainBarrier>>);

impl Drop for DrainOnDrop {
    fn drop(&mut self) {
        if let Some(barrier) = &self.0 {
            barrier.entered.notify_one();
            barrier
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(15))
                .expect("release method drain");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnected_revoked_method_retains_owner_until_actual_drain() {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let token = store.issue(binding("agent-alpha")).unwrap();
    let pairings = Arc::new(RwLock::new(store));
    let registry = Arc::downgrade(&pairings);
    let (release, wait) = std::sync::mpsc::channel();
    let drain = Arc::new(DrainBarrier {
        entered: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(wait),
    });
    let handler = InFlightHandler {
        gateway: fixture.gateway.clone(),
        execution: Arc::new(tokio::sync::Mutex::new(())),
        started: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
        continued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        drain: Some(drain.clone()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let server = tokio::spawn(serve(
        addr.port(),
        handler.clone(),
        pairings.clone(),
        tokio_util::sync::CancellationToken::new(),
    ));
    timeout(Duration::from_secs(10), async {
        while TcpStream::connect(addr).await.is_err() {
            assert!(!server.is_finished());
            tokio::task::yield_now().await;
        }
        let session = initialize_http_session(addr, token.reveal()).await;
        let request = Request::builder().method("POST").uri(MCP_PATH)
            .header("host", addr.to_string())
            .header("authorization", format!("Bearer {}", token.reveal()))
            .header("mcp-session-id", session)
            .header("mcp-protocol-version", "2025-06-18")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"place_fixture","arguments":{}}}"#)).unwrap();
        let response = send_http_request(addr, request).await;
        handler.started.notified().await;
        drop(response);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handler.execution.try_lock().is_err(), "HTTP disconnect unexpectedly ended the method");
        assert!(pairings.write().unwrap().revoke(token.id).unwrap());
        drain.entered.notified().await;
        assert!(handler.execution.try_lock().is_err(), "method already drained");

        // Remove the registry and transport owners. Only method/session authority
        // can now prevent a replacement cache from opening the journal.
        drop(pairings);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        while registry.strong_count() != 0 { tokio::task::yield_now().await; }
        assert!(matches!(PairingJournal::open(fixture.ledger.clone(), fixture.hmac.clone()), Err(oppen_core::ledger::PairingError::AlreadyOwned)));
        release.send(()).unwrap();
        let journal = loop {
            match PairingJournal::open(fixture.ledger.clone(), fixture.hmac.clone()) {
                Ok(journal) => break journal,
                Err(oppen_core::ledger::PairingError::AlreadyOwned) => tokio::task::yield_now().await,
                Err(error) => panic!("owner handoff failed: {error}"),
            }
        };
        assert!(handler.execution.try_lock().is_ok());
        handler.resume.notify_one();
        assert!(!handler.continued.load(std::sync::atomic::Ordering::SeqCst));
        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].binding, binding("agent-alpha"));
        assert!(records[0].revoked_at_ms.is_some());
        let reopened = TokenStore::open(journal).unwrap();
        assert!(matches!(reopened.authenticate(token.reveal()), Err(oppen_mcp::auth::AuthError::Revoked)));
    }).await.expect("actual method ownership did not drain");
}

impl GatewayHandler for InFlightHandler {
    fn gateway(&self) -> &Gateway {
        &self.gateway
    }
}

impl rmcp::ServerHandler for InFlightHandler {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        self.gateway.get_info()
    }

    async fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let bound = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Binding>())
            .expect("real authenticated request binding");
        assert_eq!(bound.agent, AgentId::new("agent-alpha"));
        // Real tools may extract a binding then discard their HTTP context.
        // The outer actual-method guard must independently retain authority.
        drop(context);
        // Models the account lock held across an exposure read before signing.
        let _execution = self.execution.lock().await;
        let _drain = DrainOnDrop(self.drain.clone());
        self.started.notify_one();
        self.resume.notified().await;
        self.continued
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(rmcp::model::CallToolResult::success(Vec::new()).into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_shutdown_drops_in_flight_execution_before_returning() {
    let fixture = Fixture::new(Network::Testnet);
    let mut store = fixture.store();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let handler = InFlightHandler {
        gateway: fixture.gateway.clone(),
        execution: Arc::new(tokio::sync::Mutex::new(())),
        started: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
        continued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        drain: None,
    };
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(serve(
        addr.port(),
        handler.clone(),
        pairings,
        shutdown.clone(),
    ));
    timeout(Duration::from_secs(5), async {
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            assert!(!server.is_finished(), "server failed to start");
            tokio::task::yield_now().await;
        }
        let session = initialize_http_session(addr, issued.reveal()).await;
        let request = Request::builder()
            .method("POST")
            .uri(MCP_PATH)
            .header("host", addr.to_string())
            .header("authorization", format!("Bearer {}", issued.reveal()))
            .header("mcp-session-id", session)
            .header("mcp-protocol-version", "2025-06-18")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"place_fixture","arguments":{}}}"#,
            ))
            .expect("tool request");
        let mut response = send_http_request(addr, request).await;
        handler.started.notified().await;
        assert!(handler.execution.try_lock().is_err());
        shutdown.cancel();
        timeout(Duration::from_secs(1), server)
            .await
            .expect("shutdown waited for blocked execution")
            .expect("server task")
            .expect("server shutdown");

        assert!(
            handler.execution.try_lock().is_ok(),
            "serve returned with execution still alive"
        );
        handler.resume.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handler.continued.load(std::sync::atomic::Ordering::SeqCst));
        let mut body = Vec::new();
        timeout(Duration::from_secs(1), response.read_to_end(&mut body))
            .await
            .expect("execution connection survived shutdown")
            .expect("connection EOF");
        assert!(TcpStream::connect(addr).await.is_err());
    })
    .await
    .expect("execution lifecycle timed out");
}
