//! The console's live socket (`docs/spec.md` items 31, 32 and 34).
//!
//! Item 34 asks for per-feed status and last-tick timestamps, and the console
//! could answer neither: `account_state` reported `last_tick_ms: None` because
//! nothing here ran a socket, so the staleness overlay was permanently stuck on
//! "never connected" and every number on screen was a REST read taken once at
//! mount. This is the socket that makes those answers true.
//!
//! **The fold is [`oppen_core::feed::FeedSession`]'s, not this module's.** The
//! session already knows what a tick means and what a drop means; the console
//! adds one thing to it — the payloads themselves have to reach the operator's
//! screen, which the MCP path never needed. So the loop here does two things
//! per event and no more: hand it to the session, then emit what the panels
//! draw. Anything that decides something belongs in the core.
//!
//! **The pump is deliberately not used.** [`oppen_core::feed::pump::FeedPump`]
//! owns the reconcile retry, the alert feeds and the quote leases — all three
//! are the agent gateway's concerns, and its `run` loop owns the event stream
//! it would have to share. The console needs freshness and payloads, which is
//! `FeedSession::apply` plus this loop. When the console grows an order path it
//! will need the pump's `reconciled` flag too, and that is the point to move
//! this onto the pump rather than now.

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
pub(crate) struct FeedEnvelope {
    pub network: Network,
    pub generation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub update: FeedUpdate,
}

impl FeedEnvelope {
    fn new(
        network: Network,
        generation: &str,
        update: FeedUpdate,
        failure: &Mutex<Option<String>>,
    ) -> Self {
        Self {
            network,
            generation: generation.to_owned(),
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
    session: Arc<FeedSession>,
    pool: Option<WsPool>,
    event_task: Option<JoinHandle<Result<(), String>>>,
    failure: Arc<Mutex<Option<String>>>,
    drained: Option<Result<(), String>>,
    chart: Option<ChartTransport>,
    diagnostics: Arc<crate::channel_health::Diagnostics>,
    /// What the operator is looking at. Swapped whole on every selection, so
    /// the console never holds a feed for a symbol it stopped drawing.
    watching: Mutex<Vec<Subscription>>,
}

impl ConsoleFeed {
    pub(crate) fn emit_chart(
        app: &AppHandle,
        binding: &ChartBinding,
        projection: crate::chart_transport::Projection,
    ) -> Result<(), String> {
        app.emit(
            CHANNEL,
            FeedEnvelope {
                network: binding.network,
                generation: binding.generation.clone(),
                failure: None,
                update: FeedUpdate::Chart {
                    projection: Box::new(projection),
                },
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
        let (pool, events) = WsPool::new(WsPoolConfig {
            network,
            ..WsPoolConfig::default()
        })
        .map_err(|e| format!("socket pool: {e}"))?;

        let handle = app.clone();
        let loop_session = Arc::clone(&session);
        let loop_account = account.unwrap_or_default();
        let failure = Arc::new(Mutex::new(None));
        let diagnostics = Arc::new(crate::channel_health::Diagnostics::default());
        let loop_diagnostics = diagnostics.clone();
        let loop_failure = failure.clone();
        let event_task =
            spawn_event_consumer(events, failure.clone(), move |event, received_at_ms| {
                loop_diagnostics.record(
                    crate::channel_health::Owner::Console,
                    event,
                    received_at_ms,
                );
                // One blocking consumer preserves application order and owns every
                // ledger write through completion, without occupying an async worker.
                let apply_error = loop_session
                    .apply(&ledger, &loop_account, event, received_at_ms)
                    .err()
                    .map(|error| format!("feed ledger application failed: {error}"));
                if let Some(error) = &apply_error {
                    remember_failure(&loop_failure, error.clone());
                }
                let last_tick_ms = loop_session.state().last_tick_ms;
                let mut update = translate(event, last_tick_ms);
                if let Some(error) = failure_detail(&loop_failure) {
                    match &mut update {
                        Some(FeedUpdate::Status { detail, .. }) => {
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
                                        &generation,
                                        FeedUpdate::Status {
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
                            FeedEnvelope::new(network, &generation, update, &loop_failure),
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
            session,
            pool: Some(pool),
            event_task: Some(event_task),
            failure,
            drained: None,
            chart: None,
            diagnostics,
            watching: Mutex::new(Vec::new()),
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
            remember_failure(&self.failure, error);
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
        let result = self.failure().map_or(Ok(()), Err);
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
    pub(crate) fn last_tick_ms(&self) -> Option<u64> {
        self.session.state().last_tick_ms
    }

    /// Point the socket at the symbol the operator selected.
    ///
    /// Context, BBO and book stay on the common feed. Trades and candles belong
    /// to a separately drained chart transport incarnation. Everything held for the previous symbol is given
    /// back in the same pass — a console that accumulated subscriptions as the
    /// operator browsed would walk into the venue's per-IP ceiling.
    pub(crate) fn watch(&self, coin: &str, _interval: &str) -> Result<(), String> {
        let pool = self.pool.as_ref().ok_or("feed is stopping")?;
        let wanted = vec![
            Subscription::ActiveAssetCtx { coin: coin.into() },
            Subscription::Bbo { coin: coin.into() },
            Subscription::L2Book { coin: coin.into() },
        ];
        self.diagnostics.select(&wanted);
        let mut held = self.watching.lock().map_err(|_| "feed lock poisoned")?;
        // Recorded as each one is taken, never assumed. Returning early on a
        // refusal without writing down what already succeeded would leak those
        // subscriptions permanently: nothing else knows the pool is holding
        // them, so nothing would ever give them back.
        let mut refused = None;
        for sub in &wanted {
            if held.contains(sub) {
                continue;
            }
            match pool.subscribe(sub.clone()) {
                Ok(()) => held.push(sub.clone()),
                Err(error) => {
                    refused = Some(format!("{}: {error}", sub.key()));
                    remember_failure(&self.failure, format!("feed subscription failed: {error}"));
                    break;
                }
            }
        }
        // Released after the new ones are asked for, so switching symbols does
        // not leave the console with no feed at all if a subscribe is refused —
        // and released even when one was, because the symbol the operator left
        // is not coming back on screen either way.
        held.retain(|sub| {
            if wanted.contains(sub) {
                return true;
            }
            if let Err(error) = pool.unsubscribe(sub) {
                remember_failure(&self.failure, format!("feed unsubscribe failed: {error}"));
            }
            false
        });
        match refused {
            Some(error) => Err(error),
            None => Ok(()),
        }
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

    pub(crate) fn chart_failure(&self) -> Option<crate::chart_transport::ChartFailure> {
        self.chart.as_ref().and_then(ChartTransport::failure)
    }

    pub(crate) fn channel_health(
        &self,
        binding: &ChartBinding,
        at: u64,
    ) -> Option<(
        crate::channel_health::PoolObservation,
        crate::channel_health::PoolObservation,
    )> {
        let health = match &self.pool {
            Some(pool) => pool.try_health()?,
            None => Vec::new(),
        };
        let failure = self.failure.try_lock().ok()?.clone().or_else(|| {
            (self.drained.is_none()
                && self
                    .event_task
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished))
            .then(|| "console consumer terminated before shutdown".into())
        });
        let console = self.diagnostics.sample(
            health,
            crate::channel_health::Owner::Console,
            failure.as_deref(),
            at,
        )?;
        let chart = match &self.chart {
            Some(chart) => chart.channel_health(binding, at)?,
            None => crate::channel_health::PoolObservation::missing(),
        };
        Some((console, chart))
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

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::types::{AssetCtx, Candle, L2Book};

    fn owned_test_feed(
        pool: Option<WsPool>,
        event_task: JoinHandle<Result<(), String>>,
        failure: Arc<Mutex<Option<String>>>,
    ) -> ConsoleFeed {
        ConsoleFeed {
            network: Network::Testnet,
            session: Arc::new(FeedSession::new()),
            pool,
            event_task: Some(event_task),
            failure,
            drained: None,
            chart: None,
            diagnostics: Arc::new(crate::channel_health::Diagnostics::default()),
            watching: Mutex::new(Vec::new()),
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
        let envelope = |generation: &str| FeedEnvelope {
            network: Network::Testnet,
            generation: generation.to_owned(),
            failure: None,
            update: translate(&tick(7), Some(7)).unwrap(),
        };
        let first = serde_json::to_value(envelope("1")).unwrap();
        let returned = serde_json::to_value(envelope("3")).unwrap();
        assert_eq!(first["network"], serde_json::json!(Network::Testnet));
        assert_ne!(first["generation"], returned["generation"]);
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
        let envelope = FeedEnvelope::new(
            Network::Testnet,
            "7",
            FeedUpdate::Chart {
                projection: Box::new(crate::chart_transport::Projection {
                    selection_id: "11".into(),
                    chart: chart.projection(0),
                }),
            },
            &Mutex::new(None),
        );
        let wire = serde_json::to_value(envelope).unwrap();
        assert_eq!(wire["network"], "testnet");
        assert_eq!(wire["generation"], "7");
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
        let status = || FeedUpdate::Status {
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
        assert!(feed.watch("BTC", "1m").is_err());
        feed.shutdown_and_drain().await.unwrap();
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
            applied_session
                .apply(&ledger, &account.to_string(), event, received_at_ms)
                .map(|_| ())
                .map_err(|error| error.to_string())
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
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), feed.shutdown_and_drain())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state().last_tick_ms, Some(123));
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
                translate(event, None).unwrap(),
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
        assert!(envelopes[0].failure.is_none());
        let later = serde_json::to_value(&envelopes[1]).unwrap();
        assert_eq!(later["update"]["kind"], "bbo");
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
