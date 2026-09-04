//! The gateway door, exercised through the real router rather than through the
//! guard's pure functions (`docs/spec.md` item 14).
//!
//! These tests exist because the unit tests in `guard` prove the *predicates*
//! and prove nothing about whether the middleware is wired in front of the
//! transport. A guard that is correct and unreachable is the failure mode this
//! file is here to catch.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use oppen_mcp::Network;
use oppen_mcp::auth::TokenStore;
use oppen_mcp::server::{MCP_PATH, router};
use oppen_mcp::tools::Gateway;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

/// A router plus a token that authenticates against it.
fn fixture() -> (axum::Router, String) {
    let mut store = TokenStore::new();
    let issued = store.issue().expect("entropy");
    let token = issued.reveal().to_owned();
    let gateway = Gateway::new(Network::Testnet).expect("gateway");
    (router(gateway, Arc::new(RwLock::new(store))), token)
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
    let issued = store.issue().expect("entropy");
    let token = issued.reveal().to_owned();
    store.revoke(issued.id);

    let gateway = Gateway::new(Network::Testnet).expect("gateway");
    let router = router(gateway, Arc::new(RwLock::new(store)));
    let auth = format!("Bearer {token}");
    assert_eq!(
        status(router, post("127.0.0.1:7433", None, Some(&auth))).await,
        StatusCode::UNAUTHORIZED
    );
}
