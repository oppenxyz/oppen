//! A decision replay stalled after venue IO remains owned until it drains.

use super::*;
use oppen_core::ledger::{Anchor, HeadAnchor, LedgerError, PairingError, PairingJournal};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
struct HeldAnchor {
    head: Mutex<Option<Anchor>>,
    wait: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    entered: tokio::sync::Notify,
}

#[derive(Debug)]
struct AnchorHandle(Arc<HeldAnchor>);

impl HeadAnchor for AnchorHandle {
    fn load(&self) -> Result<Option<Anchor>, LedgerError> {
        if let Some(wait) = self.0.wait.lock().unwrap().take() {
            self.0.entered.notify_one();
            let _ = wait.recv_timeout(Duration::from_secs(15));
        }
        Ok(self.0.head.lock().unwrap().clone())
    }

    fn store(&self, head: &Anchor) -> Result<(), LedgerError> {
        *self.0.head.lock().unwrap() = Some(head.clone());
        Ok(())
    }
}

fn request(
    addr: std::net::SocketAddr,
    token: &str,
    session: Option<&str>,
    value: Value,
) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(crate::server::MCP_PATH)
        .header("host", addr.to_string())
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request.body(Body::from(value.to_string())).unwrap()
}

#[tokio::test]
async fn timed_out_policy_refresh_reports_retry_without_releasing_singleflight() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let anchor = Arc::new(HeldAnchor::default());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        keys.clone(),
        Some(Box::new(AnchorHandle(anchor.clone()))),
    )
    .await;
    runtime.activate_orders().await;
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.wait.lock().unwrap() = Some(wait);
    let gateway = runtime.gateway.clone();
    let tracker = runtime.tracker();
    let sweep = tokio::spawn(async move { gateway.enforce_pauses(&[], tracker).await });
    timeout(Duration::from_secs(2), anchor.entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(6), sweep)
            .await
            .unwrap()
            .unwrap(),
        Err(ToolError::Unavailable {
            what: "decision timeout",
            ..
        })
    ));
    assert_eq!(runtime.gateway.inner.decision_worker.available_permits(), 0);
    assert!(matches!(
        runtime.gateway.enforce_pauses(&[], runtime.tracker()).await,
        Err(ToolError::Unavailable {
            what: "decision worker busy",
            ..
        })
    ));
    release.send(()).unwrap();
    timeout(Duration::from_secs(2), async {
        while runtime.gateway.inner.decision_worker.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(venue.submissions().is_empty());
    assert!(keys.read_heads.lock().unwrap().is_empty());
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn blocked_supervisor_policy_refresh_retains_owner_until_shutdown_drain() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let anchor = Arc::new(HeldAnchor::default());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        keys.clone(),
        Some(Box::new(AnchorHandle(anchor.clone()))),
    )
    .await;
    runtime.activate_orders().await;
    assert!(runtime.request("DELETE", None).await.status().is_success());
    let gateway = runtime.gateway.clone();
    let ledger = runtime.ledger.clone();
    let pairings = runtime.pairings.clone();
    drop(runtime);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.wait.lock().unwrap() = Some(wait);
    let shutdown = CancellationToken::new();
    let mut server = tokio::spawn(crate::server::serve(
        addr.port(),
        gateway.clone(),
        pairings,
        shutdown.clone(),
    ));
    timeout(Duration::from_secs(2), anchor.entered.notified())
        .await
        .expect("supervisor did not replay policy");
    assert_eq!(gateway.inner.decision_worker.available_permits(), 0);
    // The five-second refresh deadline must not release the worker or owner.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(gateway.inner.decision_worker.available_permits(), 0);
    assert!(!server.is_finished());
    let heartbeat = Request::builder()
        .uri(crate::server::MCP_PATH)
        .header("host", addr.to_string())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        timeout(
            Duration::from_millis(500),
            transport::http_request(addr, heartbeat)
        )
        .await
        .expect("refresh blocked async worker")
        .0,
        401
    );
    shutdown.cancel();
    assert!(
        timeout(Duration::from_millis(100), &mut server)
            .await
            .is_err(),
        "serve returned before refresh drained"
    );
    let hmac = Arc::new(oppen_core::keys::HmacKey::from_bytes([77; 32]));
    assert!(matches!(
        PairingJournal::open(ledger.clone(), hmac.clone()),
        Err(PairingError::AlreadyOwned)
    ));
    release.send(()).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .expect("refresh did not drain")
        .unwrap()
        .unwrap();
    assert_eq!(gateway.inner.decision_worker.available_permits(), 1);
    assert!(PairingJournal::open(ledger, hmac).is_ok());
    assert!(venue.submissions().is_empty());
    assert!(keys.read_heads.lock().unwrap().is_empty());
    venue.shutdown().await;
}

async fn initialize(addr: std::net::SocketAddr, token: &str) -> String {
    let (status, headers, mut body) = transport::http_request(addr, request(addr, token, None, json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize",
        "params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"decision-fixture","version":"0"}}
    }))).await;
    assert_eq!(status, 200);
    let session = headers["mcp-session-id"].to_str().unwrap().to_owned();
    body.read_to_end(&mut Vec::new()).await.unwrap();
    assert_eq!(
        transport::http_request(
            addr,
            request(
                addr,
                token,
                Some(&session),
                json!({
                    "jsonrpc":"2.0", "method":"notifications/initialized"
                })
            )
        )
        .await
        .0,
        202
    );
    session
}

#[tokio::test]
async fn post_lookup_decisions_keep_runtime_ownership_until_drain_without_background_signing() {
    for tool in ["preflight", "place", "cancel", "supervision"] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let anchor = Arc::new(HeldAnchor::default());
        let runtime = Runtime::open_with_anchor(
            dir.path(),
            venue.port(),
            keys.clone(),
            Some(Box::new(AnchorHandle(anchor.clone()))),
        )
        .await;
        runtime.activate_orders().await;
        let bound = Binding {
            agent: AgentId::new("fixture-agent"),
            account: runtime.account,
        };
        if matches!(tool, "cancel" | "supervision") {
            assert_eq!(
                runtime
                    .call("place", place(Cloid::from_bytes([93; 16]).as_str(), "0.12"))
                    .await["status"],
                "resting"
            );
        }
        if tool == "supervision" {
            runtime
                .gateway
                .inner
                .engine
                .operator_engage_kill(
                    oppen_core::guardrail::KillScope::Agent {
                        agent: bound.agent.clone(),
                    },
                    oppen_core::guardrail::KillReason::Operator,
                    now_ms(),
                )
                .unwrap();
        }
        let gate = venue.hold_info(if matches!(tool, "cancel" | "supervision") {
            "frontendOpenOrders"
        } else {
            "portfolio"
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let shutdown = CancellationToken::new();
        let gateway = runtime.gateway.clone();
        let ledger = runtime.ledger.clone();
        let pairings = Arc::downgrade(&runtime.pairings);
        let token = runtime.token.clone();
        let mut server = tokio::spawn(crate::server::serve(
            addr.port(),
            gateway.clone(),
            runtime.pairings.clone(),
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
        let mut response = if tool == "supervision" {
            None
        } else {
            let session = initialize(addr, &token).await;
            let arguments = match tool {
                "preflight" => {
                    json!({"symbol":"TEST", "is_buy":true, "size":"0.12", "limit_px":"100"})
                }
                "place" => place(Cloid::from_bytes([94; 16]).as_str(), "0.12"),
                "cancel" => json!({"oid": 1, "reason":"stalled decision"}),
                _ => unreachable!(),
            };
            Some(transport::send_http_request(addr, request(addr, &token, Some(&session), json!({
                "jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":tool, "arguments":arguments}
            }))).await)
        };
        timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("preliminary route check must reach venue");
        // Close the fixture's other MCP session, so it cannot retain the owner.
        assert!(runtime.request("DELETE", None).await.status().is_success());
        drop(runtime);
        let submissions = venue.submissions().len();
        let key_reads = keys.read_heads.lock().unwrap().len();
        let (release, wait) = std::sync::mpsc::channel();
        *anchor.wait.lock().unwrap() = Some(wait);
        let held = ledger.clone();
        let holder = tokio::task::spawn_blocking(move || held.verify().unwrap());
        timeout(Duration::from_secs(2), anchor.entered.notified())
            .await
            .unwrap();
        gate.release.notify_one();
        timeout(Duration::from_secs(2), async {
            while gateway.inner.decision_worker.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("decision did not start after venue response");
        let heartbeat = Request::builder()
            .uri(crate::server::MCP_PATH)
            .header("host", addr.to_string())
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            timeout(
                Duration::from_millis(500),
                transport::http_request(addr, heartbeat)
            )
            .await
            .expect("async worker blocked")
            .0,
            401
        );
        shutdown.cancel();
        if let Some(response) = &mut response {
            timeout(
                Duration::from_millis(500),
                response.read_to_end(&mut Vec::new()),
            )
            .await
            .expect("HTTP cancellation blocked")
            .unwrap();
        }
        assert!(
            timeout(Duration::from_millis(100), &mut server)
                .await
                .is_err(),
            "serve returned before decision drained"
        );
        if tool != "supervision" {
            timeout(Duration::from_secs(1), async {
                while pairings.strong_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("transport registry still alive");
        }
        let hmac = Arc::new(oppen_core::keys::HmacKey::from_bytes([77; 32]));
        assert!(matches!(
            PairingJournal::open(ledger.clone(), hmac.clone()),
            Err(PairingError::AlreadyOwned)
        ));
        assert_eq!(venue.submissions().len(), submissions);
        release.send(()).unwrap();
        holder.await.unwrap();
        timeout(Duration::from_secs(2), server)
            .await
            .expect("server failed to drain")
            .unwrap()
            .unwrap();
        assert_eq!(gateway.inner.decision_worker.available_permits(), 1);
        assert_eq!(
            venue.submissions().len(),
            submissions,
            "canceled {tool} resumed submission"
        );
        assert_eq!(
            keys.read_heads.lock().unwrap().len(),
            key_reads,
            "canceled {tool} read signing keys"
        );
        let journal = timeout(Duration::from_secs(2), async {
            loop {
                match PairingJournal::open(ledger.clone(), hmac.clone()) {
                    Ok(journal) => break journal,
                    Err(PairingError::AlreadyOwned) => tokio::task::yield_now().await,
                    Err(error) => panic!("owner recovery failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(
            crate::auth::TokenStore::open(journal)
                .unwrap()
                .authenticate(&token)
                .is_ok()
        );
        venue.shutdown().await;
    }
}
