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
//! and reports back with [`FeedSession::reconciled`]. The half that decides is
//! testable; the half that talks is glue.
//!
//! **A reconnect does not clear the flag on its own.** The socket coming back
//! is not the account being caught up: the gap between the last message and
//! the new session may hold fills nothing has seen. `reconciled` goes true
//! only when a reconcile over that window has actually returned.

use std::sync::Mutex;

use oppen_hl::types::Fill;
use oppen_hl::ws::{GapWindow, WsEvent};
use serde_json::json;

use crate::ledger::{Ledger, NewFill};

/// The two facts `oppen_core::state` and the guardrail engine need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedState {
    /// Newest message across every subscription, ms. `None` means nothing has
    /// ever arrived — which item 34 distinguishes from having gone quiet,
    /// because there is no last-good value behind the overlay.
    pub last_tick_ms: Option<u64>,
    /// Whether the account has been reconciled against the venue since the
    /// last outage. False until a reconcile returns, and false again the
    /// moment a socket drops.
    pub reconciled: bool,
}

/// What [`FeedSession::apply`] wants done that it cannot do itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// A connection came back. Reconcile this window, then call
    /// [`FeedSession::reconciled`] with the time it completed.
    Reconcile(GapWindow),
}

#[derive(Debug)]
struct Inner {
    last_tick_ms: Option<u64>,
    reconciled: bool,
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
                last_tick_ms: None,
                reconciled: false,
            }),
        }
    }

    pub fn state(&self) -> FeedState {
        let inner = self.lock();
        FeedState {
            last_tick_ms: inner.last_tick_ms,
            reconciled: inner.reconciled,
        }
    }

    /// Record that a reconcile over the last gap has returned.
    ///
    /// Separate from [`Action::Reconcile`] because the call between them can
    /// fail: a reconcile that errored must leave the flag false, and the only
    /// way to express that is for success to be an explicit second step.
    pub fn reconciled(&self, at_ms: u64) {
        let mut inner = self.lock();
        inner.reconciled = true;
        // A reconcile is itself evidence the venue answered, so it counts as a
        // tick. Without this, an account that reconnects into a quiet market
        // would read as stale until the next trade prints.
        inner.last_tick_ms = Some(inner.last_tick_ms.map_or(at_ms, |last| last.max(at_ms)));
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
                let mut inner = self.lock();
                // The account is no longer known to be caught up. Anything
                // that filled while the socket was down is unseen, so the
                // engine must refuse until a reconcile says otherwise.
                inner.reconciled = false;
                // The tick clock is *not* reset: item 34 wants "how stale",
                // and the last message is what answers that.
                let _ = dropped;
                Ok(Action::None)
            }

            WsEvent::Reconnected(back) => {
                // Deliberately does not set `reconciled`. A socket is back;
                // the account is not caught up until the gap has been read.
                self.tick(back.at_ms);
                Ok(Action::Reconcile(back.gap))
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
            "coin": fill.coin,
            "px": fill.px.to_string(),
            "sz": fill.sz.to_string(),
            "side": fill.side,
            "time_ms": fill.time,
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

impl Default for FeedSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::Network;
    use oppen_hl::ws::{ConnectionId, Disconnected, Reconnected};
    use tempfile::TempDir;

    const NOW: u64 = 1_756_000_000_000;

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

    /// The posture everything else rests on: nothing has been checked, so
    /// nothing clears.
    #[test]
    fn a_fresh_session_is_unreconciled_and_has_never_ticked() {
        let session = FeedSession::new();
        assert_eq!(
            session.state(),
            FeedState {
                last_tick_ms: None,
                reconciled: false
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
        session.reconciled(NOW);
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
        assert_eq!(action, Action::Reconcile(gap));
        assert!(
            !session.state().reconciled,
            "the socket is back; the account is not caught up until the gap is read"
        );

        session.reconciled(NOW + 30_100);
        assert!(session.state().reconciled);
    }

    /// The tick clock survives a disconnect: item 34 asks *how* stale, and the
    /// last message is what answers it.
    #[test]
    fn a_disconnect_does_not_reset_the_tick_clock() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let session = FeedSession::new();

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
        session.reconciled(NOW);
        assert_eq!(session.state().last_tick_ms, Some(NOW));
    }
}
