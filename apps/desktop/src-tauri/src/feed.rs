//! Native account observations and separately bound selected-market delivery.
//!
//! The account owner applies matching account events through `FeedSession` and
//! retains ledger gaps across selection changes. Its observation clock advances
//! only after successful matching-account application, never from public traffic
//! or connection status. This is display evidence, not execution admission.
//! All five selected public channels belong to the separately drained transport;
//! none are applied to this account session. MCP retains its own guard pump.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::chart_transport::{ChartBinding, ChartTransport};
use oppen_core::feed::FeedSession;
use oppen_core::ledger::Ledger;
use oppen_core::market::{BookLevel, MarketRow};
use oppen_hl::Network;
use oppen_hl::types::Level;
use oppen_hl::ws::{EventReceiver, Subscription, WsEvent, WsPool, WsPoolConfig};
use tauri::{AppHandle, Emitter, Manager};
use tokio::task::JoinHandle;

/// The single channel the console listens on.
///
/// One event with a tagged payload rather than five, so the frontend has one
/// listener and one place where an unknown variant is ignored — a console that
/// silently stopped drawing because a new variant went to a channel nobody
/// subscribed is the failure this shape rules out.
const CHANNEL: &str = "feed://update";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub(crate) enum FeedEnvelope {
    Selected {
        binding: ChartBinding,
        update: Box<FeedUpdate>,
        #[serde(skip_serializing_if = "Option::is_none")]
        failure: Option<String>,
    },
    Account {
        binding: crate::runtime::FeedBinding,
        update: AccountStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        failure: Option<String>,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AccountStatus {
    Status {
        last_tick_ms: Option<u64>,
        connected: bool,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AccountFailure {
    pub binding: crate::runtime::FeedBinding,
    pub detail: String,
}

struct AccountObservation {
    account: Option<oppen_hl::Address>,
    last: Mutex<Option<u64>>,
}

impl AccountObservation {
    fn apply(
        &self,
        session: &FeedSession,
        ledger: &Ledger,
        event: &WsEvent,
        at: u64,
    ) -> Result<(), String> {
        let user = match event {
            WsEvent::UserFills { user, .. } | WsEvent::OrderUpdates { user, .. } => Some(*user),
            WsEvent::ActiveAssetCtx { .. }
            | WsEvent::Bbo { .. }
            | WsEvent::L2Book(_)
            | WsEvent::Trades { .. }
            | WsEvent::Candle(_) => return Ok(()),
            _ => None,
        };
        if user.is_some() && user != self.account {
            return Err("foreign account event refused before ledger application".into());
        }
        session
            .apply(
                ledger,
                &self
                    .account
                    .map(|account| account.to_string())
                    .unwrap_or_default(),
                event,
                at,
            )
            .map_err(|error| format!("feed ledger application failed: {error}"))?;
        if user.is_some() {
            let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
            *last = Some(last.map_or(at, |last| last.max(at)));
        }
        Ok(())
    }
    fn last(&self) -> Option<u64> {
        *self.last.try_lock().ok()?
    }
}

impl FeedEnvelope {
    fn new(
        network: Network,
        generation: &str,
        update: AccountStatus,
        failure: &Mutex<Option<String>>,
    ) -> Self {
        Self::Account {
            binding: crate::runtime::FeedBinding {
                network,
                generation: generation.to_owned(),
            },
            failure: failure_detail(failure),
            update,
        }
    }
}

/// What the console draws, as it arrives.
///
/// Prices stay strings the whole way across, like every other price on this
/// boundary: the renderer parses at its own edge, and nothing between the venue
/// and the pixel rounds.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FeedUpdate {
    Chart {
        projection: Box<crate::chart_transport::Projection>,
    },
    /// `activeAssetCtx`, ~1 s. The rail row, and the strip that reads it.
    ///
    /// A whole [`MarketRow`], not the three fields the strip shows: the frame
    /// carries everything the REST universe read carries, so the console
    /// replaces the row rather than patching fields into it — and the rail,
    /// which renders the same row, goes live for the watched symbol for free.
    Ctx { at_ms: u64, row: MarketRow },
    /// `bbo`, ~0.11 s. Top of book, and the spread the strip shows.
    ///
    /// Either side is `None` when that side is empty, which
    /// `docs/specs/fair-value.md` §5.2 requires to read as stale rather than
    /// as zero — so the frontend must render an absent side as "—".
    Bbo {
        coin: String,
        at_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        bid: Option<BookLevel>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ask: Option<BookLevel>,
    },
    /// `l2Book`, ~5.4 s median. The depth ladder.
    Book {
        coin: String,
        at_ms: u64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    /// The socket's own state, so item 34's overlay has something to read.
    ///
    /// Emitted on connect, drop and resume rather than per tick: a status event
    /// beside every `bbo` frame would be nine messages a second saying the same
    /// thing. Freshness between these is the `at_ms` the payload events carry.
    Status {
        #[serde(skip_serializing_if = "Option::is_none")]
        last_tick_ms: Option<u64>,
        connected: bool,
        /// The venue's own words when a socket dropped, display-only.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

fn level(from: &Level) -> BookLevel {
    BookLevel {
        px: from.px.to_string(),
        sz: from.sz.to_string(),
        n: from.n,
    }
}

/// One network's live feeds, and the subscriptions the console is holding.
pub(crate) struct ConsoleFeed {
    network: Network,
    generation: String,
    observation: Arc<AccountObservation>,
    pool: Option<WsPool>,
    event_task: Option<JoinHandle<Result<(), String>>>,
    failure: Arc<Mutex<Option<String>>>,
    shutdown_failure: Option<String>,
    drained: Option<Result<(), String>>,
    chart: Option<ChartTransport>,
}

impl ConsoleFeed {
    #[cfg(test)]
    pub(crate) fn observation_fixture(account: oppen_hl::Address, at: u64) -> Self {
        Self {
            network: Network::Testnet,
            generation: "1".into(),
            observation: Arc::new(AccountObservation {
                account: Some(account),
                last: Mutex::new(Some(at)),
            }),
            pool: None,
            event_task: None,
            failure: Arc::new(Mutex::new(None)),
            shutdown_failure: None,
            drained: Some(Ok(())),
            chart: None,
        }
    }
    pub(crate) fn selected_envelope(
        binding: &ChartBinding,
        event: &WsEvent,
        failure: Option<String>,
    ) -> Option<FeedEnvelope> {
        let matching = match event {
            WsEvent::ActiveAssetCtx { coin, .. } | WsEvent::Bbo { coin, .. } => {
                coin == &binding.symbol
            }
            WsEvent::L2Book(book) => book.coin == binding.symbol,
            WsEvent::Disconnected(_)
            | WsEvent::Reconnected(_)
            | WsEvent::MessageDropped { .. }
            | WsEvent::VenueError { .. }
            | WsEvent::SubscriptionQuarantined { .. } => true,
            _ => false,
        };
        if matching && let Some(update) = translate(event, None) {
            Some(FeedEnvelope::Selected {
                binding: binding.clone(),
                update: Box::new(update),
                failure,
            })
        } else {
            None
        }
    }
    pub(crate) fn emit_selected(
        app: &AppHandle,
        binding: &ChartBinding,
        event: &WsEvent,
        failure: Option<String>,
    ) -> Result<(), String> {
        if let Some(envelope) = Self::selected_envelope(binding, event, failure) {
            app.emit(CHANNEL, envelope)
                .map_err(|error| format!("selected emission: {error}"))?;
        }
        Ok(())
    }
    pub(crate) fn emit_chart(
        app: &AppHandle,
        binding: &ChartBinding,
        projection: crate::chart_transport::Projection,
    ) -> Result<(), String> {
        app.emit(
            CHANNEL,
            FeedEnvelope::Selected {
                binding: binding.clone(),
                failure: None,
                update: Box::new(FeedUpdate::Chart {
                    projection: Box::new(projection),
                }),
            },
        )
        .map_err(|error| format!("chart emission: {error}"))
    }
    /// Open the socket for one network and start folding its events.
    ///
    /// The account channels are subscribed only when an account is configured.
    /// Market data does not need one, and a console with no account still has
    /// a chart to draw — item 34's status is about the socket, not the wallet.
    /// Runtime supplies its immutable resolved directory and owns this local
    /// blocking startup through completion. A Tokio runtime context is required.
    pub(crate) fn start(
        app: &AppHandle,
        dir: &Path,
        network: Network,
        generation: String,
        account: Option<String>,
    ) -> Result<Self, String> {
        let user = account
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|error| format!("feed account: {error}"))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let ledger = Arc::new(
            Ledger::open_at(&dir.join(oppen_core::db_file_name(network)), network)
                .map_err(|e| format!("ledger: {e}"))?,
        );
        let session = Arc::new(FeedSession::new());
        let observation = Arc::new(AccountObservation {
            account: user,
            last: Mutex::new(None),
        });
        let (pool, events) = WsPool::new(WsPoolConfig {
            network,
            ..WsPoolConfig::default()
        })
        .map_err(|e| format!("socket pool: {e}"))?;

        let handle = app.clone();
        let loop_session = Arc::clone(&session);
        let loop_observation = observation.clone();
        let event_generation = generation.clone();
        let failure = Arc::new(Mutex::new(None));
        let loop_failure = failure.clone();
        let event_task =
            spawn_event_consumer(events, failure.clone(), move |event, received_at_ms| {
                // One blocking consumer preserves application order and owns every
                // ledger write through completion, without occupying an async worker.
                let apply_error = loop_observation
                    .apply(&loop_session, &ledger, event, received_at_ms)
                    .err();
                if let Some(error) = &apply_error {
                    remember_failure(&loop_failure, error.clone());
                }
                let last_tick_ms = loop_observation.last();
                let mut update = account_status(event, last_tick_ms);
                if let Some(error) = failure_detail(&loop_failure) {
                    match &mut update {
                        Some(AccountStatus::Status { detail, .. }) => {
                            *detail = Some(match detail.take() {
                                Some(status) => format!("{error}; {status}"),
                                None => error,
                            });
                        }
                        _ if apply_error.is_some() => {
                            handle
                                .emit(
                                    CHANNEL,
                                    FeedEnvelope::new(
                                        network,
                                        &event_generation,
                                        AccountStatus::Status {
                                            last_tick_ms,
                                            connected: false,
                                            detail: Some(error),
                                        },
                                        &loop_failure,
                                    ),
                                )
                                .map_err(|error| format!("feed event emission failed: {error}"))?;
                        }
                        _ => {}
                    }
                }
                if let Some(update) = update {
                    handle
                        .emit(
                            CHANNEL,
                            FeedEnvelope::new(network, &event_generation, update, &loop_failure),
                        )
                        .map_err(|error| format!("feed event emission failed: {error}"))?;
                }
                apply_error.map_or(Ok(()), Err)
            });
        // Nothing below can return an ownerless construction error. Partial
        // subscriptions are retained as degraded work and drained by Runtime.
        if let Some(user) = user {
            for sub in [
                Subscription::UserFills { user },
                Subscription::OrderUpdates { user },
            ] {
                if let Err(error) = pool.subscribe(sub) {
                    remember_failure(&failure, format!("feed subscription failed: {error}"));
                }
            }
        }
        Ok(Self {
            network,
            generation,
            observation,
            pool: Some(pool),
            event_task: Some(event_task),
            failure,
            shutdown_failure: None,
            drained: None,
            chart: None,
        })
    }

    /// Cancellation-safe while Runtime retains this object. Pool shutdown can
    /// discard an in-flight frame; this drains queued events, not venue history.
    /// A later connection still needs reconciliation before trading.
    pub(crate) async fn shutdown_and_drain(&mut self) -> Result<(), String> {
        if let Some(result) = &self.drained {
            return result.clone();
        }
        if let Err(error) = self.retire_chart().await {
            self.shutdown_failure.get_or_insert(error);
        }
        if let Some(pool) = &self.pool
            && let Err(error) = pool.shutdown_and_drain().await
        {
            remember_failure(&self.failure, format!("socket drain failed: {error}"));
        }
        // The consumer must remain alive through socket drain. Dropping this
        // final sender now lets it finish the queue and then observe EOF.
        drop(self.pool.take());
        if let Some(task) = &mut self.event_task {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => remember_failure(&self.failure, error),
                Err(error) => {
                    remember_failure(&self.failure, format!("feed event task failed: {error}"))
                }
            }
        }
        self.event_task = None;
        let result = self
            .shutdown_failure
            .clone()
            .or_else(|| self.failure())
            .map_or(Ok(()), Err);
        self.drained = Some(result.clone());
        result
    }

    pub(crate) fn failure(&self) -> Option<String> {
        failure_detail(&self.failure).or_else(|| {
            (self.pool.is_some()
                && self
                    .event_task
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished))
            .then(|| "feed event task terminated before shutdown".to_owned())
        })
    }

    /// Whether this feed already serves the network being asked about.
    pub(crate) fn serves(&self, network: Network) -> bool {
        self.network == network
    }

    /// The freshness `account_state` reports (item 34).
    pub(crate) fn last_tick_ms(&self, account: oppen_hl::Address) -> Option<u64> {
        if self.observation.account != Some(account) {
            return None;
        }
        self.observation.last()
    }

    pub(crate) fn account_failure(&self) -> Option<AccountFailure> {
        self.failure().map(|detail| AccountFailure {
            binding: crate::runtime::FeedBinding {
                network: self.network,
                generation: self.generation.clone(),
            },
            detail,
        })
    }

    pub(crate) async fn retire_chart(&mut self) -> Result<(), String> {
        let result = match &mut self.chart {
            Some(chart) => chart.shutdown_and_drain().await,
            None => Ok(()),
        };
        // Err is a completed consumer/socket diagnostic, not a live task.
        // Cancellation before completion leaves the owner in this slot.
        self.chart = None;
        result
    }

    pub(crate) fn start_chart(
        &mut self,
        binding: ChartBinding,
        failure: Arc<Mutex<Option<String>>>,
        apply: impl FnMut(&ChartBinding, Option<&WsEvent>, u64) -> Result<(), String> + Send + 'static,
    ) -> Result<(), String> {
        if self.chart.is_some() {
            return Err("previous chart transport has not drained".into());
        }
        self.chart = Some(ChartTransport::start(binding, failure, apply)?);
        Ok(())
    }

    pub(crate) fn selected_failure(&self) -> Option<crate::chart_transport::ChartFailure> {
        self.chart.as_ref().and_then(ChartTransport::failure)
    }

    pub(crate) fn channel_health(
        &self,
        binding: &ChartBinding,
        at: u64,
    ) -> Option<crate::channel_health::PoolObservation> {
        Some(match &self.chart {
            Some(chart) => chart.channel_health(binding, at)?,
            None => crate::channel_health::PoolObservation::missing(),
        })
    }
}

fn failure_detail(failure: &Mutex<Option<String>>) -> Option<String> {
    failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn remember_failure(failure: &Mutex<Option<String>>, error: String) {
    failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert(error);
}

fn spawn_event_consumer(
    mut events: EventReceiver,
    failure: Arc<Mutex<Option<String>>>,
    mut apply: impl FnMut(&WsEvent, u64) -> Result<(), String> + Send + 'static,
) -> JoinHandle<Result<(), String>> {
    tokio::task::spawn_blocking(move || {
        while let Some(frame) = events.blocking_recv() {
            match apply(frame.event(), frame.received_at_ms()) {
                Ok(()) => frame.acknowledge(),
                Err(error) => remember_failure(&failure, error),
            }
        }
        if let Err(error) = events.complete() {
            remember_failure(&failure, format!("feed ingress completion failed: {error}"));
        }
        failure_detail(&failure).map_or(Ok(()), Err)
    })
}

/// Where the ledger lives.
///
/// `OPPEN_DATA_DIR` first, because that is what `oppen-mcp`'s gateway reads and
/// the two must land on the same file when they are pointed at the same
/// account — R4 makes the database per network, not per process.
pub(crate) fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = match std::env::var("OPPEN_DATA_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => app
            .path()
            .app_data_dir()
            .map_err(|e| format!("no app data directory: {e}"))?,
    };
    Ok(dir)
}

/// What the panels draw, or `None` for an event that carries nothing to draw.
///
/// Matched explicitly rather than with a wildcard, for the reason
/// `FeedSession::apply` gives: a variant this console starts receiving should
/// be a decision about what it draws, not a silent no-op.
fn translate(event: &WsEvent, last_tick_ms: Option<u64>) -> Option<FeedUpdate> {
    match event {
        WsEvent::ActiveAssetCtx {
            coin,
            ctx,
            received_at_ms,
        } => Some(FeedUpdate::Ctx {
            at_ms: *received_at_ms,
            row: MarketRow::of(coin, ctx),
        }),
        WsEvent::Bbo {
            coin,
            venue_time_ms,
            bid,
            ask,
        } => Some(FeedUpdate::Bbo {
            coin: coin.clone(),
            at_ms: *venue_time_ms,
            bid: bid.as_ref().map(level),
            ask: ask.as_ref().map(level),
        }),
        WsEvent::L2Book(book) => Some(FeedUpdate::Book {
            coin: book.coin.clone(),
            at_ms: book.time,
            bids: book.bids().iter().map(level).collect(),
            asks: book.asks().iter().map(level).collect(),
        }),
        WsEvent::Candle(_) | WsEvent::Trades { .. } => None,
        WsEvent::Disconnected(dropped) => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: false,
            detail: Some(dropped.reason.clone()),
        }),
        WsEvent::Reconnected(_) => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: true,
            detail: None,
        }),
        // A quarantined feed is one subscription down, not the socket, and the
        // operator has to fix it — so it reports as a status with the venue's
        // own words rather than as a disconnect.
        WsEvent::SubscriptionQuarantined { subscription, .. } => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: true,
            detail: Some(format!("{} is not being resubscribed", subscription.key())),
        }),
        WsEvent::VenueError { message, .. } => Some(FeedUpdate::Status {
            last_tick_ms,
            connected: true,
            detail: Some(message.clone()),
        }),
        // The account channels reach the screen through `account_state`, which
        // reads the ledger this loop has already written. Emitting them here
        // too would give the activity stream two sources for one fill.
        WsEvent::UserFills { .. } | WsEvent::OrderUpdates { .. } => None,
        // Carries no price, and the pool's own staleness bookkeeping already
        // reports a frame it could not read.
        WsEvent::MessageDropped { .. } => None,
    }
}

fn account_status(event: &WsEvent, last_tick_ms: Option<u64>) -> Option<AccountStatus> {
    match translate(event, last_tick_ms) {
        Some(FeedUpdate::Status {
            last_tick_ms,
            connected,
            detail,
        }) => Some(AccountStatus::Status {
            last_tick_ms,
            connected,
            detail,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::{AssetCtx, Candle, L2Book};

    fn account_fill(user: oppen_hl::Address, tid: u64) -> WsEvent {
        WsEvent::UserFills {
            user,
            is_snapshot: false,
            fills: vec![
                serde_json::from_value(serde_json::json!({
                    "coin":"BTC", "px":"100", "sz":"0.1", "side":"B", "time":100,
                    "startPosition":"0", "dir":"Open Long", "closedPnl":"0", "hash":"synthetic",
                    "oid":1, "crossed":true, "fee":"0.01", "feeToken":"USDC", "tid":tid
                }))
                .unwrap(),
            ],
        }
    }

    #[test]
    fn account_observation_ignores_public_refuses_foreign_and_preserves_durable_history() {
        use oppen_hl::ws::{ConnectionId, Disconnected, GapWindow, Reconnected};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = Ledger::open_at(&path, Network::Testnet).unwrap();
        // Existing public-feed gaps remain historical evidence after migration.
        ledger
            .open_gap(
                &Subscription::Bbo { coin: "BTC".into() }.key(),
                1,
                Some("historical public gap"),
            )
            .unwrap();
        let account: oppen_hl::Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let foreign: oppen_hl::Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();
        let observation = AccountObservation {
            account: Some(account),
            last: Mutex::new(None),
        };
        let session = FeedSession::new();
        let ctx = serde_json::from_value(serde_json::json!({"funding":"0","openInterest":"1","prevDayPx":"100","dayNtlVlm":"1","premium":"0","oraclePx":"100","markPx":"100","midPx":"100","impactPxs":["99","101"],"dayBaseVlm":"1"})).unwrap();
        for event in [
            tick(100),
            WsEvent::ActiveAssetCtx { coin: "BTC".into(), ctx, received_at_ms: 100 },
            WsEvent::L2Book(serde_json::from_value(serde_json::json!({"coin":"BTC","time":100,"levels":[[],[]]})).unwrap()),
            WsEvent::Trades { coin: "BTC".into(), trades: vec![] },
            WsEvent::Candle(serde_json::from_value(serde_json::json!({"t":0,"T":59999,"s":"BTC","i":"1m","o":"100","c":"100","h":"100","l":"100","v":"1","n":1})).unwrap()),
        ] {
            observation.apply(&session, &ledger, &event, 100).unwrap();
        }
        assert_eq!(session.state().last_tick_ms, None);
        assert_eq!(observation.last(), None);
        let head = ledger.chain_head().unwrap();
        assert!(
            observation
                .apply(&session, &ledger, &account_fill(foreign, 1), 110)
                .is_err()
        );
        let unconfigured = AccountObservation {
            account: None,
            last: Mutex::new(None),
        };
        assert!(
            unconfigured
                .apply(&session, &ledger, &account_fill(account, 2), 111)
                .is_err()
        );
        assert_eq!(ledger.chain_head().unwrap(), head);
        assert_eq!(session.state().last_tick_ms, None);
        for (at, event) in [
            (
                120,
                WsEvent::UserFills {
                    user: account,
                    is_snapshot: true,
                    fills: vec![],
                },
            ),
            (
                130,
                WsEvent::OrderUpdates {
                    user: account,
                    updates: vec![],
                },
            ),
            (140, account_fill(account, 3)),
        ] {
            observation.apply(&session, &ledger, &event, at).unwrap();
            assert_eq!(observation.last(), Some(at));
        }
        let subscriptions = vec![Subscription::UserFills { user: account }];
        observation
            .apply(
                &session,
                &ledger,
                &WsEvent::Disconnected(Box::new(Disconnected {
                    connection: ConnectionId::new(0),
                    at_ms: 150,
                    last_message_ms: Some(140),
                    subscriptions: subscriptions.clone(),
                    unacked: vec![],
                    reason: "synthetic gap".into(),
                })),
                150,
            )
            .unwrap();
        observation
            .apply(
                &session,
                &ledger,
                &WsEvent::Reconnected(Box::new(Reconnected {
                    connection: ConnectionId::new(0),
                    at_ms: 160,
                    gap: GapWindow {
                        start_ms: 140,
                        end_ms: 160,
                    },
                    resubscribed: subscriptions,
                    attempts: 1,
                })),
                160,
            )
            .unwrap();
        assert_eq!(observation.last(), Some(140));
        let rows = serde_json::to_value(ledger.get_events(0, 100).unwrap()).unwrap();
        let gaps = ledger.unreconciled_gaps().unwrap();
        assert_eq!(gaps.len(), 2);
        assert!(gaps.iter().any(|gap| gap.closed_ts_ms == Some(160)));
        assert!(
            gaps.iter()
                .any(|gap| gap.opened_ts_ms == 1 && gap.closed_ts_ms.is_none())
        );
        drop(ledger);
        let reopened = Ledger::open_at(&path, Network::Testnet).unwrap();
        assert_eq!(
            serde_json::to_value(reopened.get_events(0, 100).unwrap()).unwrap(),
            rows
        );
        assert_eq!(reopened.unreconciled_gaps().unwrap(), gaps);
    }

    type PublicationGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<bool>);

    #[derive(Debug)]
    struct ApplicationAnchor {
        file: oppen_core::ledger::FileAnchor,
        gate: Arc<Mutex<Option<PublicationGate>>>,
    }

    impl oppen_core::ledger::HeadAnchor for ApplicationAnchor {
        fn load(&self) -> oppen_core::ledger::Result<Option<oppen_core::ledger::Anchor>> {
            self.file.load()
        }
        fn store(&self, anchor: &oppen_core::ledger::Anchor) -> oppen_core::ledger::Result<()> {
            let gate = self.gate.lock().unwrap().take();
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                if release
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                {
                    return Err(std::io::Error::other("synthetic publication failure").into());
                }
            }
            self.file.store(anchor)
        }
    }

    #[test]
    fn account_observation_waits_actual_ledger_publication_and_does_not_advance_on_failure() {
        for fail in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.db");
            let gate = Arc::new(Mutex::new(None));
            let ledger = Ledger::open_anchored(
                &path,
                Network::Testnet,
                Some(Box::new(ApplicationAnchor {
                    file: oppen_core::ledger::FileAnchor::beside(&path),
                    gate: gate.clone(),
                })),
            )
            .unwrap();
            let account = "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap();
            let observation = Arc::new(AccountObservation {
                account: Some(account),
                last: Mutex::new(Some(10)),
            });
            let (entered, blocked) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel();
            *gate.lock().unwrap() = Some((entered, wait));
            let owned = observation.clone();
            let task = std::thread::spawn(move || {
                owned.apply(&FeedSession::new(), &ledger, &account_fill(account, 4), 200)
            });
            let started = blocked.recv_timeout(std::time::Duration::from_secs(3));
            let during = observation.last();
            release.send(fail).unwrap();
            let result = task.join().unwrap();
            started.unwrap();
            assert_eq!(during, Some(10));
            assert_eq!(result.is_err(), fail);
            assert_eq!(observation.last(), Some(if fail { 10 } else { 200 }));
            let reopened = Ledger::open_at(&path, Network::Testnet).unwrap();
            assert_eq!(
                reopened
                    .get_events(0, 100)
                    .unwrap()
                    .events
                    .iter()
                    .filter(|row| row.kind == oppen_core::ledger::EventKind::Fill)
                    .count(),
                1
            );
        }
    }

    fn owned_test_feed(
        pool: Option<WsPool>,
        event_task: JoinHandle<Result<(), String>>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> ConsoleFeed {
        ConsoleFeed {
            network: Network::Testnet,
            generation: "1".into(),
            observation: Arc::new(AccountObservation {
                account: None,
                last: Mutex::new(None),
            }),
            pool,
            event_task: Some(event_task),
            failure,
            shutdown_failure: None,
            drained: None,
            chart: None,
        }
    }

    fn tick(at_ms: u64) -> WsEvent {
        WsEvent::Bbo {
            coin: "BTC".into(),
            venue_time_ms: at_ms,
            bid: None,
            ask: None,
        }
    }

    #[test]
    fn envelope_preserves_update_and_distinguishes_return_to_same_network() {
        let envelope = |generation: &str| FeedEnvelope::Selected {
            binding: ChartBinding {
                network: Network::Testnet,
                generation: generation.into(),
                selection_id: generation.into(),
                symbol: "BTC".into(),
                interval: "1m".into(),
            },
            failure: None,
            update: Box::new(translate(&tick(7), Some(7)).unwrap()),
        };
        let first = serde_json::to_value(envelope("1")).unwrap();
        let returned = serde_json::to_value(envelope("3")).unwrap();
        assert_eq!(
            first["binding"]["network"],
            serde_json::json!(Network::Testnet)
        );
        assert_eq!(first["scope"], "selected");
        assert_ne!(
            first["binding"]["generation"],
            returned["binding"]["generation"]
        );
        assert_eq!(first["update"], returned["update"]);
        assert_eq!(first["update"]["kind"], "bbo");
        assert_eq!(first["update"]["at_ms"], 7);
        assert!(first.get("failure").is_none());
    }

    #[test]
    fn boxed_chart_projection_preserves_frontend_wire_shape() {
        let mut chart = oppen_core::live_chart::LiveChart::new(
            "BTC".into(),
            oppen_core::candles::Interval::parse("1m").unwrap(),
            None,
        )
        .unwrap();
        let envelope = FeedEnvelope::Selected {
            binding: ChartBinding {
                network: Network::Testnet,
                generation: "7".into(),
                selection_id: "11".into(),
                symbol: "BTC".into(),
                interval: "1m".into(),
            },
            update: Box::new(FeedUpdate::Chart {
                projection: Box::new(crate::chart_transport::Projection {
                    selection_id: "11".into(),
                    chart: chart.projection(0),
                }),
            }),
            failure: None,
        };
        let wire = serde_json::to_value(envelope).unwrap();
        assert_eq!(wire["binding"]["network"], "testnet");
        assert_eq!(wire["binding"]["generation"], "7");
        assert_eq!(wire["update"]["kind"], "chart");
        let projection = &wire["update"]["projection"];
        assert_eq!(projection["selection_id"], "11");
        assert_eq!(projection["symbol"], "BTC");
        assert_eq!(projection["interval"], "1m");
        assert_eq!(projection["interval_ms"], 60_000);
        assert!(projection["revision"].is_string());
        assert!(projection["price_decimals"].is_null());
        assert!(projection["closed"].is_array());
        assert!(projection.get("chart").is_none());
    }

    #[test]
    fn envelope_failure_is_latched_work_failure_not_normal_disconnect() {
        let failure = Mutex::new(None);
        let status = || AccountStatus::Status {
            last_tick_ms: Some(7),
            connected: false,
            detail: Some("normal socket disconnect".into()),
        };
        let normal =
            serde_json::to_value(FeedEnvelope::new(Network::Testnet, "1", status(), &failure))
                .unwrap();
        assert!(normal.get("failure").is_none());
        remember_failure(&failure, "synthetic ledger write failure".into());
        let degraded =
            serde_json::to_value(FeedEnvelope::new(Network::Testnet, "1", status(), &failure))
                .unwrap();
        assert_eq!(degraded["failure"], "synthetic ledger write failure");
        assert_eq!(degraded["update"], normal["update"]);
    }

    #[tokio::test]
    async fn drain_drops_pool_sender_then_joins_consumer_and_is_repeatable() {
        // An unsubscribed pool starts no connection and needs no venue.
        let (pool, events) = WsPool::new(WsPoolConfig::default()).unwrap();
        let failure = Arc::new(Mutex::new(None));
        let task = spawn_event_consumer(events, failure.clone(), |_, _| Ok(()));
        let mut feed = owned_test_feed(Some(pool), task, failure);
        assert!(feed.serves(Network::Testnet));
        tokio::time::timeout(std::time::Duration::from_secs(2), feed.shutdown_and_drain())
            .await
            .unwrap()
            .unwrap();
        assert!(feed.event_task.is_none());
        assert!(feed.pool.is_none());
        assert!(feed.failure().is_none());
        feed.shutdown_and_drain().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selected_shutdown_error_survives_dropped_account_drain_without_false_account_failure()
    {
        use crate::chart_transport::tests::Socket;
        for account_fails in [false, true] {
            let socket = Socket::start();
            let selected = ChartBinding {
                network: Network::Testnet,
                generation: "1".into(),
                selection_id: "1".into(),
                symbol: "BTC".into(),
                interval: "1m".into(),
            };
            let (pool, receiver) = WsPool::loopback_fixture(socket.port).unwrap();
            let chart = ChartTransport::from_pool(
                selected,
                pool,
                receiver,
                Arc::new(Mutex::new(None)),
                |_, _, _| Err("synthetic selected emission failure".into()),
            );
            let (sender, events) = oppen_hl::ws::event_channel(1);
            let (entered, entered_rx) = tokio::sync::oneshot::channel();
            let mut entered = Some(entered);
            let (release, wait) = std::sync::mpsc::channel();
            let failure = Arc::new(Mutex::new(None));
            let consumer = spawn_event_consumer(events, failure.clone(), move |_, _| {
                let _ = entered.take().unwrap().send(());
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                if account_fails {
                    Err("synthetic account application failure".into())
                } else {
                    Ok(())
                }
            });
            let mut feed = owned_test_feed(None, consumer, failure);
            feed.chart = Some(chart);
            sender.send(tick(1), 1).await.unwrap();
            entered_rx.await.unwrap();
            drop(sender);
            let failed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while feed.selected_failure().is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let pending = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let mut drain = Box::pin(feed.shutdown_and_drain());
                    let polled = std::future::poll_fn(|cx| {
                        std::task::Poll::Ready(std::future::Future::poll(drain.as_mut(), cx))
                    })
                    .await;
                    drop(drain);
                    if polled.is_ready() {
                        break false;
                    }
                    if feed.chart.is_none() {
                        break true;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let selected_retired = feed.chart.is_none();
            let before_account_failure = feed.account_failure();
            let premature_complete = feed.drained.is_some();
            release.send(()).unwrap();
            let result = feed.shutdown_and_drain().await;
            socket.task.await.unwrap();
            failed.unwrap();
            assert!(pending.unwrap() && selected_retired && !premature_complete);
            assert!(before_account_failure.is_none());
            let error = result.unwrap_err();
            assert!(error.contains("synthetic selected emission failure"));
            assert_eq!(feed.shutdown_and_drain().await.unwrap_err(), error);
            assert_eq!(feed.account_failure().is_some(), account_fails);
            if let Some(failure) = feed.account_failure() {
                assert_eq!(failure.detail, "synthetic account application failure");
                assert_eq!(failure.binding.generation, "1");
            }
        }
    }

    #[tokio::test]
    async fn dropped_drain_waiter_retains_blocking_work_and_queue_order() {
        let (tx, events) = oppen_hl::ws::event_channel(4);
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let (release, gate) = std::sync::mpsc::channel();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let observed = applied.clone();
        let failure = Arc::new(Mutex::new(None));
        let task = spawn_event_consumer(events, failure.clone(), move |event, received_at_ms| {
            let WsEvent::Bbo { venue_time_ms, .. } = event else {
                unreachable!()
            };
            if let Some(started) = started.take() {
                started.send(()).unwrap();
                gate.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            assert_eq!(*venue_time_ms, received_at_ms);
            observed.lock().unwrap().push(*venue_time_ms);
            Ok(())
        });
        let mut feed = owned_test_feed(None, task, failure);
        tx.send(tick(1), 1).await.unwrap();
        tx.send(tick(2), 2).await.unwrap();
        drop(tx);
        started_rx.await.unwrap();
        // A current-thread runtime still runs this timer while the fold blocks.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                feed.shutdown_and_drain()
            )
            .await
            .is_err()
        );
        assert!(feed.event_task.is_some());
        assert!(feed.drained.is_none());
        assert!(applied.lock().unwrap().is_empty());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), feed.shutdown_and_drain())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*applied.lock().unwrap(), vec![1, 2]);
        assert!(feed.event_task.is_none());
    }

    #[tokio::test]
    async fn account_receipt_stays_pending_until_native_application_completes() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open_at(&dir.path().join("ledger.db"), Network::Testnet).unwrap();
        let session = Arc::new(FeedSession::new());
        let applied_session = session.clone();
        let account: oppen_hl::Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let observation = Arc::new(AccountObservation {
            account: Some(account),
            last: Mutex::new(None),
        });
        let applied_observation = observation.clone();
        let (tx, events) = oppen_hl::ws::event_channel(1);
        let monitor = events.monitor();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let (release, gate) = std::sync::mpsc::channel();
        let failure = Arc::new(Mutex::new(None));
        let task = spawn_event_consumer(events, failure.clone(), move |event, received_at_ms| {
            started.take().unwrap().send(()).unwrap();
            gate.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            applied_observation.apply(&applied_session, &ledger, event, received_at_ms)
        });
        let mut feed = owned_test_feed(None, task, failure);
        tx.send(
            WsEvent::OrderUpdates {
                user: account,
                updates: Vec::new(),
            },
            123,
        )
        .await
        .unwrap();
        drop(tx);
        started_rx.await.unwrap();
        assert_eq!(monitor.status().pending, 1);
        assert!(monitor.admit(&monitor.observation()).is_err());
        assert_eq!(session.state().last_tick_ms, None);
        assert_eq!(observation.last(), None);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), feed.shutdown_and_drain())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state().last_tick_ms, Some(123));
        assert_eq!(observation.last(), Some(123));
        assert_eq!(monitor.status().pending, 0);
        assert!(monitor.status().failure.is_none());
        assert!(monitor.replacement_eligible());
        assert!(monitor.admit(&monitor.observation()).is_err());
    }

    #[tokio::test]
    async fn application_failure_remains_degraded_but_keeps_later_ingress() {
        let (tx, events) = oppen_hl::ws::event_channel(4);
        let monitor = events.monitor();
        let failure = Arc::new(Mutex::new(None));
        let applied = Arc::new(Mutex::new(Vec::new()));
        let observed = applied.clone();
        let envelopes = Arc::new(Mutex::new(Vec::new()));
        let emitted = envelopes.clone();
        let loop_failure = failure.clone();
        let task = spawn_event_consumer(events, failure.clone(), move |event, _| {
            emitted.lock().unwrap().push(FeedEnvelope::new(
                Network::Testnet,
                "1",
                AccountStatus::Status {
                    last_tick_ms: None,
                    connected: true,
                    detail: None,
                },
                &loop_failure,
            ));
            let WsEvent::Bbo { venue_time_ms, .. } = event else {
                unreachable!()
            };
            observed.lock().unwrap().push(*venue_time_ms);
            if *venue_time_ms == 1 {
                Err("synthetic ledger write failure".into())
            } else {
                Ok(())
            }
        });
        let mut feed = owned_test_feed(None, task, failure);
        tx.send(tick(1), 1).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while monitor.status().failure.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let later_delivery = tx.send(tick(2), 2).await;
        drop(tx);
        let first = feed.shutdown_and_drain().await.unwrap_err();
        assert!(
            later_delivery.is_ok(),
            "failed admission must not stop accounting"
        );
        assert!(first.contains("ledger write failure"));
        assert_eq!(feed.failure(), Some(first.clone()));
        assert_eq!(feed.shutdown_and_drain().await.unwrap_err(), first);
        assert_eq!(*applied.lock().unwrap(), vec![1, 2]);
        let envelopes = envelopes.lock().unwrap();
        assert!(
            serde_json::to_value(&envelopes[0])
                .unwrap()
                .get("failure")
                .is_none()
        );
        let later = serde_json::to_value(&envelopes[1]).unwrap();
        assert_eq!(later["scope"], "account");
        assert_eq!(later["update"]["kind"], "status");
        assert_eq!(later["failure"], "synthetic ledger write failure");
        assert!(feed.event_task.is_none());
    }

    #[tokio::test]
    async fn consumer_panic_is_a_factual_drain_failure_not_stopped_success() {
        let (tx, events) = oppen_hl::ws::event_channel(1);
        let failure = Arc::new(Mutex::new(None));
        let task = spawn_event_consumer(events, failure.clone(), |_, _| {
            panic!("synthetic consumer panic")
        });
        let mut feed = owned_test_feed(None, task, failure);
        tx.send(tick(1), 1).await.unwrap();
        drop(tx);
        let error = feed.shutdown_and_drain().await.unwrap_err();
        assert!(error.contains("feed event task failed"));
        assert_eq!(feed.shutdown_and_drain().await.unwrap_err(), error);
        assert!(feed.event_task.is_none());
    }

    fn ctx() -> AssetCtx {
        serde_json::from_value(serde_json::json!({
            "funding": "0.0000125",
            "openInterest": "1234.5",
            "prevDayPx": "100.0",
            "dayNtlVlm": "9999.0",
            "premium": "0.0001",
            "oraclePx": "101.0",
            "markPx": "101.5",
            "midPx": "101.4",
            "impactPxs": ["101.3", "101.6"]
        }))
        .expect("asset context fixture")
    }

    #[test]
    fn a_context_frame_carries_the_strip_the_console_draws() {
        let event = WsEvent::ActiveAssetCtx {
            coin: "BTC".into(),
            ctx: Box::new(ctx()),
            received_at_ms: 1_788_000_000_000,
        };
        let Some(FeedUpdate::Ctx { at_ms, row }) = translate(&event, None) else {
            panic!("a context frame must reach the strip");
        };
        assert_eq!(row.symbol, "BTC");
        assert_eq!(at_ms, 1_788_000_000_000);
        // The venue's own decimal, not a float that has been through an f64.
        assert_eq!(row.mark_px, "101.5");
        assert_eq!(row.open_interest, "1234.5");
        // The same hour-to-date bps rule the REST rail uses: 0.0000125 of
        // price is 0.125 bp. A socket row that disagreed with a REST row would
        // make the rail flicker between two conventions.
        assert_eq!(row.funding_1h_bps, "0.1250");
    }

    #[test]
    fn an_empty_book_side_stays_absent_rather_than_zero() {
        let event = WsEvent::Bbo {
            coin: "ETH".into(),
            venue_time_ms: 42,
            bid: Some(Level {
                px: "10".parse().expect("a price"),
                sz: "2".parse().expect("a size"),
                n: 3,
            }),
            ask: None,
        };
        let Some(FeedUpdate::Bbo { bid, ask, .. }) = translate(&event, None) else {
            panic!("a bbo frame must reach the strip");
        };
        assert_eq!(bid.expect("the bid side").px, "10");
        // §5.2: an empty side is stale, never zero. A `Some(0)` here would let
        // the spread render as a number the venue never quoted.
        assert!(ask.is_none());
    }

    #[test]
    fn a_drop_reports_the_last_tick_it_had_rather_than_clearing_it() {
        let event = WsEvent::Disconnected(Box::new(oppen_hl::ws::Disconnected {
            connection: oppen_hl::ws::ConnectionId::new(0),
            at_ms: 900,
            last_message_ms: Some(800),
            subscriptions: Vec::new(),
            unacked: Vec::new(),
            reason: "1006".into(),
        }));
        let Some(FeedUpdate::Status {
            last_tick_ms,
            connected,
            detail,
        }) = translate(&event, Some(800))
        else {
            panic!("a drop must reach the overlay");
        };
        assert!(!connected);
        // Item 34 wants "how stale", so the last good tick survives the drop.
        assert_eq!(last_tick_ms, Some(800));
        assert_eq!(detail.as_deref(), Some("1006"));
    }

    #[test]
    fn a_fill_does_not_reach_the_screen_twice() {
        let event = WsEvent::OrderUpdates {
            user: "0x0000000000000000000000000000000000000001"
                .parse()
                .expect("an address"),
            updates: Vec::new(),
        };
        // The ledger is the one event system (D6): the activity stream reads it
        // through `account_state`, so emitting here would be a second source.
        assert!(translate(&event, None).is_none());
    }

    #[test]
    fn the_common_feed_cannot_publish_unbound_chart_frames() {
        let candle: Candle = serde_json::from_value(serde_json::json!({
            "t": 1_788_000_000_000u64,
            "T": 1_788_000_059_999u64,
            "s": "SOL", "i": "1m",
            "o": "1.0", "c": "1.5", "h": "1.6", "l": "0.9", "v": "10", "n": 4
        }))
        .expect("a candle fixture");
        assert!(translate(&WsEvent::Candle(Box::new(candle)), None).is_none());
    }

    #[test]
    fn a_depth_frame_keeps_the_venue_side_order() {
        let book: L2Book = serde_json::from_value(serde_json::json!({
            "coin": "BTC",
            "time": 7,
            "levels": [
                [{"px": "100", "sz": "1", "n": 1}, {"px": "99", "sz": "2", "n": 2}],
                [{"px": "101", "sz": "3", "n": 3}]
            ]
        }))
        .expect("a book fixture");
        let Some(FeedUpdate::Book { bids, asks, .. }) =
            translate(&WsEvent::L2Book(Box::new(book)), None)
        else {
            panic!("a book frame must reach the ladder");
        };
        // levels[0] is bids best-first, levels[1] asks. Swapping them would
        // draw the ladder inside out.
        assert_eq!(bids.len(), 2);
        assert_eq!(bids[0].px, "100");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].px, "101");
    }
}
