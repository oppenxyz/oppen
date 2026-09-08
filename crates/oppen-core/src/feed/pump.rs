//! Running the feed: a live socket's events into the session, and the
//! reconcile the session asks for (`docs/spec.md` item 9).
//!
//! [`crate::feed::FeedSession`] decides what an event means and
//! [`crate::reconcile::Reconciler`] closes the windows a drop leaves behind.
//! Both were already here and nothing joined them to a socket, so `place` kept
//! answering `reconciled: false` against a session no events reached. This is
//! that join, and it is deliberately thin: every judgement it could make has
//! already been made in one of the two modules it calls.
//!
//! **Why a fresh start reconciles at all.** The pool emits
//! [`oppen_hl::ws::WsEvent::Reconnected`] only in answer to a `Disconnected`,
//! so a clean first connect produces none, and a pump that waited for one
//! would leave the account permanently unreconciled — the exact refusal this
//! is meant to lift. So [`FeedPump::run`] opens and immediately closes a gap
//! for the account's own feeds before reading a single event. That is not a
//! trick: oppen genuinely was not watching until now, and the window before
//! now is genuinely unknown. `reconcile.rs` already answers that window
//! correctly from the chain — the newest fill it holds, less the venue-clock
//! margin — or walks its first-run lookback when the chain holds none.
//!
//! **The window between the startup walk and the socket being live is closed
//! by the venue, not by ordering.** The walk ends at the venue's own now, and
//! the subscriptions are not acknowledged until some milliseconds later; a
//! fill printing in between is in neither. It arrives anyway, because
//! `userFills` sends a snapshot of the recent backlog on subscribe, and
//! [`crate::feed::FeedSession::apply`] records a snapshot fill exactly like a
//! live one — deduped by the venue's `tid`. That is the same property the
//! reconnect backlog relies on, so nothing here has to wait for an ack the
//! pool does not report.
//!
//! **A reconcile that fails is retried on a timer, not on the next event.** A
//! failed window leaves its gap open and the account unreconciled, which
//! refuses every order; if the retry were driven by arriving events, an
//! account that went quiet at exactly the wrong moment would stay refused
//! until the market moved.

use std::time::Duration;

use oppen_hl::Address;
use oppen_hl::ws::{EventReceiver, FrameEnvelope};
use oppen_hl::ws::{PoolError, Subscription, WsEvent, WsPool};

use crate::alert::{self, AlertStore, Fired, MarketTick};
use crate::features::quotes::QuoteCache;
use crate::feed::{Action, FeedSession};
use crate::ledger::{EventKind, Ledger, NewEvent, now_ms};
use crate::reconcile::{GapStatus, ReconcileError, ReconcileSource, Reconciler};

/// What the pump needs from the socket pool to answer an alert.
///
/// A trait because the tests must not open a socket: [`WsPool::subscribe`]
/// spawns a connection task on the first subscription for a shard, so a pump
/// test holding a real pool would reach the venue. Two methods, both of which
/// [`WsPool`] already has.
pub trait FeedSubscriber {
    fn subscribe(&self, sub: Subscription) -> Result<(), PoolError>;
    fn unsubscribe(&self, sub: &Subscription) -> Result<(), PoolError>;
}

impl FeedSubscriber for WsPool {
    fn subscribe(&self, sub: Subscription) -> Result<(), PoolError> {
        WsPool::subscribe(self, sub)
    }

    fn unsubscribe(&self, sub: &Subscription) -> Result<(), PoolError> {
        WsPool::unsubscribe(self, sub)
    }
}

/// The channel `micro_tilt_bps` is answered from.
///
/// Separate from [`market_feed`] because they are different questions:
/// `activeAssetCtx` carries the mark and the funding at ~1 s, and `bbo`
/// carries the top of book at 0.10–0.13 s. §14.4 correction 4 is explicit that
/// `micro` sources from this one and not from the depth ladder.
fn quote_feed(symbol: &str) -> Subscription {
    Subscription::Bbo {
        coin: symbol.to_owned(),
    }
}

/// The channel an alert's market condition is answered from.
///
/// `activeAssetCtx` rather than `bbo`: one ~1 s frame carries both the mark a
/// price cross reads and the funding a rate threshold reads, so the two
/// conditions cost one subscription rather than two. `bbo` is the microprice
/// source and carries neither.
fn market_feed(symbol: &str) -> Subscription {
    Subscription::ActiveAssetCtx {
        coin: symbol.to_owned(),
    }
}

/// How often an unreconciled account retries its open windows.
///
/// The account cannot trade while it is unreconciled, so this is the latency
/// of recovering from a transient venue error — it wants to be short. It is
/// also a `POST /info` request against the address's shared budget
/// (`docs/spec.md` item 10), so it does not want to be seconds. Thirty is the
/// same order as the outage the P2 gate is written about.
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// The note a startup gap carries, so the operator can tell one from a socket
/// that actually dropped.
const STARTUP_NOTE: &str = "oppen was not watching";

/// Feeds the pump opens a startup window for.
///
/// The two account channels and nothing else: these are the ones
/// `reconcile.rs` can actually close, and a startup gap on a market-data feed
/// would sit unreconciled forever with no component able to answer it. A
/// market-data outage is still recorded — but by a real
/// [`WsEvent::Disconnected`], which is a real outage, rather than by oppen
/// starting up.
fn account_scopes(account: Address) -> [String; 2] {
    [
        Subscription::UserFills { user: account }.key(),
        Subscription::OrderUpdates { user: account }.key(),
    ]
}

/// Drives one account's feeds.
///
/// Borrows rather than owning an [`std::sync::Arc`] of each: the ledger has to
/// outlive the [`Reconciler`] built over it, and a pump that owned both would
/// be self-referential. The caller holds them and calls [`FeedPump::run`] in
/// that scope, which is what `examples/serve.rs` does.
#[derive(Debug)]
pub struct FeedPump<'a, S, F> {
    session: &'a FeedSession,
    ledger: &'a Ledger,
    account: Address,
    /// `account` as [`crate::feed::FeedSession::apply`] wants it. Held rather
    /// than formatted per event, which would allocate on every tick.
    account_id: String,
    reconciler: Reconciler<'a, S>,
    alerts: &'a AlertStore,
    /// The latest `bbo` per symbol, for `get_features`. The pump fills it and
    /// subscribes what it leases; see [`QuoteCache`] for why the lifecycle is
    /// a lease rather than the alert module's armed set.
    quotes: &'a QuoteCache,
    /// What the pump subscribes a symbol through. See [`FeedSubscriber`].
    feeds: &'a F,
    /// The market feeds this pump has subscribed for alerts, so a fired alert
    /// gives its socket back. Sorted, because everything that reaches a
    /// serialized surface is (`AGENTS.md` invariant 6) and because the diff
    /// against `watched_symbols` is one comparison of two sorted lists.
    subscribed: std::sync::Mutex<Vec<Subscription>>,
}

impl<'a, S: ReconcileSource, F: FeedSubscriber> FeedPump<'a, S, F> {
    /// Build a pump.
    ///
    /// Refuses a source that is not on the ledger's own network — that is
    /// [`Reconciler::new`]'s check, and inheriting it is why the reconciler is
    /// built here rather than per reconcile (`docs/decisions.md` R4).
    pub fn new(
        session: &'a FeedSession,
        ledger: &'a Ledger,
        account: Address,
        source: S,
        alerts: &'a AlertStore,
        quotes: &'a QuoteCache,
        feeds: &'a F,
    ) -> Result<Self, ReconcileError> {
        let reconciler = Reconciler::new(ledger, source)?;
        session.bind(ledger.network(), account)?;
        session.bind_ledger(ledger)?;
        Ok(FeedPump {
            session,
            ledger,
            account,
            account_id: account.to_string(),
            reconciler,
            alerts,
            quotes,
            feeds,
            subscribed: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Catch up, then fold events until the pool is gone.
    ///
    /// Returns after all event senders are dropped and the queue is consumed.
    /// Pool shutdown alone does not close this stream: the pool retains a
    /// sender. Owners retaining the pool should use `run_until_shutdown`.
    pub async fn run(&self, events: &mut EventReceiver) {
        self.run_inner(events, std::future::pending(), std::future::pending())
            .await;
        self.session.unreconciled();
    }

    /// Finish active work and acknowledge quiescence before pool shutdown.
    /// After acknowledgment no new subscription or reconciliation work starts,
    /// but events keep folding while producers finish. Send `producers_done`
    /// only after `WsPool::shutdown_and_drain` has joined them; the receiver is
    /// then closed and its queued events drained. Pool shutdown may discard
    /// in-flight frames, so this is not lossless venue reconciliation.
    /// Dropping either signal sender activates its phase. Early producer
    /// completion implies quiescence and final drain without further work.
    ///
    /// The owner must retain this run future/task through completion, even if
    /// an observer stops waiting. Do not abort it to implement shutdown.
    /// The receiver owner calls `EventReceiver::complete` after this returns
    /// and all physical producers (including the pool's sender) are released.
    /// Completion proves drain, not healthy application: inspect the retained
    /// `FeedSession::state().failure` before reporting the owner's outcome.
    pub async fn run_until_shutdown(
        &self,
        events: &mut EventReceiver,
        quiesce: tokio::sync::oneshot::Receiver<tokio::sync::oneshot::Sender<()>>,
        producers_done: tokio::sync::oneshot::Receiver<()>,
    ) {
        self.run_inner(events, async { quiesce.await.ok() }, async {
            let _ = producers_done.await;
        })
        .await;
        self.session.unreconciled();
    }

    async fn run_inner(
        &self,
        events: &mut EventReceiver,
        quiesce: impl std::future::Future<Output = Option<tokio::sync::oneshot::Sender<()>>>,
        producers_done: impl std::future::Future<Output = ()>,
    ) {
        if let Err(error) = self.session.bind_ingress(events.monitor()) {
            self.session.record_failure(error.to_string());
            return;
        }
        tokio::pin!(quiesce, producers_done);
        let mut retry =
            tokio::time::interval_at(tokio::time::Instant::now() + RETRY_INTERVAL, RETRY_INTERVAL);
        let mut started = false;
        let mut subscribed = false;
        let mut quiescent = false;
        enum Next {
            Shutdown,
            Quiesce(Option<tokio::sync::oneshot::Sender<()>>),
            Startup,
            SyncFeeds,
            Retry,
            Event(Option<FrameEnvelope>),
        }
        loop {
            // Only readiness selection is cancellable. Shutdown has priority,
            // while ordinary sources retain fair selection under demand churn.
            let next = tokio::select! {
                biased;
                () = &mut producers_done => Next::Shutdown,
                ack = &mut quiesce, if !quiescent => Next::Quiesce(ack),
                next = async {
                    if quiescent { return Next::Event(events.recv().await); }
                    if !started { return Next::Startup; }
                    if !subscribed { return Next::SyncFeeds; }
                    tokio::select! {
                        // Quiet markets must still acquire newly demanded feeds.
                        () = self.alerts.armed_changed() => Next::SyncFeeds,
                        () = self.quotes.leased_changed() => Next::SyncFeeds,
                        _ = retry.tick() => Next::Retry,
                        event = events.recv() => Next::Event(event),
                    }
                } => next,
            };
            match next {
                Next::Shutdown => {
                    self.drain_events(events).await;
                    return;
                }
                Next::Quiesce(ack) => {
                    quiescent = true;
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                }
                Next::Startup => {
                    // Active work is outside every cancellable select. The
                    // acknowledgment cannot run ahead of its durable effects.
                    self.catch_up().await;
                    retry.reset();
                    started = true;
                }
                Next::SyncFeeds => {
                    self.sync_alert_feeds();
                    subscribed = true;
                }
                Next::Retry => {
                    // Unconditional: a lease expires on a clock, not on an
                    // event, so a healthy session that reconciles nothing is
                    // exactly when a swept lease has to give its socket back.
                    self.sync_alert_feeds();
                    if !self.session.state().reconciled {
                        self.reconcile().await;
                    }
                }
                Next::Event(Some(event)) => self.handle(event, quiescent).await,
                Next::Event(None) => {
                    if quiescent {
                        producers_done.await;
                    }
                    return;
                }
            }
        }
    }

    async fn drain_events(&self, events: &mut EventReceiver) {
        events.close();
        while let Some(event) = events.recv().await {
            self.handle(event, true).await;
        }
    }

    /// Subscribe the market feeds the armed alerts and the leased quotes need,
    /// and drop the ones nothing needs any more.
    ///
    /// The account channels are not touched: they are subscribed for the life
    /// of the pump and a fill alert reads them without asking for anything.
    ///
    /// The two demands are kept separate on the wire because they are
    /// different channels — an alert wants `activeAssetCtx`, a feature call
    /// wants `bbo` — but they are synced together so one pass reconciles
    /// everything the pump is holding.
    fn sync_alert_feeds(&self) {
        let armed = match self.alerts.watched_symbols() {
            Ok(armed) => armed,
            Err(error) => {
                // Left as it is rather than torn down: an unreadable store is
                // a reason to keep the feeds already flowing, not to stop
                // watching what is still armed.
                tracing::error!(%error, "could not read the alert watch list");
                self.session
                    .record_failure(format!("could not read the alert watch list: {error}"));
                return;
            }
        };
        let mut wanted: Vec<Subscription> = armed.iter().map(|s| market_feed(s)).collect();
        wanted.extend(
            self.quotes
                .leased(now_ms_u64())
                .iter()
                .map(|s| quote_feed(s)),
        );
        wanted.sort();

        let mut held = self.subscribed.lock().unwrap_or_else(|e| e.into_inner());
        let missing: Vec<Subscription> = wanted
            .iter()
            .filter(|sub| !held.contains(sub))
            .cloned()
            .collect();
        for sub in missing {
            match self.feeds.subscribe(sub.clone()) {
                // Recorded only once the pool took it, so a refused
                // subscription is retried on the next change rather than
                // remembered as held.
                Ok(()) => held.push(sub),
                Err(error) => {
                    tracing::error!(%error, feed = %sub.key(), "no feed for this demand");
                    self.session
                        .record_failure(format!("could not subscribe {}: {error}", sub.key()));
                }
            }
        }
        held.retain(|sub| {
            if wanted.contains(sub) {
                return true;
            }
            if let Err(error) = self.feeds.unsubscribe(sub) {
                tracing::warn!(%error, feed = %sub.key(), "could not drop a feed");
                self.session
                    .record_failure(format!("could not unsubscribe {}: {error}", sub.key()));
            }
            false
        });
        held.sort();
    }

    /// Chain what fired, so the agent reads it on its next `get_events`.
    ///
    /// One row per alert, attributed to the agent that armed it — which is
    /// what makes `docs/decisions.md` C6 deliver it to that agent and to
    /// nobody else. A row that cannot be written is reported and the others
    /// still go: the alert has already been marked fired, so dropping the
    /// batch would lose the rest of the wakeups too.
    fn chain(&self, fired: Vec<Fired>, now: i64, draining: bool) {
        for alert in fired {
            let payload = alert.payload();
            let event = NewEvent {
                kind: EventKind::Alert,
                ts_ms: now,
                agent_id: Some(&alert.agent),
                payload: &payload,
                snapshot: None,
            };
            if let Err(error) = self.ledger.append(&event) {
                tracing::error!(%error, alert_id = alert.alert_id, "alert fired unrecorded");
                self.session.record_failure(format!(
                    "alert {} fired unrecorded: {error}",
                    alert.alert_id
                ));
            }
        }
        // A firing changes the armed set, so a feed nothing watches any more
        // is given back.
        if !draining {
            self.sync_alert_feeds();
        }
    }

    /// Open and immediately close a window for everything before now, then
    /// work it. See the module docs for why this exists.
    async fn catch_up(&self) {
        let at_ms = now_ms();
        for scope in account_scopes(self.account) {
            // `open_gap` is idempotent per scope, so a window an earlier run
            // left open comes back here and is closed at now — which is when
            // that outage actually ended.
            match self
                .ledger
                .open_gap(&scope, at_ms, Some(STARTUP_NOTE))
                .and_then(|gap| self.ledger.close_gap(gap.gap_id, at_ms))
            {
                Ok(_) => {}
                // No later empty work list can prove this missing evidence safe.
                Err(error) => {
                    tracing::error!(%error, scope, "could not open the startup window");
                    self.session.record_failure(format!(
                        "could not open or close startup window {scope}: {error}"
                    ));
                }
            }
        }
        self.reconcile().await;
    }

    async fn handle(&self, envelope: FrameEnvelope, draining: bool) {
        let event = envelope.event();
        self.evaluate_alerts(event, draining);
        match self.session.apply(
            self.ledger,
            &self.account_id,
            event,
            envelope.received_at_ms(),
        ) {
            Ok(Action::None) => {}
            Ok(Action::Reconcile) if !draining => self.reconcile().await,
            // The queued lifecycle evidence is durable, but a new venue walk
            // belongs to the next startup after this owner has stopped.
            Ok(Action::Reconcile) => {}
            Err(error) => {
                // A fill the chain did not take, or a gap row the ledger
                // refused. Either way oppen's account of the position is now
                // short of the venue's, and the only honest posture is the one
                // a disconnect produces.
                tracing::error!(%error, "feed event not recorded; the account is not caught up");
                self.session
                    .record_failure(format!("feed event not recorded: {error}"));
            }
        }
        // Dropping without acknowledgment leaves the delivery monitor failed
        // closed. A retryable reconciliation failure leaves readiness false,
        // but its lifecycle event's durable application is still complete.
        if self.session.state().failure.is_none() {
            envelope.acknowledge();
        }
    }

    /// Answer the alerts this event can answer.
    ///
    /// Only two kinds of event carry a condition's reading, and they are
    /// matched by name rather than by a wildcard for the reason
    /// `FeedSession::apply` matches that way: a feed oppen starts consuming
    /// should be a decision about what it can answer.
    fn evaluate_alerts(&self, event: &WsEvent, draining: bool) {
        let now = now_ms();
        let fired = match event {
            // Not an alert condition: the quote cache is what `get_features`
            // reads `micro_tilt_bps` from, and this is the only place a frame
            // reaches it.
            WsEvent::Bbo {
                coin,
                venue_time_ms,
                bid,
                ask,
            } => {
                self.quotes.observe(oppen_hl::types::Bbo {
                    coin: coin.clone(),
                    time: *venue_time_ms,
                    bbo: [bid.clone(), ask.clone()],
                });
                return;
            }
            WsEvent::ActiveAssetCtx { coin, ctx, .. } => {
                let tick = MarketTick {
                    symbol: coin,
                    // The same liveness rule the order path uses: a published
                    // `markPx` on an asset the venue has stopped quoting is a
                    // frozen last print, and `ReferencePrices` refuses it
                    // there for the reason an alert must not fire on it here.
                    mark_px: ctx.mid_px_no_fallback().map(|_| ctx.mark_px),
                    funding_hour_to_date_bps: alert::funding_bps(ctx.funding),
                };
                alert::on_market_tick(self.alerts, &tick, now)
            }
            // A snapshot fill is the backlog the venue replays on subscribe.
            // Waking an agent for a fill it was already told about is a false
            // wakeup, and the alert would be spent on it.
            WsEvent::UserFills {
                is_snapshot: false,
                fills,
                ..
            } => alert::on_fills(self.alerts, fills, now),
            _ => return,
        };
        match fired {
            Ok(fired) if fired.is_empty() => {}
            Ok(fired) => self.chain(fired, now, draining),
            Err(error) => {
                tracing::error!(%error, "could not evaluate alerts");
                self.session
                    .record_failure(format!("could not evaluate alerts: {error}"));
            }
        }
    }

    /// Work every open window, and clear the flag only if none is outstanding.
    async fn reconcile(&self) {
        let stamp = self.session.unreconciled();
        // No pending cloids: item 19's settle-by-client-id needs the set of
        // orders the *gateway* believes live, which this has no access to. An
        // `orderUpdates` gap still re-reads the resting book, so what is
        // skipped is the query for orders the caller thought were live and the
        // venue is not resting.
        let outcomes = match self.reconciler.reconcile_all(&[]).await {
            Ok(outcomes) => outcomes,
            Err(error) => {
                tracing::error!(%error, "could not read the gap work list");
                self.record_reconcile_failure(&error);
                return;
            }
        };

        let mut outstanding = 0usize;
        for outcome in &outcomes {
            if let Some(receipt) = &outcome.fill_walk {
                self.session.record_fill_walk(&stamp, receipt.clone());
            }
            match &outcome.status {
                // Closed, or not this component's to close: `reconcile.rs`
                // leaves a market-data window for whoever owns candle
                // backfill, and it will never come off the work list. Waiting
                // on it would mean never trading again after the first
                // `l2Book` drop.
                GapStatus::Reconciled | GapStatus::NotAnAccountFeed => {}
                status => {
                    outstanding += 1;
                    if let GapStatus::Failed(error) = status {
                        self.record_reconcile_failure(error);
                    }
                    tracing::warn!(
                        gap_id = outcome.gap_id,
                        scope = outcome.scope,
                        ?status,
                        "gap still open"
                    );
                }
            }
        }

        if outstanding == 0 {
            self.session.reconciled(&stamp, now_ms_u64());
        }
    }

    fn record_reconcile_failure(&self, error: &ReconcileError) {
        // Venue/transport and pagination failures remain retryable. Missing or
        // malformed local evidence requires an operator restart, not a timer.
        if matches!(
            error,
            ReconcileError::Ledger(_)
                | ReconcileError::UnusableWindow { .. }
                | ReconcileError::UnreadableScope { .. }
        ) {
            self.session
                .record_failure(format!("reconcile local evidence failure: {error}"));
        }
    }
}

/// The host clock in the unsigned milliseconds [`FeedSession`] counts in.
///
/// Shares [`crate::ledger::now_ms`]'s reading, and its answer to a clock
/// before the epoch: zero, which reads as the stalest possible feed rather
/// than as a panic on the event path.
fn now_ms_u64() -> u64 {
    u64::try_from(now_ms()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use oppen_hl::Network;
    use oppen_hl::info::OrderRef;
    use oppen_hl::types::{Fill, OpenOrder, OrderStatusResponse, Side};
    use oppen_hl::ws::{ConnectionId, Disconnected, GapWindow, Reconnected};
    use rust_decimal::Decimal;
    use tempfile::TempDir;

    use super::*;
    use crate::alert::{Condition, Direction};
    use crate::ledger::EventKind;

    /// Records what the pump asked for instead of opening a socket.
    #[derive(Debug, Default)]
    struct Feeds {
        subscribed: Mutex<Vec<String>>,
        calls: AtomicUsize,
        shutdown: AtomicBool,
    }

    impl Feeds {
        fn keys(&self) -> Vec<String> {
            let mut keys = self.subscribed.lock().expect("feeds").clone();
            keys.sort();
            keys
        }
    }

    impl FeedSubscriber for Feeds {
        fn subscribe(&self, sub: Subscription) -> Result<(), PoolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.shutdown.load(Ordering::SeqCst) {
                return Err(PoolError::Shutdown);
            }
            self.subscribed.lock().expect("feeds").push(sub.key());
            Ok(())
        }

        fn unsubscribe(&self, sub: &Subscription) -> Result<(), PoolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.shutdown.load(Ordering::SeqCst) {
                return Err(PoolError::Shutdown);
            }
            self.subscribed
                .lock()
                .expect("feeds")
                .retain(|key| key != &sub.key());
            Ok(())
        }
    }

    fn alerts() -> AlertStore {
        AlertStore::open(":memory:").expect("alerts")
    }

    fn asset_ctx(coin: &str, mark: &str, mid: Option<&str>) -> WsEvent {
        let d = |s: &str| Decimal::from_str(s).expect("decimal");
        WsEvent::ActiveAssetCtx {
            coin: coin.into(),
            ctx: Box::new(oppen_hl::types::AssetCtx {
                funding: oppen_hl::types::HourToDateRate1h::from_hour_to_date_1h(Decimal::ZERO),
                open_interest: d("1000"),
                prev_day_px: d(mark),
                day_ntl_vlm: d("1"),
                premium: mid.map(|_| Decimal::ZERO),
                oracle_px: d(mark),
                mark_px: d(mark),
                mid_px: mid.map(d),
                impact_pxs: None,
            }),
            received_at_ms: now_ms_u64(),
        }
    }

    fn alert_rows(ledger: &Ledger) -> Vec<(String, serde_json::Value)> {
        ledger
            .get_events(0, 1_000)
            .expect("page")
            .events
            .into_iter()
            .filter(|event| event.kind == EventKind::Alert)
            .map(|event| {
                (
                    event.agent_id.unwrap_or_default(),
                    event.payload.unwrap_or_default(),
                )
            })
            .collect()
    }

    const ACCOUNT: &str = "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2";

    fn account() -> Address {
        ACCOUNT.parse().expect("address")
    }

    fn ledger(dir: &TempDir) -> Ledger {
        Ledger::open(dir.path(), Network::Testnet).expect("ledger")
    }

    fn fill(tid: u64, time_ms: u64) -> Fill {
        let d = |s: &str| Decimal::from_str(s).expect("decimal");
        Fill {
            coin: "BTC".into(),
            px: d("64000"),
            sz: d("0.01"),
            side: Side::B,
            time: time_ms,
            start_position: Decimal::ZERO,
            dir: "Open Long".into(),
            closed_pnl: Decimal::ZERO,
            hash: "0x00".into(),
            oid: 1,
            crossed: true,
            fee: d("0.02"),
            fee_token: "USDC".into(),
            builder_fee: None,
            tid,
            cloid: None,
        }
    }

    /// A venue that answers the three reads, and can be made to refuse them.
    ///
    /// Refusal is what the pump's own behaviour turns on — a window that could
    /// not be walked must leave the account unable to trade — so it is a state
    /// of the fixture rather than a separate double. The pump takes its source
    /// by value, so the switch and the counter are shared through a
    /// [`Controls`] the test keeps.
    #[derive(Debug)]
    struct FakeVenue {
        controls: Controls,
        gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    /// The half of [`FakeVenue`] a test still holds after the pump owns it.
    ///
    /// The fills live here rather than in the venue because a fill that prints
    /// *during* an outage is the whole point: a fixture fixed at construction
    /// would already hold it when the startup window is walked, and a test
    /// over it could not tell which window recovered it.
    #[derive(Debug, Clone)]
    struct Controls {
        fills: Arc<Mutex<Vec<Fill>>>,
        refusing: Arc<AtomicBool>,
        walks: Arc<AtomicUsize>,
        refusing_orders: Arc<AtomicBool>,
    }

    impl Controls {
        fn new(refusing: bool) -> Self {
            Controls {
                fills: Arc::new(Mutex::new(Vec::new())),
                refusing: Arc::new(AtomicBool::new(refusing)),
                walks: Arc::new(AtomicUsize::new(0)),
                refusing_orders: Arc::new(AtomicBool::new(false)),
            }
        }

        fn prints(&self, fill: Fill) {
            self.fills.lock().expect("fills").push(fill);
        }

        fn recover(&self) {
            self.refusing.store(false, Ordering::SeqCst);
        }

        fn walks(&self) -> usize {
            self.walks.load(Ordering::SeqCst)
        }
    }

    impl FakeVenue {
        fn holding(fills: Vec<Fill>) -> Self {
            let controls = Controls::new(false);
            for fill in fills {
                controls.prints(fill);
            }
            FakeVenue {
                controls,
                gate: None,
            }
        }

        fn refusing(controls: Controls) -> Self {
            FakeVenue {
                controls,
                gate: None,
            }
        }

        fn refused(&self) -> bool {
            self.controls.refusing.load(Ordering::SeqCst)
        }
    }

    fn unavailable() -> oppen_hl::Error {
        oppen_hl::Error::Venue {
            status: 503,
            message: "the venue is unavailable".into(),
        }
    }

    impl ReconcileSource for FakeVenue {
        fn network(&self) -> Network {
            Network::Testnet
        }

        async fn user_fills_by_time(
            &self,
            _user: Address,
            start_ms: u64,
            end_ms: Option<u64>,
        ) -> std::result::Result<Vec<Fill>, oppen_hl::Error> {
            if self.refused() {
                return Err(unavailable());
            }
            self.controls.walks.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.acquire().await.expect("test gate open").forget();
            }
            let mut rows: Vec<Fill> = self
                .controls
                .fills
                .lock()
                .expect("fills")
                .iter()
                .filter(|fill| {
                    fill.time >= start_ms && end_ms.is_none_or(|end_ms| fill.time <= end_ms)
                })
                .cloned()
                .collect();
            rows.sort_by_key(|fill| (fill.time, fill.tid));
            Ok(rows)
        }

        async fn frontend_open_orders(
            &self,
            _user: Address,
        ) -> std::result::Result<Vec<OpenOrder>, oppen_hl::Error> {
            if self.refused() || self.controls.refusing_orders.load(Ordering::SeqCst) {
                return Err(unavailable());
            }
            Ok(Vec::new())
        }

        async fn order_status(
            &self,
            _user: Address,
            _order: OrderRef,
        ) -> std::result::Result<OrderStatusResponse, oppen_hl::Error> {
            if self.refused() {
                return Err(unavailable());
            }
            Ok(OrderStatusResponse::UnknownOid)
        }
    }

    fn disconnected(at_ms: u64, last_message_ms: u64, owned: Vec<Subscription>) -> WsEvent {
        WsEvent::Disconnected(Box::new(Disconnected {
            connection: ConnectionId::new(0),
            at_ms,
            last_message_ms: Some(last_message_ms),
            subscriptions: owned,
            unacked: Vec::new(),
            reason: "1006 socket closed".into(),
        }))
    }

    fn reconnected(at_ms: u64, resubscribed: Vec<Subscription>) -> WsEvent {
        WsEvent::Reconnected(Box::new(Reconnected {
            connection: ConnectionId::new(0),
            at_ms,
            gap: GapWindow {
                start_ms: at_ms - 30_000,
                end_ms: at_ms,
            },
            resubscribed,
            attempts: 1,
        }))
    }

    fn gaps(ledger: &Ledger) -> BTreeMap<String, (bool, bool)> {
        ledger
            .unreconciled_gaps()
            .expect("gaps")
            .into_iter()
            .map(|gap| {
                (
                    gap.scope,
                    (gap.closed_ts_ms.is_some(), gap.reconciled_ts_ms.is_some()),
                )
            })
            .collect()
    }

    fn fill_rows(ledger: &Ledger) -> usize {
        ledger
            .get_events(0, 1_000)
            .expect("page")
            .events
            .iter()
            .filter(|event| event.kind == EventKind::Fill)
            .count()
    }

    /// Exercise real startup and durable handling while retaining a live
    /// scripted stream. Shutdown-loop ownership has separate tests below.
    async fn drive(
        pump: &FeedPump<'_, FakeVenue, Feeds>,
        script: Vec<WsEvent>,
    ) -> crate::feed::TestIngress {
        let mut ingress = crate::feed::test_ingress(pump.session);
        let count = script.len();
        for event in script {
            ingress
                .sender
                .as_ref()
                .unwrap()
                .send(event, now_ms_u64())
                .await
                .expect("send");
        }
        pump.catch_up().await;
        pump.sync_alert_feeds();
        for _ in 0..count {
            pump.handle(
                ingress.receiver.as_mut().unwrap().recv().await.unwrap(),
                false,
            )
            .await;
        }
        ingress
    }

    async fn handle_one(
        pump: &FeedPump<'_, FakeVenue, Feeds>,
        event: WsEvent,
        ingress: &mut crate::feed::TestIngress,
    ) {
        ingress
            .sender
            .as_ref()
            .unwrap()
            .send(event, now_ms_u64())
            .await
            .unwrap();
        pump.handle(
            ingress.receiver.as_mut().unwrap().recv().await.unwrap(),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn initial_fill_receipt_survives_other_gap_retry_but_not_monitor_replacement() {
        let dir = TempDir::new().unwrap();
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let controls = Controls::new(false);
        controls.refusing_orders.store(true, Ordering::SeqCst);
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::refusing(controls.clone()),
            &alerts,
            &quotes,
            &feeds,
        )
        .unwrap();
        let mut ingress = crate::feed::test_ingress(&session);
        pump.catch_up().await;
        assert!(!session.state().reconciled);
        let receipt = session.lock().initial_fill_walk.clone().unwrap();
        assert!(receipt.initial_window);
        assert!(receipt.terminated_by_short_page);
        assert_eq!(receipt.requested_end_ms, None);
        assert_eq!(receipt.pages, 1);
        assert_eq!(controls.walks(), 1);
        controls.refusing_orders.store(false, Ordering::SeqCst);
        pump.reconcile().await;
        assert!(session.state().reconciled);
        assert_eq!(
            controls.walks(),
            1,
            "healed fills gap is no longer on retry work list"
        );
        assert_eq!(session.lock().initial_fill_walk.as_ref(), Some(&receipt));
        ingress.sender.take();
        let mut receiver = ingress.receiver.take().unwrap();
        assert!(receiver.recv().await.is_none());
        receiver.complete().unwrap();
        let _replacement = crate::feed::test_ingress(&session);
        assert!(session.lock().initial_fill_walk.is_none());
        let stale = session.stamp();
        session.unreconciled();
        session.record_fill_walk(&stale, receipt);
        assert!(session.lock().initial_fill_walk.is_none());
    }

    #[tokio::test]
    async fn shutdown_closes_retained_sender_and_drains_ledger_events_without_new_work() {
        for dropped in [false, true] {
            let dir = TempDir::new().expect("tempdir");
            let ledger = ledger(&dir);
            let session = FeedSession::new();
            let alerts = alerts();
            let quotes = QuoteCache::new();
            let feeds = Feeds::default();
            let controls = Controls::new(false);
            alerts
                .arm(
                    "agent-a",
                    &Condition::PriceCross {
                        symbol: "BTC".into(),
                        direction: Direction::Above,
                        px: Decimal::ONE,
                    },
                    now_ms(),
                )
                .unwrap();
            alerts
                .arm(
                    "agent-a",
                    &Condition::PriceCross {
                        symbol: "ETH".into(),
                        direction: Direction::Above,
                        px: Decimal::ONE,
                    },
                    now_ms(),
                )
                .unwrap();
            quotes.lease("SOL", now_ms_u64());
            let pump = FeedPump::new(
                &session,
                &ledger,
                account(),
                FakeVenue::refusing(controls.clone()),
                &alerts,
                &quotes,
                &feeds,
            )
            .unwrap();
            let (tx, mut rx) = oppen_hl::ws::event_channel(8);
            let now = now_ms_u64();
            let scope = Subscription::UserFills { user: account() };
            tx.send(
                disconnected(now - 30_000, now - 30_000, vec![scope.clone()]),
                now_ms_u64(),
            )
            .await
            .unwrap();
            tx.send(reconnected(now, vec![scope]), now_ms_u64())
                .await
                .unwrap();
            for tid in [1, 1, 2] {
                tx.send(
                    WsEvent::UserFills {
                        user: account(),
                        is_snapshot: false,
                        fills: vec![fill(tid, now)],
                    },
                    now_ms_u64(),
                )
                .await
                .unwrap();
            }
            tx.send(asset_ctx("BTC", "64000", Some("64000")), now_ms_u64())
                .await
                .unwrap();
            let (stop, shutdown) = tokio::sync::oneshot::channel();
            let (_request, quiesce) = tokio::sync::oneshot::channel();
            if dropped {
                drop(stop);
            } else {
                stop.send(()).unwrap();
            }
            tokio::time::timeout(
                Duration::from_secs(2),
                pump.run_until_shutdown(&mut rx, quiesce, shutdown),
            )
            .await
            .unwrap();
            assert!(rx.is_closed());
            assert!(
                tx.send(bbo_frame("BTC", "99", "101"), now_ms_u64())
                    .await
                    .is_err()
            );
            assert_eq!(
                fill_rows(&ledger),
                2,
                "queued fills persisted and deduplicated"
            );
            assert_eq!(alert_rows(&ledger).len(), 1, "queued alert still chained");
            assert!(ledger.verify().unwrap().is_intact());
            assert!(!session.state().reconciled);
            assert!(
                !gaps(&ledger).is_empty(),
                "reconnect evidence retained for next startup"
            );
            assert_eq!(controls.walks(), 0, "drain never starts a remote walk");
            assert!(
                feeds.keys().is_empty(),
                "neither startup nor queued alerts subscribe during drain"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_active_reconcile_after_observer_drops_then_drains_queue() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let controls = Controls::new(false);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let source = FakeVenue {
            controls: controls.clone(),
            gate: Some(gate.clone()),
        };
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            source,
            &alerts,
            &quotes,
            &feeds,
        )
        .unwrap();
        let (tx, mut rx) = oppen_hl::ws::event_channel(2);
        tx.send(
            WsEvent::UserFills {
                user: account(),
                is_snapshot: false,
                fills: vec![fill(7, now_ms_u64())],
            },
            now_ms_u64(),
        )
        .await
        .unwrap();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let (request, quiesce) = tokio::sync::oneshot::channel();
        let (ack, mut acknowledged) = tokio::sync::oneshot::channel();
        // Retain the actual run future; timing out only drops the observer's
        // borrow. Production retains its owning task and completion handle.
        let run = pump.run_until_shutdown(&mut rx, quiesce, shutdown);
        tokio::pin!(run);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut run)
                .await
                .is_err()
        );
        assert_eq!(controls.walks(), 1);
        assert_eq!(fill_rows(&ledger), 0);
        request.send(ack).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut run)
                .await
                .is_err(),
            "shutdown does not cancel in-progress reconciliation"
        );
        assert_eq!(fill_rows(&ledger), 0);
        assert!(
            matches!(
                acknowledged.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "active reconciliation prevents acknowledgment"
        );
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut acknowledged => result.unwrap(),
                () = &mut run => panic!("pump ended before producer completion"),
            }
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), &mut run)
            .await
            .unwrap();
        assert_eq!(fill_rows(&ledger), 1);
        assert!(ledger.verify().unwrap().is_intact());
        assert_eq!(
            controls.walks(),
            1,
            "no timed retry or second walk after shutdown"
        );
        assert!(
            tx.is_closed(),
            "retained outer sender does not prevent completion"
        );
        assert!(feeds.keys().is_empty());
    }

    #[tokio::test]
    async fn quiesced_demand_changes_do_not_touch_stopping_pool_while_events_keep_folding() {
        for dropped_request in [false, true] {
            let dir = TempDir::new().unwrap();
            let ledger = ledger(&dir);
            let session = FeedSession::new();
            let alerts = alerts();
            let quotes = QuoteCache::new();
            let feeds = Feeds::default();
            let controls = Controls::new(false);
            let pump = FeedPump::new(
                &session,
                &ledger,
                account(),
                FakeVenue::refusing(controls.clone()),
                &alerts,
                &quotes,
                &feeds,
            )
            .unwrap();
            let (tx, mut rx) = oppen_hl::ws::event_channel(1);
            let (request, quiesce) = tokio::sync::oneshot::channel();
            let (ack, acknowledged) = tokio::sync::oneshot::channel();
            let (done, producers_done) = tokio::sync::oneshot::channel();
            if dropped_request {
                drop(request);
                drop(ack);
            } else {
                request.send(ack).unwrap();
            }
            let driver = async {
                if !dropped_request {
                    acknowledged.await.unwrap();
                }
                // The real owner starts pool shutdown only after the ack.
                // This fake returns Shutdown for any accidental later call.
                feeds.shutdown.store(true, Ordering::SeqCst);
                for symbol in ["BTC", "ETH"] {
                    alerts
                        .arm(
                            "agent-a",
                            &Condition::PriceCross {
                                symbol: symbol.into(),
                                direction: Direction::Above,
                                px: Decimal::ONE,
                            },
                            now_ms(),
                        )
                        .unwrap();
                }
                quotes.lease("SOL", now_ms_u64());
                tx.send(
                    WsEvent::UserFills {
                        user: account(),
                        is_snapshot: false,
                        fills: vec![fill(51, now_ms_u64())],
                    },
                    now_ms_u64(),
                )
                .await
                .unwrap();
                tx.send(asset_ctx("BTC", "64000", Some("64000")), now_ms_u64())
                    .await
                    .unwrap();
                tx.send(bbo_frame("SOL", "99", "101"), now_ms_u64())
                    .await
                    .unwrap();
                // Receipt of the final quote proves the preceding ledger and
                // alert folds ran before producers_done was signaled.
                tokio::time::timeout(Duration::from_secs(2), async {
                    while quotes.peek("SOL").is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(fill_rows(&ledger), 1);
                assert_eq!(alert_rows(&ledger).len(), 1);
                assert_eq!(feeds.calls.load(Ordering::SeqCst), 0);
                assert!(session.state().failure.is_none());
                assert!(
                    !tx.is_closed(),
                    "consumer remains alive during producer drain"
                );
                tx.send(
                    WsEvent::UserFills {
                        user: account(),
                        is_snapshot: false,
                        fills: vec![fill(52, now_ms_u64())],
                    },
                    now_ms_u64(),
                )
                .await
                .unwrap();
                done.send(()).unwrap();
            };
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    pump.run_until_shutdown(&mut rx, quiesce, producers_done),
                    driver
                );
            })
            .await
            .unwrap();
            assert_eq!(fill_rows(&ledger), 2);
            assert_eq!(feeds.calls.load(Ordering::SeqCst), 0);
            assert_eq!(controls.walks(), 0);
            assert!(session.state().failure.is_none());
            assert!(ledger.verify().unwrap().is_intact());
        }
    }

    #[tokio::test]
    async fn persistence_failure_survives_empty_reconcile_and_completed_drain() {
        for target in ["startup", "fill", "alert"] {
            let dir = TempDir::new().unwrap();
            let ledger = ledger(&dir);
            let session = FeedSession::new();
            let alerts = alerts();
            let quotes = QuoteCache::new();
            let feeds = Feeds::default();
            if target == "alert" {
                alerts
                    .arm("agent-a", &Condition::Fill { symbol: None }, now_ms())
                    .unwrap();
            }
            let sql =
                rusqlite::Connection::open(dir.path().join(crate::db_file_name(Network::Testnet)))
                    .unwrap();
            let trigger = match target {
                "startup" => {
                    "CREATE TRIGGER fail_pump BEFORE INSERT ON feed_gaps BEGIN SELECT RAISE(ABORT, 'synthetic startup write failure'); END"
                }
                "fill" => {
                    "CREATE TRIGGER fail_pump BEFORE INSERT ON events WHEN NEW.kind = 'fill' BEGIN SELECT RAISE(ABORT, 'synthetic fill write failure'); END"
                }
                "alert" => {
                    "CREATE TRIGGER fail_pump BEFORE INSERT ON events WHEN NEW.kind = 'alert' BEGIN SELECT RAISE(ABORT, 'synthetic alert write failure'); END"
                }
                _ => unreachable!(),
            };
            sql.execute_batch(trigger).unwrap();
            let pump = FeedPump::new(
                &session,
                &ledger,
                account(),
                FakeVenue::holding(Vec::new()),
                &alerts,
                &quotes,
                &feeds,
            )
            .unwrap();
            let _ingress = crate::feed::test_ingress(&session);
            pump.catch_up().await;
            assert!(
                gaps(&ledger).is_empty(),
                "startup refusal must not manufacture a gap"
            );
            drop(_ingress);
            let (tx, mut rx) = oppen_hl::ws::event_channel(1);
            tx.send(
                WsEvent::UserFills {
                    user: account(),
                    is_snapshot: false,
                    fills: vec![fill(41, now_ms_u64())],
                },
                now_ms_u64(),
            )
            .await
            .unwrap();
            let (stop, shutdown) = tokio::sync::oneshot::channel();
            let (_request, quiesce) = tokio::sync::oneshot::channel();
            stop.send(()).unwrap();
            tokio::time::timeout(
                Duration::from_secs(2),
                pump.run_until_shutdown(&mut rx, quiesce, shutdown),
            )
            .await
            .unwrap();
            assert!(
                tx.is_closed(),
                "failed application still finishes actual drain"
            );
            let failure = session
                .state()
                .failure
                .expect("drain exposes persistent local failure");
            assert!(failure.contains("synthetic"), "{target}: {failure}");
            assert!(!session.state().reconciled);
            assert_eq!(fill_rows(&ledger), usize::from(target != "fill"));
            sql.execute_batch("DROP TRIGGER fail_pump").unwrap();
            pump.reconcile().await;
            assert!(gaps(&ledger).is_empty());
            assert_eq!(session.state().failure.as_deref(), Some(failure.as_str()));
            assert!(
                !session.state().reconciled,
                "empty reconcile cannot bless lost {target} evidence"
            );
            assert!(ledger.verify().unwrap().is_intact());
        }
    }

    #[tokio::test]
    async fn retryable_venue_failure_clears_previous_readiness_without_permanent_latch() {
        let dir = TempDir::new().unwrap();
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let controls = Controls::new(false);
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::refusing(controls.clone()),
            &alerts,
            &quotes,
            &feeds,
        )
        .unwrap();
        let _ingress = crate::feed::test_ingress(&session);
        pump.catch_up().await;
        assert!(session.state().reconciled);
        let gap = ledger
            .open_gap(&account_scopes(account())[0], now_ms(), None)
            .unwrap();
        ledger.close_gap(gap.gap_id, now_ms()).unwrap();
        controls.refusing.store(true, Ordering::SeqCst);
        pump.reconcile().await;
        assert!(!session.state().reconciled);
        assert!(session.state().failure.is_none());
        controls.recover();
        pump.reconcile().await;
        assert!(session.state().reconciled);
        assert!(session.state().failure.is_none());
    }

    #[tokio::test]
    async fn unreadable_persisted_scope_stays_failed_after_work_list_resolution_and_tick() {
        for scope in ["userFills:not-an-address", "orderUpdates:not-an-address"] {
            let dir = TempDir::new().unwrap();
            let ledger = ledger(&dir);
            let session = FeedSession::new();
            let alerts = alerts();
            let quotes = QuoteCache::new();
            let feeds = Feeds::default();
            let pump = FeedPump::new(
                &session,
                &ledger,
                account(),
                FakeVenue::holding(Vec::new()),
                &alerts,
                &quotes,
                &feeds,
            )
            .unwrap();
            let mut ingress = crate::feed::test_ingress(&session);
            pump.catch_up().await;
            assert!(session.state().reconciled);
            let gap = ledger.open_gap(scope, now_ms(), None).unwrap();
            ledger.close_gap(gap.gap_id, now_ms()).unwrap();
            pump.reconcile().await;
            let failure = session
                .state()
                .failure
                .expect("malformed account scope is local evidence failure");
            assert!(failure.contains(scope));
            assert!(failure.contains("does not name a valid address"));
            assert!(!session.state().reconciled);
            // Removing the malformed entry from pending work is not permission
            // to clear the already observed failure in this session.
            ledger.mark_gap_reconciled(gap.gap_id, now_ms()).unwrap();
            assert!(gaps(&ledger).is_empty());
            pump.reconcile().await;
            handle_one(&pump, bbo_frame("BTC", "99", "101"), &mut ingress).await;
            assert!(quotes.peek("BTC").is_some(), "later data still applies");
            assert!(session.state().last_tick_ms.is_some());
            assert_eq!(session.state().failure.as_deref(), Some(failure.as_str()));
            assert!(!session.state().reconciled);
            assert!(ledger.verify().unwrap().is_intact());
        }
    }

    /// The pool emits `Reconnected` only in answer to a `Disconnected`, so a
    /// clean first connect produces no event at all. Without a startup window
    /// the account would stay unreconciled for the life of the process and
    /// every order would be refused — which is the refusal this whole module
    /// exists to lift.
    #[tokio::test]
    async fn a_fresh_pump_is_reconciled_before_it_has_seen_an_event() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        assert!(!session.state().reconciled, "nothing has been checked yet");
        let _ingress = drive(&pump, Vec::new()).await;

        assert!(session.state().reconciled);
        assert!(
            gaps(&ledger).is_empty(),
            "the startup window is closed and reconciled, so it leaves the work list"
        );
    }

    /// The startup window is walked against the venue, not assumed empty: a
    /// fill that landed while oppen was not running is in the chain by the
    /// time the first order can be placed.
    #[tokio::test]
    async fn the_startup_window_recovers_fills_from_before_the_process_started() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let missed = now_ms_u64() - 60_000;
        let venue = FakeVenue::holding(vec![fill(11, missed), fill(12, missed + 10)]);
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            venue,
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(&pump, Vec::new()).await;

        assert_eq!(fill_rows(&ledger), 2);
        assert!(session.state().reconciled);
    }

    /// **The P2 gate, end to end.** A socket drops, two fills print while it
    /// is down, and the socket comes back. Neither was ever delivered live,
    /// and both are in the chain once the reconnect has been worked.
    ///
    /// The two phases are deliberate. The startup window is walked first,
    /// against a venue that holds only the earlier fill — so the chain has an
    /// anchor and the outage fills do not yet exist. Only then do they print.
    /// Handing the events to a second `run` instead would open a second
    /// startup window that recovered them on its own, and the test would pass
    /// with the disconnect window never written.
    #[tokio::test]
    async fn no_fill_is_lost_across_a_disconnect() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let dropped_at = now_ms_u64();
        let venue = FakeVenue::holding(vec![fill(20, dropped_at - 60_000)]);
        let controls = venue.controls.clone();
        let user_fills = Subscription::UserFills { user: account() };
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            venue,
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let mut ingress = drive(&pump, Vec::new()).await;
        assert_eq!(fill_rows(&ledger), 1, "the chain now has an anchor");

        controls.prints(fill(21, dropped_at + 5_000));
        controls.prints(fill(22, dropped_at + 20_000));
        handle_one(
            &pump,
            disconnected(dropped_at, dropped_at, vec![user_fills.clone()]),
            &mut ingress,
        )
        .await;
        assert!(!session.state().reconciled, "the drop refuses new orders");
        handle_one(
            &pump,
            reconnected(dropped_at + 30_000, vec![user_fills]),
            &mut ingress,
        )
        .await;

        assert_eq!(fill_rows(&ledger), 3, "both fills from inside the outage");
        assert!(session.state().reconciled);
        assert!(gaps(&ledger).is_empty(), "every window closed and worked");
    }

    /// A feed the pool could not bring back is still down, and closing its
    /// window would hand the reconciler an end instant for an outage that has
    /// not ended.
    #[tokio::test]
    async fn a_feed_that_did_not_resume_keeps_its_window_open() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let at = now_ms_u64();
        let user_fills = Subscription::UserFills { user: account() };
        let book = Subscription::L2Book { coin: "BTC".into() };
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(
            &pump,
            vec![
                disconnected(at, at, vec![user_fills.clone(), book.clone()]),
                // The book feed was quarantined and never came back.
                reconnected(at + 30_000, vec![user_fills]),
            ],
        )
        .await;

        let open = gaps(&ledger);
        assert_eq!(
            open.get(&book.key()),
            Some(&(false, false)),
            "the book window is still open"
        );
        assert!(
            !open.contains_key(&Subscription::UserFills { user: account() }.key()),
            "the fills window closed and was worked"
        );
        assert!(
            session.state().reconciled,
            "a market-data window is not this component's to close, so waiting \
             on it would mean never trading again after the first l2Book drop"
        );
    }

    /// A window that could not be walked leaves the account unable to trade.
    /// The alternative — treating a venue error as "nothing to recover" — marks
    /// the gate passed on the strength of a failed request.
    #[tokio::test]
    async fn a_venue_that_refuses_the_walk_leaves_the_account_unreconciled() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let venue = FakeVenue::refusing(Controls::new(true));
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            venue,
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(&pump, Vec::new()).await;

        assert!(!session.state().reconciled);
        assert!(
            !gaps(&ledger).is_empty(),
            "the window stays on the work list, so item 34's overlay stays up"
        );
    }

    /// The retry is on a timer rather than on the next event: an account that
    /// went quiet at the wrong moment must not stay refused until the market
    /// moves again.
    #[tokio::test(start_paused = true)]
    async fn an_unreconciled_account_retries_without_a_new_event() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let controls = Controls::new(true);
        let venue = FakeVenue::refusing(controls.clone());
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            venue,
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let (tx, mut rx) = oppen_hl::ws::event_channel(1);
        let driver = async {
            // The startup walk has failed by now, and nothing further arrives.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(!session.state().reconciled);
            controls.recover();
            tokio::time::sleep(RETRY_INTERVAL * 2).await;
            assert!(
                session.state().reconciled,
                "the timer came back to it while the stream was owned"
            );
            drop(tx);
        };
        tokio::join!(pump.run(&mut rx), driver);

        assert!(
            !session.state().reconciled,
            "a stopped pump is not order admission"
        );
        rx.complete().unwrap();
        assert!(
            controls.walks() >= 1,
            "the retry actually walked the window"
        );
        assert!(gaps(&ledger).is_empty());
    }

    /// The end-to-end shape of item 22: an agent arms a level, the market
    /// reaches it, and the agent has a row waiting on its next `get_events`.
    #[tokio::test]
    async fn a_price_cross_wakes_the_agent_that_armed_it() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "BTC".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("70000").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(
            &pump,
            vec![
                asset_ctx("BTC", "69999", Some("69999")),
                asset_ctx("BTC", "70001", Some("70001")),
            ],
        )
        .await;

        let rows = alert_rows(&ledger);
        assert_eq!(rows.len(), 1, "one wakeup, on the tick that reached it");
        assert_eq!(rows[0].0, "agent-a", "attributed, so C6 delivers it");
        assert_eq!(rows[0].1["observed"]["mark_px"], "70001");
    }

    /// A price alert on a symbol nothing is watching is an alert that never
    /// fires. The pump is the only thing that can subscribe it, so arming has
    /// to reach the pool.
    #[tokio::test]
    async fn arming_a_price_alert_subscribes_its_market_feed() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "ETH".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("4000").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(&pump, Vec::new()).await;

        assert_eq!(
            feeds.keys(),
            ["activeAssetCtx:ETH"],
            "activeAssetCtx, which carries both the mark and the funding"
        );
    }

    /// A fired alert is the last thing holding its feed, so the socket goes
    /// back. Otherwise a session that armed a hundred levels over a day ends
    /// it subscribed to a hundred symbols nothing is watching.
    #[tokio::test]
    async fn a_fired_alert_gives_its_feed_back() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "BTC".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("1").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(&pump, vec![asset_ctx("BTC", "64000", Some("64000"))]).await;

        assert_eq!(alert_rows(&ledger).len(), 1);
        assert!(
            feeds.keys().is_empty(),
            "nothing is watching BTC any more, so the feed is dropped"
        );
    }

    /// The venue keeps publishing a `markPx` for an asset it has stopped
    /// quoting. Waking an agent on that frozen print is the fault
    /// `ReferencePrices` refuses on the order path, and it is refused here for
    /// the same reason.
    #[tokio::test]
    async fn an_unquoted_market_does_not_wake_anyone() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "FRIEND".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("1").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        // markPx 4.72 with a null mid: the measured FRIEND case.
        let _ingress = drive(&pump, vec![asset_ctx("FRIEND", "4.72", None)]).await;

        assert!(alert_rows(&ledger).is_empty());
    }

    /// The venue replays a backlog of fills on subscribe. An agent woken for a
    /// fill it was already told about has spent its alert on nothing.
    #[tokio::test]
    async fn the_subscribe_backlog_does_not_wake_a_fill_alert() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm("agent-a", &Condition::Fill { symbol: None }, now_ms())
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let backlog = WsEvent::UserFills {
            user: account(),
            is_snapshot: true,
            fills: vec![fill(1, now_ms_u64())],
        };
        let live = WsEvent::UserFills {
            user: account(),
            is_snapshot: false,
            fills: vec![fill(2, now_ms_u64())],
        };
        let _ingress = drive(&pump, vec![backlog, live]).await;

        let rows = alert_rows(&ledger);
        assert_eq!(rows.len(), 1, "the live fill, not the replayed one");
        assert_eq!(rows[0].1["observed"]["tid"], 2);
    }

    /// The deadlock G4 exists to break: an alert armed while the market is
    /// quiet must not wait for a tick on a feed nothing has subscribed, on a
    /// symbol nothing will subscribe until something ticks.
    #[tokio::test(start_paused = true)]
    async fn an_alert_armed_while_running_gets_its_feed_without_a_tick() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let (tx, mut rx) = oppen_hl::ws::event_channel(1);
        let driver = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(feeds.keys().is_empty(), "nothing armed, nothing subscribed");

            alerts
                .arm(
                    "agent-a",
                    &Condition::PriceCross {
                        symbol: "SOL".into(),
                        direction: Direction::Above,
                        px: Decimal::from_str("200").expect("decimal"),
                    },
                    now_ms(),
                )
                .expect("arm");

            // No event is ever sent on the channel. The only thing that can
            // subscribe the feed is the arming itself.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                feeds.keys(),
                ["activeAssetCtx:SOL"],
                "arming woke the pump and it subscribed the feed"
            );
            drop(tx);
        };
        tokio::join!(pump.run(&mut rx), driver);
    }

    /// Cancelling releases the feed the alert was holding, and does it without
    /// waiting for a tick — the same wake path arming uses, in reverse.
    /// Without it a cancelled symbol keeps a socket open until something else
    /// happens to move the armed set.
    #[tokio::test(start_paused = true)]
    async fn cancelling_an_alert_releases_its_feed_without_a_tick() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let armed = alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "SOL".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("200").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let (tx, mut rx) = oppen_hl::ws::event_channel(1);
        let driver = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(feeds.keys(), ["activeAssetCtx:SOL"], "armed, so subscribed");

            assert!(alerts.cancel("agent-a", armed.alert_id).expect("cancel"));

            // No event is ever sent. The cancel is the only thing that can
            // release the feed.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(
                feeds.keys().is_empty(),
                "cancelling woke the pump and it dropped the feed"
            );
            drop(tx);
        };
        tokio::join!(pump.run(&mut rx), driver);
    }

    fn bbo_frame(coin: &str, bid: &str, ask: &str) -> WsEvent {
        let d = |v: &str| Decimal::from_str(v).expect("decimal");
        let level = |px: &str| oppen_hl::types::Level {
            px: d(px),
            sz: Decimal::ONE,
            n: 1,
        };
        WsEvent::Bbo {
            coin: coin.into(),
            venue_time_ms: now_ms_u64(),
            bid: Some(level(bid)),
            ask: Some(level(ask)),
        }
    }

    /// The only path a `bbo` frame takes into `get_features`.
    #[tokio::test]
    async fn a_quote_frame_reaches_the_cache() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(&pump, vec![bbo_frame("BTC", "99", "101")]).await;

        assert!(quotes.peek("BTC").is_some(), "the frame is readable");
    }

    /// A `bbo` frame is not an alert condition. It must not fire a price
    /// cross, which reads `activeAssetCtx`'s mark and its liveness gate.
    #[tokio::test]
    async fn a_quote_frame_does_not_fire_a_price_alert() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "BTC".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("1").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let _ingress = drive(&pump, vec![bbo_frame("BTC", "99", "101")]).await;

        assert!(
            alert_rows(&ledger).is_empty(),
            "a quote is not the mark a price cross is defined against"
        );
    }

    /// Leasing a symbol has to reach the pool, and it has to reach it as
    /// `bbo` — the alert path's `activeAssetCtx` carries no top of book.
    #[tokio::test(start_paused = true)]
    async fn leasing_a_quote_subscribes_bbo_and_expiry_gives_it_back() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let (tx, mut rx) = oppen_hl::ws::event_channel(1);
        let driver = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(
                feeds.keys().is_empty(),
                "nothing leased, nothing subscribed"
            );

            quotes.lease("SOL", now_ms_u64());
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                feeds.keys(),
                ["bbo:SOL"],
                "bbo, not activeAssetCtx: only bbo carries the top of book"
            );

            // Nobody renews it, so the lease expires. The expiry is stamped
            // on the system clock while `start_paused` only moves tokio's, so
            // the sweep is asked for directly here rather than waited out —
            // `QuoteCache::leased` is what expires an entry, and
            // `a_lease_nobody_renews_expires` covers the clock arithmetic.
            let ttl_ms = crate::features::quotes::LEASE_TTL.as_millis() as u64;
            assert!(
                quotes.leased(now_ms_u64() + ttl_ms).is_empty(),
                "the lease is past its ttl"
            );

            // The pump's own pass is what turns that into an unsubscribe, and
            // it runs on the retry tick whether or not anything needs
            // reconciling.
            tokio::time::sleep(RETRY_INTERVAL * 2).await;
            assert!(feeds.keys().is_empty(), "an expired lease drops its feed");
            drop(tx);
        };
        tokio::join!(pump.run(&mut rx), driver);
    }

    /// An alert and a feature call on the same symbol want different channels,
    /// and holding one must not be read as holding the other.
    #[tokio::test(start_paused = true)]
    async fn an_alert_and_a_lease_on_one_symbol_hold_both_feeds() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let alerts = alerts();
        let quotes = QuoteCache::new();
        let feeds = Feeds::default();
        alerts
            .arm(
                "agent-a",
                &Condition::PriceCross {
                    symbol: "BTC".into(),
                    direction: Direction::Above,
                    px: Decimal::from_str("999999").expect("decimal"),
                },
                now_ms(),
            )
            .expect("arm");
        let pump = FeedPump::new(
            &session,
            &ledger,
            account(),
            FakeVenue::holding(Vec::new()),
            &alerts,
            &quotes,
            &feeds,
        )
        .expect("pump");

        let (tx, mut rx) = oppen_hl::ws::event_channel(1);
        let driver = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            quotes.lease("BTC", now_ms_u64());
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(feeds.keys(), ["activeAssetCtx:BTC", "bbo:BTC"]);
            drop(tx);
        };
        tokio::join!(pump.run(&mut rx), driver);
    }
}
