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
use tokio::sync::{Notify, watch};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::auth::{SessionAuthority, TokenStore};
use crate::guard::{Refusal, bearer_token, check_host, check_origin};
use crate::tools::Gateway;

mod operator;
pub(crate) use operator::OperatorWork;
pub use operator::{OperatorControl, OperatorReview};

/// The path agents connect to. `claude mcp add --transport http oppen
/// http://127.0.0.1:<port>/mcp`.
pub const MCP_PATH: &str = "/mcp";

/// Shared pairing registry. `RwLock` because authentication is a read on every
/// request and pairing or revoking is a rare write.
pub type Pairings = Arc<RwLock<TokenStore>>;

// Decision workers may outlive the request waiting for them. Their lifetime
// remains part of serve's drain, and retains the pairing journal's owner.
#[derive(Clone)]
pub(crate) struct ExecutionTracker {
    _execution: watch::Receiver<()>,
    _owner: ExecutionOwner,
}

#[derive(Clone)]
enum ExecutionOwner {
    Session { _authority: SessionAuthority },
    Supervision { _pairings: Pairings },
}

impl ExecutionTracker {
    pub(crate) fn supervision(execution: &watch::Sender<()>, pairings: Pairings) -> Self {
        Self {
            _execution: execution.subscribe(),
            _owner: ExecutionOwner::Supervision {
                _pairings: pairings,
            },
        }
    }

    pub(crate) fn from_context(
        context: &RequestContext<RoleServer>,
    ) -> Result<Self, crate::outcome::ToolError> {
        context.extensions.get::<Self>().cloned().ok_or_else(|| {
            crate::outcome::ToolError::unavailable(
                "execution tracker",
                "decision requires an authenticated, tracked method task",
            )
        })
    }
}

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
    gateway: impl GatewayHandler,
    pairings: Pairings,
    shutdown: CancellationToken,
    execution: watch::Sender<()>,
) -> axum::Router {
    let network = gateway.gateway().network();
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
            (pairings, shutdown, network),
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
        mut context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Receivers count active tool futures. Register before checking
        // shutdown: a late rmcp task cannot enter the tool after closed().
        let _execution = self.execution.subscribe();
        let mut authority = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<SessionAuthority>())
            .cloned()
            .ok_or_else(|| crate::outcome::ToolError::Unavailable {
                what: "pairing authority",
                detail: "this method task carries no authenticated pairing authority".into(),
            })?;
        context.extensions.insert(ExecutionTracker {
            _execution: self.execution.subscribe(),
            _owner: ExecutionOwner::Session {
                _authority: authority.clone(),
            },
        });
        // Keep authority outside the selected inner future: HTTP disconnects
        // and cancellation must not release the owner lease before it drains.
        tokio::select! {
            biased;
            () = authority.closed() => Err(crate::outcome::ToolError::TimeoutUnknownOutcome {
                cloid: None,
                detail: "pairing revoked during execution; reconcile before resubmitting".into(),
            }.into()),
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
    BoundServer::bind(port, gateway.gateway(), &pairings)
        .await?
        .serve(gateway, pairings, shutdown)
        .await
}

/// An owned loopback listener, bound before the desktop starts venue work.
/// Binding proves only local socket ownership, not MCP or trading readiness.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    address: SocketAddr,
    network: oppen_hl::Network,
    supervision: watch::Sender<SupervisionStatus>,
    supervision_wake: Arc<Notify>,
    operator: OperatorControl,
}

/// Cached pause-sweep observation, not proof that the venue is flat or ready.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SupervisionStatus {
    pub started_sequence: u64,
    pub completed_sequence: u64,
    pub in_progress: bool,
    pub last_completed_ms: Option<u64>,
    pub last_error: Option<String>,
}

/// Native-only sweep wake and observation, not a cancellation receipt.
#[derive(Clone)]
pub struct SupervisionControl {
    status: watch::Receiver<SupervisionStatus>,
    wake: Arc<Notify>,
}

impl SupervisionControl {
    /// Call after the durable halt. Only completion strictly above this baseline
    /// can describe a subsequent sweep; its error must still be checked.
    pub fn request_sweep(&self) -> u64 {
        let baseline = self.status.borrow().started_sequence;
        self.wake.notify_one();
        baseline
    }

    pub fn status(&self) -> watch::Receiver<SupervisionStatus> {
        self.status.clone()
    }
}

impl BoundServer {
    pub async fn bind(port: u16, gateway: &Gateway, pairings: &Pairings) -> std::io::Result<Self> {
        validate_pairing_network(gateway.network(), pairings)?;
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let listener = tokio::net::TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        Ok(Self {
            listener,
            address,
            network: gateway.network(),
            supervision: watch::channel(SupervisionStatus::default()).0,
            supervision_wake: Arc::new(Notify::new()),
            operator: OperatorControl::new(gateway.clone(), pairings.clone()),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub fn supervision_status(&self) -> watch::Receiver<SupervisionStatus> {
        self.supervision.subscribe()
    }

    pub fn supervision_control(&self) -> SupervisionControl {
        SupervisionControl {
            status: self.supervision.subscribe(),
            wake: self.supervision_wake.clone(),
        }
    }

    pub fn operator_control(&self) -> OperatorControl {
        self.operator.clone()
    }

    pub async fn serve(
        self,
        gateway: impl GatewayHandler,
        pairings: Pairings,
        shutdown: CancellationToken,
    ) -> std::io::Result<()> {
        if self.network != gateway.gateway().network() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bound listener and gateway networks do not match",
            ));
        }
        validate_pairing_network(self.network, &pairings)?;
        self.operator.start(gateway.gateway(), &pairings)?;
        let lifecycle = self.operator.owner.lifecycle.clone();
        drop(self.operator);
        struct CloseOperator(Arc<operator::OperatorLifecycle>);
        impl Drop for CloseOperator {
            fn drop(&mut self) {
                self.0.close();
                self.0.shutdown.cancel();
            }
        }
        let _close = CloseOperator(lifecycle.clone());
        let serving = serve_bound(
            self.listener,
            gateway,
            pairings,
            lifecycle.shutdown.clone(),
            self.supervision,
            self.supervision_wake,
            lifecycle.execution.clone(),
        );
        tokio::pin!(serving);
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                lifecycle.close();
                lifecycle.shutdown.cancel();
                serving.await
            }
            result = &mut serving => result,
        }
    }
}

fn validate_pairing_network(
    network: oppen_hl::Network,
    pairings: &Pairings,
) -> std::io::Result<()> {
    let pairing_network = pairings
        .try_read()
        .map(|store| store.network())
        .map_err(|error| {
            let kind = match error {
                std::sync::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock,
                std::sync::TryLockError::Poisoned(_) => std::io::ErrorKind::Other,
            };
            std::io::Error::new(kind, "pairing authority unavailable")
        })?;
    if pairing_network != network {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pairing store and gateway networks do not match",
        ));
    }
    Ok(())
}

async fn serve_bound(
    listener: tokio::net::TcpListener,
    gateway: impl GatewayHandler,
    pairings: Pairings,
    shutdown: CancellationToken,
    supervision: watch::Sender<SupervisionStatus>,
    supervision_wake: Arc<Notify>,
    execution: watch::Sender<()>,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, path = MCP_PATH, "MCP gateway listening on loopback");

    // Dropping serve also cancels its connections and execution. Only awaiting
    // serve can guarantee that their teardown has finished.
    let _shutdown_guard = shutdown.clone().drop_guard();

    let enforcement_gateway = gateway.gateway().clone();
    let enforcement_pairings = pairings.clone();
    let enforcement_shutdown = shutdown.child_token();
    let enforcement_execution = execution.clone();
    // Also stop enforcement if the caller drops the serve future.
    let _enforcement_guard = enforcement_shutdown.clone().drop_guard();
    let enforcement = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let sequence = tokio::select! {
                biased;
                () = enforcement_shutdown.cancelled() => break,
                sequence = next_supervision(&mut interval, &supervision_wake, &supervision) => {
                    let Some(sequence) = sequence else {
                        tracing::error!("pause supervision sequence exhausted");
                        break;
                    };
                    sequence
                }
            };
            tokio::select! {
                biased;
                () = enforcement_shutdown.cancelled() => break,
                () = async {
                    let Some(bindings) = supervision_bindings(&enforcement_pairings, &supervision, sequence) else {
                        return;
                    };
                    let tracker = ExecutionTracker::supervision(&enforcement_execution, enforcement_pairings.clone());
                    let result = enforcement_gateway.enforce_pauses(&bindings, tracker).await;
                    if let Err(error) = &result {
                        tracing::warn!(%error, "pause enforcement failed; will retry");
                    }
                    finish_supervision(&supervision, sequence, result.err().map(|error| error.to_string()));
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
    let server = axum::serve(
        listener,
        router_with_lifecycle(gateway, pairings, shutdown.clone(), execution.clone()),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned());
    supervise_and_drain(
        async move { server.await },
        enforcement,
        shutdown,
        execution,
    )
    .await
}

async fn next_supervision(
    interval: &mut tokio::time::Interval,
    wake: &Notify,
    status: &watch::Sender<SupervisionStatus>,
) -> Option<u64> {
    tokio::select! {
        _ = interval.tick() => {},
        () = wake.notified() => {},
    }
    // Only the supervisor writes sequences. Never wrap into an old receipt.
    let sequence = status.borrow().started_sequence.checked_add(1)?;
    status.send_modify(|status| {
        status.started_sequence = sequence;
        status.in_progress = true;
    });
    Some(sequence)
}

fn supervision_bindings(
    pairings: &Pairings,
    status: &watch::Sender<SupervisionStatus>,
    sequence: u64,
) -> Option<Vec<crate::auth::Binding>> {
    match pairings.try_read() {
        Ok(store) => Some(store.bindings()),
        Err(error) => {
            tracing::warn!(%error, "pause enforcement could not read pairings; will retry");
            finish_supervision(
                status,
                sequence,
                Some("pairing authority unavailable; pause sweep will retry".into()),
            );
            None
        }
    }
}

fn finish_supervision(
    status: &watch::Sender<SupervisionStatus>,
    sequence: u64,
    error: Option<String>,
) {
    status.send_modify(|status| {
        status.completed_sequence = sequence;
        status.in_progress = false;
        status.last_completed_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        status.last_error = error;
    });
}

// A dead supervisor must not leave an apparently healthy transport running.
// Cancellation closes admission, but completion still requires both task
// teardown and every tracked blocking decision to finish.
async fn supervise_and_drain(
    server: impl Future<Output = std::io::Result<()>>,
    mut enforcement: tokio::task::JoinHandle<()>,
    shutdown: CancellationToken,
    execution: watch::Sender<()>,
) -> std::io::Result<()> {
    tokio::pin!(server);
    let (server_result, enforcement_result, unexpected_exit) = tokio::select! {
        result = &mut enforcement => {
            let unexpected_exit = !shutdown.is_cancelled();
            shutdown.cancel();
            (server.await, result, unexpected_exit)
        }
        result = &mut server => {
            shutdown.cancel();
            (result, enforcement.await, false)
        }
    };
    execution.closed().await;
    enforcement_result.map_err(|error| {
        std::io::Error::other(format!("pause enforcement task failed: {error}"))
    })?;
    if unexpected_exit {
        return Err(std::io::Error::other(
            "pause enforcement task exited unexpectedly",
        ));
    }
    server_result
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
    State((pairings, shutdown, network)): State<(Pairings, CancellationToken, oppen_hl::Network)>,
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
        let store = match pairings.try_read() {
            Ok(store) => store,
            // Durable mutation can hold the writer through ledger IO. Never
            // wait on an async worker; contention and poison both fail closed.
            Err(_) => return refuse(Refusal::Auth(crate::auth::AuthError::Unauthenticated)),
        };
        if store.network() != network {
            return refuse(Refusal::Auth(crate::auth::AuthError::Unauthenticated));
        }
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
            request.extensions_mut().insert(session.authority());
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

#[cfg(test)]
mod authority_tests {
    use super::*;
    use crate::auth::Binding;
    use oppen_core::guardrail::AgentId;
    use oppen_core::keys::HmacKey;
    use oppen_core::ledger::{Ledger, PairingJournal};

    fn supervision_fixture() -> (watch::Sender<SupervisionStatus>, SupervisionControl) {
        let (sender, status) = watch::channel(SupervisionStatus::default());
        (
            sender,
            SupervisionControl {
                status,
                wake: Arc::new(Notify::new()),
            },
        )
    }

    fn distant_timer() -> tokio::time::Interval {
        tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(3600),
            Duration::from_secs(5),
        )
    }

    #[tokio::test]
    async fn supervision_control_wakes_waiting_sweep_without_timer() {
        let (status, control) = supervision_fixture();
        let mut observer = control.status();
        let mut timer = distant_timer();
        let next = next_supervision(&mut timer, &control.wake, &status);
        tokio::pin!(next);
        tokio::select! {
            biased;
            _ = &mut next => panic!("timer must not admit a sweep yet"),
            () = std::future::ready(()) => {},
        }
        assert_eq!(control.clone().request_sweep(), 0);
        let sequence = tokio::time::timeout(Duration::from_secs(1), next)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sequence, 1);
        observer.changed().await.unwrap();
        assert_eq!(observer.borrow().started_sequence, 1);
        assert_eq!(observer.borrow().completed_sequence, 0);
        assert!(observer.borrow().in_progress);
        finish_supervision(&status, sequence, None);
        assert_eq!(observer.borrow().completed_sequence, 1);
        assert!(observer.borrow().last_completed_ms.is_some());
        assert!(!observer.borrow().in_progress);
    }

    #[tokio::test]
    async fn supervision_control_old_or_inflight_completion_cannot_satisfy_request() {
        let (status, control) = supervision_fixture();
        let mut timer = distant_timer();
        control.request_sweep();
        let old = next_supervision(&mut timer, &control.wake, &status)
            .await
            .unwrap();
        finish_supervision(&status, old, None);
        control.request_sweep();
        let active = next_supervision(&mut timer, &control.wake, &status)
            .await
            .unwrap();
        let baseline = control.request_sweep();
        assert_eq!(baseline, active);
        assert!(status.borrow().completed_sequence < baseline);
        assert_eq!(control.clone().request_sweep(), baseline);
        assert_eq!(status.borrow().started_sequence, active);
        assert!(status.borrow().in_progress);
        finish_supervision(&status, active, None);
        assert_eq!(status.borrow().completed_sequence, baseline);

        // Requests during the active sweep retain one permit, not one per call.
        let subsequent = tokio::time::timeout(
            Duration::from_secs(1),
            next_supervision(&mut timer, &control.wake, &status),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(subsequent > baseline);
        assert_eq!(status.borrow().completed_sequence, baseline);
        finish_supervision(&status, subsequent, None);
        assert!(status.borrow().completed_sequence > baseline);
        tokio::select! {
            biased;
            _ = next_supervision(&mut timer, &control.wake, &status) => panic!("requests did not coalesce"),
            () = std::future::ready(()) => {},
        }
    }

    #[tokio::test]
    async fn supervision_authority_failure_completes_sequence_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(
            Ledger::open_at(&dir.path().join("ledger.db"), oppen_hl::Network::Testnet).unwrap(),
        );
        let pairings = Arc::new(RwLock::new(
            TokenStore::open(
                PairingJournal::open(ledger, Arc::new(HmacKey::from_bytes([42; 32]))).unwrap(),
            )
            .unwrap(),
        ));
        let (status, control) = supervision_fixture();
        let mut timer = distant_timer();
        let baseline = control.request_sweep();
        let sequence = next_supervision(&mut timer, &control.wake, &status)
            .await
            .unwrap();
        {
            let _unavailable = pairings.write().unwrap();
            assert!(supervision_bindings(&pairings, &status, sequence).is_none());
        }
        assert!(status.borrow().completed_sequence > baseline);
        assert_eq!(status.borrow().started_sequence, sequence);
        assert_eq!(status.borrow().completed_sequence, sequence);
        assert!(!status.borrow().in_progress);
        assert!(status.borrow().last_completed_ms.is_some());
        assert!(status.borrow().last_error.is_some());

        control.request_sweep();
        let retry = next_supervision(&mut timer, &control.wake, &status)
            .await
            .unwrap();
        assert!(
            supervision_bindings(&pairings, &status, retry)
                .unwrap()
                .is_empty()
        );
        assert_eq!(status.borrow().completed_sequence, sequence);
        assert!(status.borrow().last_error.is_some());
        finish_supervision(&status, retry, None);
        assert_eq!(status.borrow().completed_sequence, retry);
        assert!(status.borrow().last_error.is_none());
    }

    #[tokio::test]
    async fn supervisor_panic_closes_transport_but_waits_for_actual_work() {
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let (execution, worker) = watch::channel(());
        let (transport_closed, closed) = tokio::sync::oneshot::channel();
        let (release_transport, transport_release) = tokio::sync::oneshot::channel();
        let enforcement = tokio::spawn(async { panic!("supervisor failure fixture") });
        let task = tokio::spawn(supervise_and_drain(
            async move {
                server_shutdown.cancelled().await;
                transport_closed.send(()).unwrap();
                transport_release.await.unwrap();
                Ok(())
            },
            enforcement,
            shutdown.clone(),
            execution,
        ));
        closed.await.unwrap();
        assert!(shutdown.is_cancelled());
        assert!(!task.is_finished());
        release_transport.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        drop(worker);
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("pause enforcement task failed"));
    }

    #[tokio::test]
    async fn unexpected_supervisor_return_is_not_success() {
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let (execution, unused) = watch::channel(());
        drop(unused);
        let result = supervise_and_drain(
            async move {
                server_shutdown.cancelled().await;
                Ok(())
            },
            tokio::spawn(async {}),
            shutdown,
            execution,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exited unexpectedly")
        );
    }

    #[tokio::test]
    async fn transport_failure_cancels_supervisor_and_preserves_error() {
        let shutdown = CancellationToken::new();
        let supervisor_shutdown = shutdown.clone();
        let (finished, completion) = tokio::sync::oneshot::channel();
        let (execution, unused) = watch::channel(());
        drop(unused);
        let result = supervise_and_drain(
            async { Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted)) },
            tokio::spawn(async move {
                supervisor_shutdown.cancelled().await;
                finished.send(()).unwrap();
            }),
            shutdown,
            execution,
        )
        .await;
        completion.await.unwrap();
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
    }

    #[tokio::test]
    async fn requested_shutdown_is_success_after_both_tasks_finish() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let (execution, unused) = watch::channel(());
        drop(unused);
        supervise_and_drain(
            async { Ok(()) },
            tokio::spawn(async {}),
            shutdown,
            execution,
        )
        .await
        .unwrap();
    }

    #[derive(Clone)]
    struct MustNotRun;

    impl ServerHandler for MustNotRun {
        async fn call_tool(
            &self,
            _: CallToolRequestParams,
            _: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            panic!("unauthorized method reached inner execution")
        }
    }

    #[tokio::test]
    async fn actual_method_requires_authority_and_checks_already_revoked() {
        let (transport, _client) = tokio::io::duplex(4096);
        let service = rmcp::service::serve_directly(MustNotRun, transport, None);
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(
            Ledger::open_at(&dir.path().join("ledger.db"), oppen_hl::Network::Testnet).unwrap(),
        );
        let mut store = TokenStore::open(
            PairingJournal::open(ledger, Arc::new(HmacKey::from_bytes([42; 32]))).unwrap(),
        )
        .unwrap();
        let binding = Binding {
            agent: AgentId::new("alpha"),
            account: oppen_hl::Address::from_bytes([9; 20]),
        };
        let token = store.issue(binding.clone()).unwrap();
        let authority = store.authenticate(token.reveal()).unwrap().authority();
        store.revoke(token.id).unwrap();
        let handler = ShutdownHandler {
            inner: MustNotRun,
            shutdown: CancellationToken::new(),
            execution: watch::channel(()).0,
        };
        for mode in 0..3 {
            let mut context = RequestContext::new(
                rmcp::model::NumberOrString::Number(1),
                service.peer().clone(),
            );
            if mode != 0 {
                let (mut parts, ()) = http::Request::new(()).into_parts();
                parts.extensions.insert(binding.clone());
                if mode == 2 {
                    parts.extensions.insert(authority.clone());
                }
                context.extensions.insert(parts);
            }
            let error = handler
                .call_tool(CallToolRequestParams::new("must_not_run"), context)
                .await
                .unwrap_err();
            let expected = if mode == 2 {
                "timeout_unknown_outcome"
            } else {
                "unavailable"
            };
            assert_eq!(error.data.unwrap()["code"], expected);
            assert_eq!(handler.execution.receiver_count(), 0);
        }
        service.cancel().await.unwrap();
    }
}
