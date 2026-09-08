//! What the socket knows, in the two facts the guardrail engine asks for
//! (`docs/spec.md` items 9 and 34).
//!
//! The engine refuses an order when the account is unreconciled or the feed is
//! stale, and both of those were previously answered by constants: `place`
//! reported `reconciled: false` and `last_tick_ms: None` because nothing was
//! running a socket. Fail-closed and honest, but it meant no order could ever
//! clear. This is the thing that makes those answers true.
//!
//! **The state machine is synchronous and the reconcile is not.** Applying an
//! event — a tick arriving, a socket dropping — is a small, total function
//! over the session's own state, so it is driven directly by tests with
//! synthetic events rather than by a live venue. Reconciling a gap is network
//! work, so [`FeedSession::apply`] does not do it: it *asks* for it by
//! returning [`Action::Reconcile`], and whoever owns the runtime does the call
//! and reports back with `FeedSession::reconciled`. The half that decides is
//! testable; the half that talks is glue.
//!
//! **A reconnect does not clear the flag on its own.** The socket coming back
//! is not the account being caught up: the gap between the last message and
//! the new session may hold fills nothing has seen. `reconciled` goes true
//! only when a reconcile over that window has actually returned.
//!
//! **A drop is written down before it is acted on.** [`FeedSession::apply`]
//! opens a `feed_gaps` row per subscription the connection owned and closes it
//! when that feed resumes, so the window survives a restart and the
//! reconciler has something to work from. Nothing wrote those rows before —
//! [`crate::reconcile`] was built against a table only its own tests filled.
//!
//! [`pump`] is the runtime half: a live [`oppen_hl::ws::WsPool`]'s events in
//! one end, [`crate::reconcile::Reconciler`] called at the other.

pub mod pump;

#[cfg(test)]
#[must_use]
pub(crate) struct TestIngress {
    sender: Option<oppen_hl::ws::EventSender>,
    receiver: Option<oppen_hl::ws::EventReceiver>,
}

#[cfg(test)]
impl Drop for TestIngress {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(mut receiver) = self.receiver.take() {
            // Abandon queued obligations on test failure; never acknowledge
            // unprocessed work merely to make teardown look clean.
            while receiver.try_recv().is_ok() {}
            let _ = receiver.complete();
        }
    }
}

#[cfg(test)]
pub(crate) fn test_ingress(session: &FeedSession) -> TestIngress {
    let (sender, receiver) = oppen_hl::ws::event_channel(16);
    session.bind_ingress(receiver.monitor()).unwrap();
    TestIngress {
        sender: Some(sender),
        receiver: Some(receiver),
    }
}

/// Explicit ready-session fixture shared by constructor/snapshot helpers on
/// one test thread. Production always begins unreconciled.
#[cfg(test)]
pub(crate) fn test_session(account: oppen_hl::Address) -> std::sync::Arc<FeedSession> {
    thread_local! {
        static SESSIONS: std::cell::RefCell<std::collections::BTreeMap<String, (Arc<FeedSession>, TestIngress)>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    }
    SESSIONS.with(|sessions| {
        sessions
            .borrow_mut()
            .entry(account.to_string())
            .or_insert_with(|| {
                let session = Arc::new(FeedSession::new());
                session.bind(oppen_hl::Network::Testnet, account).unwrap();
                let ingress = test_ingress(&session);
                session.reconciled(&session.stamp(), 0);
                (session, ingress)
            })
            .0
            .clone()
    })
}

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use oppen_hl::types::Fill;
use oppen_hl::ws::{
    IngressAdmissionGuard, IngressMonitor, IngressObservation, Subscription, WsEvent,
};
use serde_json::json;

use crate::ledger::{Ledger, NewFill};

/// Cached freshness, completeness and lifetime failure evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedState {
    /// Newest message across every subscription, ms. `None` means nothing has
    /// ever arrived — which item 34 distinguishes from having gone quiet,
    /// because there is no last-good value behind the overlay.
    pub last_tick_ms: Option<u64>,
    /// Whether the account has been reconciled against the venue since the
    /// last outage. False until a reconcile returns, and false again the
    /// moment a socket drops.
    pub reconciled: bool,
    /// First persistent pump failure. Only a new session starts without it;
    /// later ticks or successful gap walks cannot prove the lost work recovered.
    pub failure: Option<String>,
}

/// What [`FeedSession::apply`] wants done that it cannot do itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// A connection came back and its gaps are now closed. Work the gap list
    /// ([`crate::reconcile::Reconciler::reconcile_all`]), then call
    /// `FeedSession::reconciled` with the time it completed.
    ///
    /// It carries no window. The gap rows are the window, and they carry more
    /// than the socket knows: the chain position the outage opened at, and the
    /// venue-time anchor read from it. Passing
    /// [`oppen_hl::ws::Reconnected::gap`] alongside them would be a second,
    /// weaker statement of which window is being worked — the mistake
    /// `reconcile.rs` names on [`crate::reconcile::Reconciler`] itself.
    Reconcile,
}

#[derive(Debug)]
struct Inner {
    epoch: Arc<()>,
    ingress: Option<IngressMonitor>,
    scope: Option<(oppen_hl::Network, oppen_hl::Address)>,
    last_tick_ms: Option<u64>,
    reconciled: bool,
    failure: Option<String>,
}

/// Runtime-only snapshot provenance. Invalidations replace the identity;
/// retaining an old stamp cannot make it current again after reconciliation.
#[derive(Debug, Clone)]
pub struct FeedStamp {
    epoch: Arc<()>,
    ingress: Option<IngressObservation>,
}

impl PartialEq for FeedStamp {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.epoch, &other.epoch) && self.ingress == other.ingress
    }
}
impl Eq for FeedStamp {}

pub(crate) struct AdmissionGuard<'a> {
    _ingress: IngressAdmissionGuard,
    _held: MutexGuard<'a, Inner>,
}

impl Inner {
    fn stamp(&self) -> FeedStamp {
        FeedStamp {
            epoch: self.epoch.clone(),
            ingress: self.ingress.as_ref().map(IngressMonitor::observation),
        }
    }
}

/// The live view of one network's feeds.
#[derive(Debug)]
pub struct FeedSession {
    inner: Mutex<Inner>,
}

impl FeedSession {
    /// A session that has seen nothing.
    ///
    /// Starts unreconciled, so an order placed before the first reconcile is
    /// refused rather than cleared against state nobody has checked. That is
    /// the same posture a disconnect returns to.
    pub fn new() -> Self {
        FeedSession {
            inner: Mutex::new(Inner {
                epoch: Arc::new(()),
                ingress: None,
                scope: None,
                last_tick_ms: None,
                reconciled: false,
                failure: None,
            }),
        }
    }

    pub fn state(&self) -> FeedState {
        let inner = self.lock();
        let ingress = inner.ingress.as_ref().map(IngressMonitor::status);
        FeedState {
            last_tick_ms: inner.last_tick_ms,
            reconciled: inner.reconciled
                && ingress.as_ref().is_some_and(|status| {
                    status.pending == 0 && status.failure.is_none() && !status.completed
                }),
            failure: inner
                .failure
                .clone()
                .or_else(|| ingress.and_then(|status| status.failure)),
        }
    }

    /// Capture before gathering the account snapshot, never after its reads.
    pub fn stamp(&self) -> FeedStamp {
        self.lock().stamp()
    }

    pub(crate) fn bind(
        &self,
        network: oppen_hl::Network,
        account: oppen_hl::Address,
    ) -> Result<(), crate::reconcile::ReconcileError> {
        let mut inner = self.lock();
        let requested = (network, account);
        match inner.scope {
            Some(scope) if scope != requested => {
                Err(crate::reconcile::ReconcileError::FeedScopeMismatch)
            }
            Some(_) => Ok(()),
            None => {
                inner.scope = Some(requested);
                inner.reconciled = false;
                inner.epoch = Arc::new(());
                Ok(())
            }
        }
    }

    /// Bind the exact receiver before startup work. A cleanly completed old
    /// consumer is the only replaceable one; failed evidence is never reset.
    pub(crate) fn bind_ingress(
        &self,
        ingress: IngressMonitor,
    ) -> Result<(), crate::reconcile::ReconcileError> {
        let mut inner = self.lock();
        if inner
            .ingress
            .as_ref()
            .is_some_and(|old| old.same_monitor(&ingress))
        {
            return Ok(());
        }
        inner.epoch = Arc::new(());
        inner.reconciled = false;
        if inner
            .ingress
            .as_ref()
            .is_some_and(|old| !old.replacement_eligible())
        {
            return Err(crate::reconcile::ReconcileError::FeedIngressUnavailable);
        }
        inner.ingress = Some(ingress);
        Ok(())
    }

    pub(crate) fn admit(
        &self,
        stamp: Option<&FeedStamp>,
        network: oppen_hl::Network,
        account: oppen_hl::Address,
    ) -> Result<AdmissionGuard<'_>, crate::guardrail::Refusal> {
        let inner = self.lock();
        if !stamp.is_some_and(|stamp| Arc::ptr_eq(&stamp.epoch, &inner.epoch))
            || inner.scope != Some((network, account))
            || !inner.reconciled
            || inner.failure.is_some()
        {
            return Err(crate::guardrail::Unevaluable::FeedAdmission.into());
        }
        let ingress = inner
            .ingress
            .as_ref()
            .ok_or(crate::guardrail::Unevaluable::FeedAdmission)?;
        let observed = stamp
            .and_then(|stamp| stamp.ingress.as_ref())
            .ok_or(crate::guardrail::Unevaluable::FeedAdmission)?;
        let guard = ingress
            .admit(observed)
            .map_err(|_| crate::guardrail::Unevaluable::FeedAdmission)?;
        Ok(AdmissionGuard {
            _ingress: guard,
            _held: inner,
        })
    }

    /// Record that a reconcile over the last gap has returned.
    ///
    /// Separate from [`Action::Reconcile`] because the call between them can
    /// fail: a reconcile that errored must leave the flag false, and the only
    /// way to express that is for success to be an explicit second step.
    ///
    /// Crate-visible: the runtime owner that reports back is [`pump::FeedPump`]
    /// and nothing outside oppen-core drives the flag.
    pub(crate) fn reconciled(&self, stamp: &FeedStamp, at_ms: u64) {
        let mut inner = self.lock();
        if stamp != &inner.stamp() {
            return;
        }
        inner.reconciled = inner.failure.is_none();
        // A reconcile is itself evidence the venue answered, so it counts as a
        // tick. Without this, an account that reconnects into a quiet market
        // would read as stale until the next trade prints.
        inner.last_tick_ms = Some(inner.last_tick_ms.map_or(at_ms, |last| last.max(at_ms)));
    }

    /// Record that the account is no longer known to be caught up.
    ///
    /// A socket dropping is one way in. The other is a fill that could not be
    /// written: the chain is then short a row nothing will offer again until
    /// the window is re-walked, and trading against it would be trading
    /// against a position oppen has mis-stated.
    ///
    /// The tick clock is untouched. Freshness and completeness are different
    /// questions, and this answers only the second.
    pub(crate) fn unreconciled(&self) -> FeedStamp {
        let mut inner = self.lock();
        inner.reconciled = false;
        inner.epoch = Arc::new(());
        inner.stamp()
    }

    /// Latch the first persistent failure without changing the freshness clock.
    /// There is deliberately no reset within this session's lifetime.
    pub(crate) fn record_failure(&self, detail: String) {
        let mut inner = self.lock();
        inner.epoch = Arc::new(());
        inner.failure.get_or_insert(detail);
        inner.reconciled = false;
    }

    /// Fold one event into the session, recording any fills it carried.
    ///
    /// Fills go to the ledger as they arrive, keyed by the venue's `tid`, so
    /// the snapshot the venue sends on subscribe is deduped against what is
    /// already chained rather than appended a second time (item 9). A fill
    /// that cannot be written is reported, and the caller decides — losing the
    /// row is bad, and silently continuing as though the account were
    /// reconciled would be worse.
    pub fn apply(
        &self,
        ledger: &Ledger,
        account: &str,
        event: &WsEvent,
        now_ms: u64,
    ) -> Result<Action, crate::ledger::LedgerError> {
        match event {
            WsEvent::Disconnected(dropped) => {
                // The account is no longer known to be caught up. Anything
                // that filled while the socket was down is unseen, so the
                // engine must refuse until a reconcile says otherwise. Set
                // before the writes below, so a ledger that refuses the gap
                // row still leaves the session fail-closed.
                //
                // The tick clock is *not* reset: item 34 wants "how stale",
                // and the last message is what answers that.
                self.unreconciled();
                // The last message this connection delivered, not the moment
                // the drop was noticed: if the socket was alive at *t*, every
                // fill up to *t* was delivered, so that is the tightest
                // provably-safe start ([`oppen_hl::ws::GapWindow`]).
                let opened_at = ms(dropped.last_message_ms.unwrap_or(dropped.at_ms));
                // One gap per subscription the connection owned, including the
                // market-data ones: `reconcile.rs` answers those with
                // `GapStatus::NotAnAccountFeed` and leaves them for the
                // component that owns candle backfill, and a feed whose outage
                // was never written down is one item 34 cannot report.
                for sub in &dropped.subscriptions {
                    ledger.open_gap(&sub.key(), opened_at, Some(&dropped.reason))?;
                }
                Ok(Action::None)
            }

            WsEvent::Reconnected(back) => {
                // Deliberately does not set `reconciled`. A socket is back;
                // the account is not caught up until the gaps have been read.
                self.tick(back.at_ms);
                let resumed: BTreeSet<String> =
                    back.resubscribed.iter().map(Subscription::key).collect();
                let closed_at = ms(back.at_ms);
                // Only the feeds this connection actually resumed. One the
                // pool quarantined is still down, and closing its window would
                // hand the reconciler an end instant for an outage that has
                // not ended.
                for gap in ledger.unreconciled_gaps()? {
                    if gap.closed_ts_ms.is_none() && resumed.contains(&gap.scope) {
                        ledger.close_gap(gap.gap_id, closed_at)?;
                    }
                }
                Ok(Action::Reconcile)
            }

            WsEvent::UserFills { user, fills, .. } => {
                self.tick(now_ms);
                let account = if user.to_string().is_empty() {
                    account.to_owned()
                } else {
                    user.to_string()
                };
                for fill in fills {
                    self.record(ledger, &account, fill)?;
                }
                Ok(Action::None)
            }

            // Everything else is a tick and nothing more. Matched explicitly
            // rather than with a wildcard so a new variant is a compile error
            // here — a feed oppen starts consuming should be a decision about
            // what it means, not a silent no-op.
            WsEvent::ActiveAssetCtx { received_at_ms, .. } => {
                self.tick(*received_at_ms);
                Ok(Action::None)
            }
            WsEvent::Bbo { venue_time_ms, .. } => {
                self.tick(*venue_time_ms);
                Ok(Action::None)
            }
            WsEvent::Trades { .. } | WsEvent::Candle(_) | WsEvent::L2Book(_) => {
                self.tick(now_ms);
                Ok(Action::None)
            }
            WsEvent::OrderUpdates { .. } => {
                self.tick(now_ms);
                Ok(Action::None)
            }

            // Control events, and none of them is a tick.
            //
            // A quarantined subscription and a venue error frame carry no
            // market data, so neither says the feed is fresh; the pool's own
            // staleness bookkeeping is what reports a dead subscription.
            //
            // `MessageDropped` is the one worth stating: a frame *did* arrive,
            // so the socket is alive — but oppen does not know what it was.
            // Counting it as a tick would claim freshness from a message it
            // could not read, and `ws.rs` puts it exactly right: silence and a
            // parse failure look identical from the outside, and only one of
            // them is safe.
            WsEvent::SubscriptionQuarantined { .. }
            | WsEvent::VenueError { .. }
            | WsEvent::MessageDropped { .. } => Ok(Action::None),
        }
    }

    /// Advance the tick clock, never backwards.
    ///
    /// Venue timestamps across connections are not one clock, and a late
    /// message from a slow socket must not make the feed look staler than the
    /// newest thing oppen has actually seen.
    fn tick(&self, at_ms: u64) {
        let mut inner = self.lock();
        inner.last_tick_ms = Some(inner.last_tick_ms.map_or(at_ms, |last| last.max(at_ms)));
    }

    fn record(
        &self,
        ledger: &Ledger,
        account: &str,
        fill: &Fill,
    ) -> Result<(), crate::ledger::LedgerError> {
        // The whole fill, as the venue sent it. `reconcile` writes the same
        // shape from `userFillsByTime`, so a fill that arrives twice — once
        // live, once in a backfill — is one row keyed by `tid`.
        let payload = json!({
            "account": account,
            "cloid": fill.cloid.as_ref().map(|cloid| cloid.as_str()),
            "coin": fill.coin,
            "px": fill.px.to_string(),
            "sz": fill.sz.to_string(),
            "side": if fill.side.is_buy() { "buy" } else { "sell" },
            "ts_ms": fill.time,
            "start_position": fill.start_position.to_string(),
            "dir": fill.dir,
            "closed_pnl": fill.closed_pnl.to_string(),
            "hash": fill.hash,
            "oid": fill.oid,
            "crossed": fill.crossed,
            "fee": fill.fee.to_string(),
            "fee_token": fill.fee_token,
            "builder_fee": fill.builder_fee.map(|f| f.to_string()),
            "tid": fill.tid,
        });
        ledger.record_fill(&NewFill {
            account,
            tid: fill.tid,
            ts_ms: fill.time as i64,
            // Attribution to an agent is the reconciler's join, not this
            // path's guess: a live fill knows its `oid`, and which intent that
            // came from is a lookup this does not do.
            agent_id: None,
            payload: &payload,
        })?;
        Ok(())
    }

    /// A poisoned lock means a previous caller panicked. The session holds two
    /// scalars and no half-applied state, so the guard is taken back rather
    /// than turning an unrelated panic into a permanently stale feed — which
    /// would refuse every order from then on.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A pool timestamp as the ledger stores it.
///
/// Both of these are the pool's own host clock — `Disconnected::last_message_ms`
/// is when a frame *arrived*, not when the venue stamped it — which is what
/// `feed_gaps` holds and what `reconcile::outage_window` treats as a host
/// reading. The pool counts in unsigned milliseconds and the chain in signed
/// ones; a value past `i64::MAX` is a clock nearly 300 million years out, and
/// saturating keeps the outage on record with a wrong timestamp rather than
/// refusing the row — the same trade `crate::ledger::now_ms` makes.
fn ms(at_ms: u64) -> i64 {
    i64::try_from(at_ms).unwrap_or(i64::MAX)
}

impl Default for FeedSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::Network;
    use oppen_hl::ws::{ConnectionId, Disconnected, GapWindow, Reconnected};
    use tempfile::TempDir;

    const NOW: u64 = 1_756_000_000_000;

    #[test]
    fn completed_ingress_refuses_even_after_successful_reconciliation() {
        let session = FeedSession::new();
        let account = account().parse().unwrap();
        session.bind(Network::Testnet, account).unwrap();
        let stream = test_ingress(&session);
        session.reconciled(&session.stamp(), NOW);
        let cleared_epoch = session.stamp();
        assert!(session.state().reconciled);
        drop(stream);
        assert!(!session.state().reconciled);
        assert!(
            session
                .admit(Some(&cleared_epoch), Network::Testnet, account)
                .is_err()
        );
        session.reconciled(&session.stamp(), NOW + 1);
        assert!(
            !session.state().reconciled,
            "reconciliation cannot reopen completed ingress"
        );
    }

    #[tokio::test]
    async fn ingress_replacement_requires_actual_clean_consumer_completion() {
        let session = FeedSession::new();
        session
            .bind(Network::Testnet, account().parse().unwrap())
            .unwrap();
        let (old_sender, mut old_receiver) = oppen_hl::ws::event_channel(1);
        session.bind_ingress(old_receiver.monitor()).unwrap();
        session.reconciled(&session.stamp(), NOW);
        let original = session.stamp();
        let (next_sender, mut next_receiver) = oppen_hl::ws::event_channel(1);
        assert!(
            session.bind_ingress(next_receiver.monitor()).is_err(),
            "zero pending does not prove the old consumer is gone"
        );
        drop(old_sender);
        assert!(old_receiver.recv().await.is_none());
        old_receiver.complete().unwrap();
        session.bind_ingress(next_receiver.monitor()).unwrap();
        assert!(!session.state().reconciled);
        session.reconciled(&original, NOW + 1);
        assert!(
            !session.state().reconciled,
            "a prior stream cannot reconcile its replacement"
        );
        session.reconciled(&session.stamp(), NOW + 2);
        assert!(session.state().reconciled);
        next_sender
            .send(user_fills(false, vec![]), NOW + 3)
            .await
            .unwrap();
        drop(next_receiver.recv().await.unwrap());
        assert!(!session.state().reconciled);
        assert!(session.state().failure.is_some());
        drop(next_sender);
        assert!(next_receiver.recv().await.is_none());
        assert!(next_receiver.complete().is_err());
        let (_, replacement) = oppen_hl::ws::event_channel(1);
        assert!(
            session.bind_ingress(replacement.monitor()).is_err(),
            "replacement must not erase abandoned accounting"
        );
    }

    #[tokio::test]
    async fn queued_and_active_fill_block_admission_until_durable_acknowledgment() {
        let dir = TempDir::new().unwrap();
        let ledger = Arc::new(ledger(&dir));
        let session = Arc::new(FeedSession::new());
        let account_address = account().parse().unwrap();
        session.bind(Network::Testnet, account_address).unwrap();
        let (sender, mut receiver) = oppen_hl::ws::event_channel(1);
        session.bind_ingress(receiver.monitor()).unwrap();
        session.reconciled(&session.stamp(), NOW);
        let before_ingress = session.stamp();
        let received_at = NOW + 100;
        sender
            .send(user_fills(false, vec![fill(991, NOW)]), received_at)
            .await
            .unwrap();
        assert!(
            !session.state().reconciled,
            "queued accounting is not caught up"
        );
        assert!(
            session
                .admit(Some(&before_ingress), Network::Testnet, account_address)
                .is_err()
        );
        let current = session.stamp();
        assert!(
            session
                .admit(Some(&current), Network::Testnet, account_address)
                .is_err()
        );
        let envelope = receiver.recv().await.unwrap();
        let mut lock_path = dir
            .path()
            .join(crate::db_file_name(Network::Testnet))
            .into_os_string();
        lock_path.push(".lock");
        let held = std::fs::File::options()
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        held.lock().unwrap();
        let applying = {
            let session = session.clone();
            let ledger = ledger.clone();
            tokio::task::spawn_blocking(move || {
                session
                    .apply(
                        &ledger,
                        account(),
                        envelope.event(),
                        envelope.received_at_ms(),
                    )
                    .unwrap();
                envelope.acknowledge();
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while session.state().last_tick_ms != Some(received_at) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !applying.is_finished(),
            "durable write is held, not merely queued"
        );
        assert!(!session.state().reconciled);
        assert!(
            session
                .admit(Some(&current), Network::Testnet, account_address)
                .is_err()
        );
        held.unlock().unwrap();
        applying.await.unwrap();
        assert_eq!(session.state().last_tick_ms, Some(received_at));
        assert!(session.state().reconciled);
        assert!(
            session
                .admit(Some(&before_ingress), Network::Testnet, account_address)
                .is_err()
        );
        assert!(
            session
                .admit(Some(&session.stamp()), Network::Testnet, account_address)
                .is_ok()
        );
        assert_eq!(
            ledger
                .events_of_kind(crate::ledger::EventKind::Fill)
                .unwrap()
                .len(),
            1
        );
        drop(sender);
        assert!(receiver.recv().await.is_none());
        receiver.complete().unwrap();
    }

    #[tokio::test]
    async fn reconciliation_matches_ingress_without_waiting_for_its_own_receipt() {
        let session = FeedSession::new();
        session
            .bind(Network::Testnet, account().parse().unwrap())
            .unwrap();
        let (sender, mut receiver) = oppen_hl::ws::event_channel(2);
        session.bind_ingress(receiver.monitor()).unwrap();
        sender.send(disconnected(NOW), NOW).await.unwrap();
        let own = receiver.recv().await.unwrap();
        let walk = session.unreconciled();
        session.reconciled(&walk, NOW);
        assert!(
            !session.state().reconciled,
            "own receipt still prevents admission"
        );
        own.acknowledge();
        assert!(
            session.state().reconciled,
            "completion did not deadlock on its own receipt"
        );
        let stale_walk = session.unreconciled();
        sender
            .send(user_fills(false, vec![]), NOW + 1)
            .await
            .unwrap();
        session.reconciled(&stale_walk, NOW + 2);
        receiver.recv().await.unwrap().acknowledge();
        assert!(
            !session.state().reconciled,
            "new ingress invalidates the prior walk"
        );
        session.reconciled(&session.stamp(), NOW + 3);
        assert!(session.state().reconciled);
        drop(sender);
        assert!(receiver.recv().await.is_none());
        receiver.complete().unwrap();
    }

    #[test]
    fn admission_rejects_foreign_scope_session_and_stale_reconciliation_completion() {
        let account = account().parse().unwrap();
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);
        session.bind(Network::Testnet, account).unwrap();
        let initial = session.stamp();
        session.reconciled(&initial, NOW);
        assert!(
            session
                .admit(Some(&initial), Network::Testnet, account)
                .is_ok()
        );
        assert!(session.admit(None, Network::Testnet, account).is_err());
        assert!(
            session
                .admit(Some(&initial), Network::Mainnet, account)
                .is_err()
        );
        let foreign_account = oppen_hl::Address::from_bytes([8; 20]);
        assert!(
            session
                .admit(Some(&initial), Network::Testnet, foreign_account)
                .is_err()
        );
        assert!(session.bind(Network::Testnet, foreign_account).is_err());
        let foreign = FeedSession::new();
        let _foreign_ingress = test_ingress(&foreign);
        foreign.bind(Network::Testnet, account).unwrap();
        foreign.reconciled(&foreign.stamp(), NOW);
        assert!(
            session
                .admit(Some(&foreign.stamp()), Network::Testnet, account)
                .is_err()
        );

        let walking = session.unreconciled();
        let after_disconnect = session.unreconciled();
        session.reconciled(&walking, NOW + 1);
        assert!(
            !session.state().reconciled,
            "an older walk cannot clear a newer outage"
        );
        session.reconciled(&after_disconnect, NOW + 2);
        assert!(session.state().reconciled);
        assert!(
            session
                .admit(Some(&initial), Network::Testnet, account)
                .is_err()
        );
        assert!(
            session
                .admit(Some(&walking), Network::Testnet, account)
                .is_err()
        );
        assert!(
            session
                .admit(Some(&after_disconnect), Network::Testnet, account)
                .is_ok()
        );
    }

    fn ledger(dir: &TempDir) -> Ledger {
        Ledger::open(dir.path(), Network::Testnet).expect("ledger")
    }

    fn account() -> &'static str {
        "0xbf829199c1ae7f0caf21fb6fc45e10edff25b7d2"
    }

    fn disconnected(at_ms: u64) -> WsEvent {
        WsEvent::Disconnected(Box::new(Disconnected {
            connection: ConnectionId::new(0),
            at_ms,
            last_message_ms: Some(at_ms - 1_000),
            subscriptions: Vec::new(),
            unacked: Vec::new(),
            reason: "socket closed".into(),
        }))
    }

    fn reconnected(at_ms: u64, gap: GapWindow) -> WsEvent {
        WsEvent::Reconnected(Box::new(Reconnected {
            connection: ConnectionId::new(0),
            at_ms,
            gap,
            resubscribed: Vec::new(),
            attempts: 1,
        }))
    }

    fn bbo(venue_time_ms: u64) -> WsEvent {
        WsEvent::Bbo {
            coin: "BTC".into(),
            venue_time_ms,
            bid: None,
            ask: None,
        }
    }

    fn fill(tid: u64, time: u64) -> Fill {
        use rust_decimal::Decimal;
        use std::str::FromStr;
        let d = |s: &str| Decimal::from_str(s).expect("decimal");
        Fill {
            coin: "BTC".into(),
            px: d("64000"),
            sz: d("0.01"),
            side: oppen_hl::types::Side::B,
            time,
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

    fn user_fills(is_snapshot: bool, fills: Vec<Fill>) -> WsEvent {
        WsEvent::UserFills {
            user: account().parse().expect("address"),
            is_snapshot,
            fills,
        }
    }

    fn fill_rows(ledger: &Ledger) -> usize {
        ledger
            .get_events(0, 1_000)
            .expect("page")
            .events
            .iter()
            .filter(|e| e.kind == crate::ledger::EventKind::Fill)
            .count()
    }

    /// The P2 gate in miniature: a fill that arrives on the socket is in the
    /// chain, and the snapshot the venue replays on re-subscribe does not
    /// double it. `tid` is what makes the two the same fill.
    #[test]
    fn a_fill_is_recorded_once_however_many_times_the_venue_sends_it() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);

        session
            .apply(
                &ledger,
                account(),
                &user_fills(false, vec![fill(7, NOW)]),
                NOW,
            )
            .expect("live fill");
        assert_eq!(fill_rows(&ledger), 1);

        // The backlog the venue sends on subscribe, carrying the same trade.
        session
            .apply(
                &ledger,
                account(),
                &user_fills(true, vec![fill(7, NOW), fill(8, NOW + 1)]),
                NOW + 5,
            )
            .expect("snapshot");

        assert_eq!(fill_rows(&ledger), 2, "tid 7 deduped, tid 8 appended");
    }

    #[test]
    fn live_fill_keeps_account_and_order_identity_for_budget_reconciliation() {
        let dir = TempDir::new().unwrap();
        let ledger = ledger(&dir);
        let mut trade = fill(7, NOW);
        let cloid = oppen_hl::wire::Cloid::from_bytes([7; 16]);
        trade.cloid = Some(cloid.clone());
        FeedSession::new()
            .apply(&ledger, account(), &user_fills(false, vec![trade]), NOW)
            .unwrap();
        let events = ledger.get_events(0, 10).unwrap().events;
        let payload = events
            .iter()
            .find(|event| event.kind == crate::ledger::EventKind::Fill)
            .unwrap()
            .payload
            .as_ref()
            .unwrap();
        assert_eq!(payload["account"], account());
        assert_eq!(payload["cloid"], cloid.as_str());
        assert_eq!(payload["side"], "buy");
        assert_eq!(payload["ts_ms"], NOW);
        assert_eq!(payload["fee_token"], "USDC");
        assert_eq!(payload["fee"], "0.02");
        assert!(ledger.verify().unwrap().is_intact());
    }

    /// The posture everything else rests on: nothing has been checked, so
    /// nothing clears.
    #[test]
    fn a_fresh_session_is_unreconciled_and_has_never_ticked() {
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);
        assert_eq!(
            session.state(),
            FeedState {
                last_tick_ms: None,
                reconciled: false,
                failure: None,
            }
        );
    }

    /// A socket coming back is **not** the account being caught up. The gap
    /// may hold fills nothing has seen, so the flag stays false and the
    /// session asks for the window to be read.
    #[test]
    fn a_reconnect_asks_for_a_reconcile_rather_than_clearing_the_flag() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);
        session.reconciled(&session.stamp(), NOW);
        assert!(session.state().reconciled);

        session
            .apply(&ledger, account(), &disconnected(NOW + 1), NOW + 1)
            .expect("disconnect");
        assert!(!session.state().reconciled, "a drop un-reconciles");

        let gap = GapWindow {
            start_ms: NOW,
            end_ms: NOW + 30_000,
        };
        let action = session
            .apply(
                &ledger,
                account(),
                &reconnected(NOW + 30_000, gap),
                NOW + 30_000,
            )
            .expect("reconnect");
        assert_eq!(action, Action::Reconcile);
        assert!(
            !session.state().reconciled,
            "the socket is back; the account is not caught up until the gap is read"
        );

        session.reconciled(&session.stamp(), NOW + 30_100);
        assert!(session.state().reconciled);
    }

    /// The tick clock survives a disconnect: item 34 asks *how* stale, and the
    /// last message is what answers it.
    #[test]
    fn a_disconnect_does_not_reset_the_tick_clock() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);

        session
            .apply(&ledger, account(), &bbo(NOW), NOW)
            .expect("bbo");
        session
            .apply(&ledger, account(), &disconnected(NOW + 5_000), NOW + 5_000)
            .expect("disconnect");

        assert_eq!(session.state().last_tick_ms, Some(NOW));
    }

    /// Venue timestamps across connections are not one clock. A late message
    /// from a slow socket must not make the feed look staler than the newest
    /// thing oppen has seen.
    #[test]
    fn the_tick_clock_never_goes_backwards() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);

        session
            .apply(&ledger, account(), &bbo(NOW + 10_000), NOW)
            .expect("new");
        session
            .apply(&ledger, account(), &bbo(NOW), NOW)
            .expect("late");

        assert_eq!(session.state().last_tick_ms, Some(NOW + 10_000));
    }

    /// A reconcile is evidence the venue answered. Without counting it, an
    /// account that reconnects into a quiet market reads as stale until the
    /// next trade prints — and refuses every order until then.
    #[test]
    fn a_completed_reconcile_counts_as_a_tick() {
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);
        session.reconciled(&session.stamp(), NOW);
        assert_eq!(session.state().last_tick_ms, Some(NOW));
    }

    #[test]
    fn failure_survives_reconciliation_and_ticks_without_affecting_a_new_session() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();
        let _session_ingress = test_ingress(&session);
        session.reconciled(&session.stamp(), NOW);
        session.record_failure("fill was not persisted".into());
        assert_eq!(session.state().last_tick_ms, Some(NOW));
        assert!(!session.state().reconciled);

        session.record_failure("later failure".into());
        session.reconciled(&session.stamp(), NOW + 1);
        assert_eq!(session.state().last_tick_ms, Some(NOW + 1));
        session
            .apply(&ledger, account(), &bbo(NOW + 2), NOW + 2)
            .expect("tick");
        session.reconciled(&session.stamp(), NOW);
        let failed = session.state();
        assert!(!failed.reconciled);
        assert_eq!(failed.failure.as_deref(), Some("fill was not persisted"));
        assert_eq!(failed.last_tick_ms, Some(NOW + 2));

        let fresh = FeedSession::new();
        let _fresh_ingress = test_ingress(&fresh);
        assert_eq!(fresh.state().failure, None);
        assert_eq!(fresh.state().last_tick_ms, None);
        fresh.reconciled(&fresh.stamp(), NOW + 3);
        assert!(fresh.state().reconciled);
        assert_eq!(session.state(), failed);
    }
}
