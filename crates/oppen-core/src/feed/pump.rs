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
use oppen_hl::ws::{Subscription, WsEvent};
use tokio::sync::mpsc::Receiver;

use crate::feed::{Action, FeedSession};
use crate::ledger::{Ledger, now_ms};
use crate::reconcile::{GapStatus, ReconcileError, ReconcileSource, Reconciler};

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
pub struct FeedPump<'a, S> {
    session: &'a FeedSession,
    ledger: &'a Ledger,
    account: Address,
    /// `account` as [`crate::feed::FeedSession::apply`] wants it. Held rather
    /// than formatted per event, which would allocate on every tick.
    account_id: String,
    reconciler: Reconciler<'a, S>,
}

impl<'a, S: ReconcileSource> FeedPump<'a, S> {
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
    ) -> Result<Self, ReconcileError> {
        Ok(FeedPump {
            session,
            ledger,
            account,
            account_id: account.to_string(),
            reconciler: Reconciler::new(ledger, source)?,
        })
    }

    /// Catch up, then fold events until the pool is gone.
    ///
    /// Returns when `events` closes, which is what dropping the
    /// [`oppen_hl::ws::WsPool`] or calling its `shutdown` does.
    pub async fn run(&self, events: &mut Receiver<WsEvent>) {
        self.catch_up().await;
        let mut retry = tokio::time::interval(RETRY_INTERVAL);
        // The first tick of a tokio interval is immediate, and `catch_up` has
        // just done that work.
        retry.tick().await;
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.handle(&event).await,
                    None => return,
                },
                _ = retry.tick() => {
                    if !self.session.state().reconciled {
                        self.reconcile().await;
                    }
                }
            }
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
                // Left unreconciled, so the retry timer comes back to it.
                Err(error) => {
                    tracing::error!(%error, scope, "could not open the startup window");
                }
            }
        }
        self.reconcile().await;
    }

    async fn handle(&self, event: &WsEvent) {
        match self
            .session
            .apply(self.ledger, &self.account_id, event, now_ms_u64())
        {
            Ok(Action::None) => {}
            Ok(Action::Reconcile) => self.reconcile().await,
            Err(error) => {
                // A fill the chain did not take, or a gap row the ledger
                // refused. Either way oppen's account of the position is now
                // short of the venue's, and the only honest posture is the one
                // a disconnect produces.
                tracing::error!(%error, "feed event not recorded; the account is not caught up");
                self.session.unreconciled();
            }
        }
    }

    /// Work every open window, and clear the flag only if none is outstanding.
    async fn reconcile(&self) {
        // No pending cloids: item 19's settle-by-client-id needs the set of
        // orders the *gateway* believes live, which this has no access to. An
        // `orderUpdates` gap still re-reads the resting book, so what is
        // skipped is the query for orders the caller thought were live and the
        // venue is not resting.
        let outcomes = match self.reconciler.reconcile_all(&[]).await {
            Ok(outcomes) => outcomes,
            Err(error) => {
                tracing::error!(%error, "could not read the gap work list");
                return;
            }
        };

        let mut outstanding = 0usize;
        for outcome in &outcomes {
            match &outcome.status {
                // Closed, or not this component's to close: `reconcile.rs`
                // leaves a market-data window for whoever owns candle
                // backfill, and it will never come off the work list. Waiting
                // on it would mean never trading again after the first
                // `l2Book` drop.
                GapStatus::Reconciled | GapStatus::NotAnAccountFeed => {}
                status => {
                    outstanding += 1;
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
            self.session.reconciled(now_ms_u64());
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
    use tokio::sync::mpsc;

    use super::*;
    use crate::ledger::EventKind;

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
    }

    impl Controls {
        fn new(refusing: bool) -> Self {
            Controls {
                fills: Arc::new(Mutex::new(Vec::new())),
                refusing: Arc::new(AtomicBool::new(refusing)),
                walks: Arc::new(AtomicUsize::new(0)),
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
            FakeVenue { controls }
        }

        fn refusing(controls: Controls) -> Self {
            FakeVenue { controls }
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
            if self.refused() {
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

    /// Drive the pump over a fixed script of events and return once it stops.
    ///
    /// The channel is closed after the last one, which is what ends
    /// [`FeedPump::run`] — the same thing dropping the pool does.
    async fn drive(pump: &FeedPump<'_, FakeVenue>, script: Vec<WsEvent>) {
        let (tx, mut rx) = mpsc::channel(16);
        for event in script {
            tx.send(event).await.expect("send");
        }
        drop(tx);
        pump.run(&mut rx).await;
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
        let pump = FeedPump::new(&session, &ledger, account(), FakeVenue::holding(Vec::new()))
            .expect("pump");

        assert!(!session.state().reconciled, "nothing has been checked yet");
        drive(&pump, Vec::new()).await;

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
        let missed = now_ms_u64() - 60_000;
        let venue = FakeVenue::holding(vec![fill(11, missed), fill(12, missed + 10)]);
        let pump = FeedPump::new(&session, &ledger, account(), venue).expect("pump");

        drive(&pump, Vec::new()).await;

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
        let dropped_at = now_ms_u64();
        let venue = FakeVenue::holding(vec![fill(20, dropped_at - 60_000)]);
        let controls = venue.controls.clone();
        let user_fills = Subscription::UserFills { user: account() };
        let pump = FeedPump::new(&session, &ledger, account(), venue).expect("pump");

        drive(&pump, Vec::new()).await;
        assert_eq!(fill_rows(&ledger), 1, "the chain now has an anchor");

        controls.prints(fill(21, dropped_at + 5_000));
        controls.prints(fill(22, dropped_at + 20_000));
        pump.handle(&disconnected(
            dropped_at,
            dropped_at,
            vec![user_fills.clone()],
        ))
        .await;
        assert!(!session.state().reconciled, "the drop refuses new orders");
        pump.handle(&reconnected(dropped_at + 30_000, vec![user_fills]))
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
        let at = now_ms_u64();
        let user_fills = Subscription::UserFills { user: account() };
        let book = Subscription::L2Book { coin: "BTC".into() };
        let pump = FeedPump::new(&session, &ledger, account(), FakeVenue::holding(Vec::new()))
            .expect("pump");

        drive(
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
        let venue = FakeVenue::refusing(Controls::new(true));
        let pump = FeedPump::new(&session, &ledger, account(), venue).expect("pump");

        drive(&pump, Vec::new()).await;

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
        let controls = Controls::new(true);
        let venue = FakeVenue::refusing(controls.clone());
        let pump = FeedPump::new(&session, &ledger, account(), venue).expect("pump");

        let (tx, mut rx) = mpsc::channel::<WsEvent>(1);
        let driver = async {
            // The startup walk has failed by now, and nothing further arrives.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(!session.state().reconciled);
            controls.recover();
            tokio::time::sleep(RETRY_INTERVAL * 2).await;
            drop(tx);
        };
        tokio::join!(pump.run(&mut rx), driver);

        assert!(session.state().reconciled, "the timer came back to it");
        assert!(
            controls.walks() >= 1,
            "the retry actually walked the window"
        );
        assert!(gaps(&ledger).is_empty());
    }
}
