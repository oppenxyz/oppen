//! The loopback HTTP server (`docs/spec.md` item 14, `docs/decisions.md` D2).
//!
//! `rmcp` owns the MCP protocol itself — JSON-RPC framing, version
//! negotiation, sessions, SSE. This module owns the two things `rmcp` cannot
//! decide for oppen: **where it listens**, and **who gets past the door**.
//!
//! The guard runs as a middleware in front of the transport rather than inside
//! a tool, so an unauthenticated request never reaches the protocol layer, let
//! alone a tool.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ListToolsResult, PaginatedRequestParams, ServerInfo,
    Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::auth::TokenStore;
use crate::guard::{Refusal, bearer_token, check_host, check_origin};
use crate::tools::Gateway;

/// The path agents connect to. `claude mcp add --transport http oppen
/// http://127.0.0.1:<port>/mcp`.
pub const MCP_PATH: &str = "/mcp";

/// Shared pairing registry. `RwLock` because authentication is a read on every
/// request and pairing or revoking is a rare write.
pub type Pairings = Arc<RwLock<TokenStore>>;

/// The runtime's gateway, with a substitutable MCP handler for lifecycle tests.
/// Pause enforcement always uses the real gateway; the test handler never signs.
pub trait GatewayHandler: rmcp::ServerHandler + Clone {
    fn gateway(&self) -> &Gateway;
}

impl GatewayHandler for Gateway {
    fn gateway(&self) -> &Gateway {
        self
    }
}

/// Build the router: the guard, then `rmcp`'s transport.
pub fn router(gateway: Gateway, pairings: Pairings) -> axum::Router {
    router_with_lifecycle(
        gateway,
        pairings,
        CancellationToken::new(),
        watch::channel(()).0,
    )
}

fn router_with_lifecycle(
    gateway: impl ServerHandler + Clone,
    pairings: Pairings,
    shutdown: CancellationToken,
    execution: watch::Sender<()>,
) -> axum::Router {
    let handler = ShutdownHandler {
        inner: gateway,
        shutdown: shutdown.clone(),
        execution,
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        LocalSessionManager::default().into(),
        rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
            .with_cancellation_token(shutdown.clone()),
    );

    axum::Router::new()
        .nest_service(MCP_PATH, service)
        .layer(axum::middleware::from_fn_with_state(
            (pairings, shutdown),
            guard_middleware,
        ))
}

// rmcp spawns method tasks independently of the HTTP request. The guard must
// live inside call_tool, and outlive the inner future, to join actual execution.
#[derive(Clone)]
struct ShutdownHandler<H> {
    inner: H,
    shutdown: CancellationToken,
    execution: watch::Sender<()>,
}

impl<H: ServerHandler> ServerHandler for ShutdownHandler<H> {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Receivers count active tool futures. Register before checking
        // shutdown: a late rmcp task cannot enter the tool after closed().
        let _execution = self.execution.subscribe();
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => Err(crate::outcome::ToolError::TimeoutUnknownOutcome {
                cloid: None,
                detail: "gateway shutdown interrupted execution; reconcile before resubmitting".into(),
            }.into()),
            result = async { self.inner.call_tool(request, context).await } => result,
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.inner.list_tools(request, context).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner.get_tool(name)
    }

    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }
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
    gateway: impl GatewayHandler,
    pairings: Pairings,
    shutdown: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, path = MCP_PATH, "MCP gateway listening on loopback");

    let shutdown = shutdown.child_token();
    // Dropping serve also cancels its connections and execution. Only awaiting
    // serve can guarantee that their teardown has finished.
    let _shutdown_guard = shutdown.clone().drop_guard();
    let (execution, _) = watch::channel(());

    let enforcement_gateway = gateway.gateway().clone();
    let enforcement_pairings = pairings.clone();
    let enforcement_shutdown = shutdown.child_token();
    // Also stop enforcement if the caller drops the serve future.
    let _enforcement_guard = enforcement_shutdown.clone().drop_guard();
    let enforcement = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = enforcement_shutdown.cancelled() => break,
                () = async {
                    interval.tick().await;
                    let bindings = match enforcement_pairings.read() {
                        Ok(store) => store.bindings(),
                        Err(error) => {
                            tracing::warn!(%error, "pause enforcement could not read pairings");
                            return;
                        }
                    };
                    if let Err(error) = enforcement_gateway.enforce_pauses(&bindings).await {
                        tracing::warn!(%error, "pause enforcement failed; will retry");
                    }
                } => {}
            }
        }
    });

    let listener = ShutdownListener {
        inner: listener,
        shutdown: shutdown.clone(),
    };
    // Await Axum's spawned connection tasks, not just its accept loop. Closing
    // their IO also handles idle SSE, partial requests, and blocked writers.
    let result = axum::serve(
        listener,
        router_with_lifecycle(gateway, pairings, shutdown.clone(), execution.clone()),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await;
    shutdown.cancel();
    if let Err(error) = enforcement.await
        && !error.is_cancelled()
    {
        tracing::warn!(%error, "pause enforcement task failed");
    }
    execution.closed().await;
    result
}

struct ShutdownListener {
    inner: tokio::net::TcpListener,
    shutdown: CancellationToken,
}

impl axum::serve::Listener for ShutdownListener {
    type Io = ShutdownIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (inner, addr) = axum::serve::Listener::accept(&mut self.inner).await;
        (
            ShutdownIo {
                inner,
                closed: Box::pin(self.shutdown.clone().cancelled_owned()),
            },
            addr,
        )
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

struct ShutdownIo {
    inner: tokio::net::TcpStream,
    closed: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl AsyncRead for ShutdownIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.closed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for ShutdownIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.closed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Refuse anything that is not a paired agent on this machine.
async fn guard_middleware(
    State((pairings, shutdown)): State<(Pairings, CancellationToken)>,
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

    // Release the read guard before forwarding so the operator can revoke a
    // pairing while its authenticated requests are still in flight.
    let authenticated = {
        let store = match pairings.read() {
            Ok(store) => store,
            // A poisoned lock means a previous request panicked while holding
            // it. Fail closed rather than reason about what it left behind.
            Err(_) => return refuse(Refusal::Auth(crate::auth::AuthError::Unauthenticated)),
        };
        store.authenticate(&token)
    };

    match authenticated {
        Ok(mut session) => {
            tracing::debug!(pairing = %session.id, agent = %session.binding.agent, "authenticated");
            // The tools read this back out of `RequestContext::extensions`,
            // where `rmcp` republishes the request's `http::request::Parts`.
            // It is the only place a tool learns which agent it is acting as,
            // so an unauthenticated request cannot reach one: there would be
            // nothing here to find.
            let mut request = request;
            request.extensions_mut().insert(session.binding.clone());
            let mut closed: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
                tokio::select! {
                    () = session.closed() => {},
                    () = shutdown.cancelled() => {},
                }
            });
            let response = tokio::select! {
                biased;
                () = &mut closed => return refuse(Refusal::Auth(crate::auth::AuthError::Revoked)),
                response = next.run(request) => response,
            };
            let (parts, body) = response.into_parts();
            Response::from_parts(
                parts,
                Body::from_stream(ReaderStream::new(SessionReader {
                    body,
                    buffered: Bytes::new(),
                    closed,
                })),
            )
        }
        Err(error) => refuse(Refusal::Auth(error)),
    }
}

// rmcp emits data frames (JSON or SSE), with no trailers. The reader keeps the
// revocation future owned by the response body, so client disconnects drop it
// and an idle SSE stream registers a wakeup without a detached watcher task.
struct SessionReader {
    body: Body,
    buffered: Bytes,
    closed: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl AsyncRead for SessionReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.closed.as_mut().poll(cx).is_ready() || buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if !self.buffered.is_empty() {
                let count = buf.remaining().min(self.buffered.len());
                buf.put_slice(&self.buffered.split_to(count));
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut self.body).poll_frame(cx)) {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buffered = data;
                    }
                }
                Some(Err(error)) => return Poll::Ready(Err(std::io::Error::other(error))),
                None => return Poll::Ready(Ok(())),
            }
        }
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
