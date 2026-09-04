//! The loopback HTTP server (`docs/spec.md` item 14, `docs/decisions.md` D2).
//!
//! `rmcp` owns the MCP protocol itself — JSON-RPC framing, version
//! negotiation, sessions, SSE. This module owns the two things `rmcp` cannot
//! decide for oppen: **where it listens**, and **who gets past the door**.
//!
//! The guard runs as a middleware in front of the transport rather than inside
//! a tool, so an unauthenticated request never reaches the protocol layer, let
//! alone a tool.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};

use crate::auth::TokenStore;
use crate::guard::{Refusal, bearer_token, check_host, check_origin};
use crate::tools::Gateway;

/// The path agents connect to. `claude mcp add --transport http oppen
/// http://127.0.0.1:<port>/mcp`.
pub const MCP_PATH: &str = "/mcp";

/// Shared pairing registry. `RwLock` because authentication is a read on every
/// request and pairing or revoking is a rare write.
pub type Pairings = Arc<RwLock<TokenStore>>;

/// Build the router: the guard, then `rmcp`'s transport.
pub fn router(gateway: Gateway, pairings: Pairings) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(gateway.clone()),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    axum::Router::new()
        .nest_service(MCP_PATH, service)
        .layer(axum::middleware::from_fn_with_state(
            pairings,
            guard_middleware,
        ))
}

/// Bind loopback and serve.
///
/// Binds `127.0.0.1` explicitly and never `0.0.0.0`: the guard below defends
/// against a browser page on this machine, and nothing defends against a
/// listener reachable from the network, so that address is not offered as an
/// option (`AGENTS.md` leanness rule 4 — a parameter every caller passes the
/// same value for is a constant).
pub async fn serve(
    port: u16,
    gateway: Gateway,
    pairings: Pairings,
    shutdown: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, path = MCP_PATH, "MCP gateway listening on loopback");

    axum::serve(listener, router(gateway, pairings))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
}

/// Refuse anything that is not a paired agent on this machine.
async fn guard_middleware(
    State(pairings): State<Pairings>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let header_str = |name: header::HeaderName| {
        headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };

    let host = header_str(header::HOST);
    let origin = header_str(header::ORIGIN);
    let authorization = header_str(header::AUTHORIZATION);

    if let Err(refusal) = check_host(host.as_deref()).and_then(|()| check_origin(origin.as_deref()))
    {
        return refuse(refusal);
    }

    let token = match bearer_token(authorization.as_deref()) {
        Ok(token) => token.to_owned(),
        Err(refusal) => return refuse(refusal),
    };

    // The read guard is released before the request is forwarded, so a tool
    // that pairs or revokes cannot deadlock against its own session.
    let authenticated = {
        let store = match pairings.read() {
            Ok(store) => store,
            // A poisoned lock means a previous request panicked while holding
            // it. Fail closed rather than reason about what it left behind.
            Err(_) => return refuse(Refusal::Auth(crate::auth::AuthError::Unauthenticated)),
        };
        store.authenticate(&token).map(|session| session.id)
    };

    match authenticated {
        Ok(id) => {
            tracing::debug!(pairing = %id, "authenticated");
            next.run(request).await
        }
        Err(error) => refuse(Refusal::Auth(error)),
    }
}

/// Render a refusal.
///
/// The body is the same three words whatever the cause. The operator gets the
/// detail through `tracing`; the caller gets nothing it could use to tell a
/// wrong token from an unknown one, or a rebinding attempt from a missing
/// header.
fn refuse(refusal: Refusal) -> Response {
    tracing::warn!(%refusal, "request refused at the gateway door");
    let status = match refusal {
        Refusal::HostMissing | Refusal::HostNotLoopback(_) | Refusal::OriginNotLoopback(_) => {
            StatusCode::FORBIDDEN
        }
        Refusal::BearerMissing | Refusal::Auth(_) => StatusCode::UNAUTHORIZED,
    };
    (status, "not authorised").into_response()
}
