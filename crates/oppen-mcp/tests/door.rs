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
use oppen_core::guardrail::{AgentId, GuardrailEngine, SqliteGuardrailStore};
use oppen_core::journal::Journal;
use oppen_core::keys::KeychainKeyStore;
use oppen_core::ledger::{EventViews, Ledger, LedgerAuditSink};
use oppen_mcp::Network;
use oppen_mcp::auth::{Binding, TokenStore};
use oppen_mcp::server::{GatewayHandler, MCP_PATH, router, serve};
use oppen_mcp::tools::Gateway;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tower::ServiceExt;

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
fn fixture() -> (axum::Router, String) {
    let mut store = TokenStore::new();
    let issued = store.issue(binding("agent-alpha")).expect("entropy");
    let token = issued.reveal().to_owned();
    (router(gateway(), Arc::new(RwLock::new(store))), token)
}

/// A gateway whose engine is backed by a throwaway database.
///
/// No test reaches a Gateway tool, so nothing here touches the venue or signs
/// anything; the engine only has to exist.
fn gateway() -> Gateway {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = Arc::new(
        Ledger::open_at(&dir.path().join("testnet.db"), Network::Testnet).expect("ledger"),
    );
    let engine = Arc::new(
        GuardrailEngine::new(
            Arc::new(SqliteGuardrailStore::open(dir.path().join("guardrails.db")).expect("store")),
            Arc::new(LedgerAuditSink::new(ledger.clone())),
            Arc::new(KeychainKeyStore::new(Network::Testnet)),
            Network::Testnet,
        )
        .expect("engine"),
    );
    // The tempdir must outlive the gateway; leaking the handle is fine in a
    // test process that is about to exit.
    let journal = Arc::new(Journal::open(dir.path().join("journal.db")).expect("journal"));
    let feed = Arc::new(FeedSession::new());
    std::mem::forget(dir);
    Gateway::new(
        Network::Testnet,
        engine,
        EventViews::new(ledger),
        journal,
        feed,
        std::sync::Arc::new(oppen_core::alert::AlertStore::open(":memory:").expect("alerts")),
        std::sync::Arc::new(oppen_core::features::quotes::QuoteCache::new()),
    )
    .expect("gateway")
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
    let (router, _token) = fixture();
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, None)).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused() {
    let (router, _token) = fixture();
    let wrong = format!("Bearer {}", "0".repeat(64));
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, Some(&wrong))).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_rebinding_host_is_refused_even_with_a_valid_token() {
    let (router, token) = fixture();
    let auth = format!("Bearer {token}");
    assert_eq!(
        status(router, post("evil.example", None, Some(&auth))).await,
        StatusCode::FORBIDDEN,
        "a page that rebound its hostname to loopback got past the door"
    );
}

#[tokio::test]
async fn a_foreign_origin_is_refused_even_with_a_valid_token() {
    let (router, token) = fixture();
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
    let (router, token) = fixture();
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
    let mut store = TokenStore::new();
    let issued = store.issue(binding("agent-alpha")).expect("entropy");
    let token = issued.reveal().to_owned();
    store.revoke(issued.id);

    let router = router(gateway(), Arc::new(RwLock::new(store)));
    let auth = format!("Bearer {token}");
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, Some(&auth))).await,
        StatusCode::UNAUTHORIZED
    );
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

    let mut store = TokenStore::new();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let app = router(gateway(), pairings.clone());
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
    assert!(pairings.write().expect("pairings").revoke(issued.id));
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
    let mut store = TokenStore::new();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let auth = format!("Bearer {}", issued.reveal());
    let response = router(gateway(), pairings.clone())
        .oneshot(post("127.0.0.1:7433", None, Some(&auth)))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(pairings.write().expect("pairings").revoke(issued.id));
    let body = timeout(
        Duration::from_secs(1),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("revocation missed the response handoff")
    .expect("body");
    assert!(body.is_empty(), "revoked response leaked buffered data");
}

// Connection: close makes EOF observable on the wire even for an SSE response.
async fn send_http_request(
    addr: std::net::SocketAddr,
    request: Request<Body>,
) -> BufReader<TcpStream> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.expect("body");
    let mut socket = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{} {} HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        parts.method,
        parts.uri,
        body.len()
    );
    for (name, value) in &parts.headers {
        head.push_str(&format!("{name}: {}\r\n", value.to_str().expect("header")));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await.expect("headers");
    socket.write_all(&body).await.expect("body");
    BufReader::new(socket)
}

async fn http_request(
    addr: std::net::SocketAddr,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, BufReader<TcpStream>) {
    let mut reader = send_http_request(addr, request).await;
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("status line");
    let status = line.split_whitespace().nth(1).expect("status");
    let status = StatusCode::from_bytes(status.as_bytes()).expect("HTTP status");
    let mut headers = axum::http::HeaderMap::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.expect("header"), 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').expect("header field");
        headers.append(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.trim().parse().expect("header value"),
        );
    }
    (status, headers, reader)
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
    let mut store = TokenStore::new();
    let alpha = store.issue(binding("agent-alpha")).expect("alpha token");
    // Even another token for the same agent has an independent lifetime.
    let beta = store.issue(binding("agent-alpha")).expect("second token");
    let pairings = Arc::new(RwLock::new(store));
    let app = router(gateway(), pairings.clone());
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

        assert!(pairings.write().expect("pairings").revoke(alpha.id));
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
        assert!(pairings.write().expect("pairings").revoke(beta.id));
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
    let mut store = TokenStore::new();
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
        gateway(),
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
        // Models the account lock held across an exposure read before signing.
        let _execution = self.execution.lock().await;
        self.started.notify_one();
        self.resume.notified().await;
        self.continued
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(rmcp::model::CallToolResult::success(Vec::new()).into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_shutdown_drops_in_flight_execution_before_returning() {
    let mut store = TokenStore::new();
    let issued = store.issue(binding("agent-alpha")).expect("token");
    let pairings = Arc::new(RwLock::new(store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    let handler = InFlightHandler {
        gateway: gateway(),
        execution: Arc::new(tokio::sync::Mutex::new(())),
        started: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
        continued: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
