//! A chart selection owns its wire incarnation, independently of account feeds.

use std::sync::{Arc, Mutex};

use oppen_core::candles::Interval;
use oppen_core::live_chart::{ChartProjection, HistorySnapshot, HistoryTicket, LiveChart};
use oppen_hl::Network;
use oppen_hl::ws::{EventReceiver, Subscription, WsEvent, WsPool, WsPoolConfig};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Projection {
    pub selection_id: String,
    #[serde(flatten)]
    pub chart: ChartProjection,
}

pub(crate) struct ChartOwner {
    pub binding: ChartBinding,
    state: Mutex<Option<LiveChart>>,
    pub failure: Arc<Mutex<Option<String>>>,
}

impl ChartOwner {
    pub(crate) fn new(binding: ChartBinding) -> Result<Self, String> {
        let interval = Interval::parse(&binding.interval).map_err(|error| error.to_string())?;
        let chart = LiveChart::new(binding.symbol.clone(), interval, None)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            binding,
            state: Mutex::new(Some(chart)),
            failure: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn retire(&self) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub(crate) fn observe(
        &self,
        binding: &ChartBinding,
        event: Option<&WsEvent>,
        received: u64,
    ) -> Option<Projection> {
        if binding != &self.binding {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let chart = state.as_mut()?;
        if let Some(event) = event {
            match event {
                WsEvent::Trades { coin, trades } if coin == &binding.symbol => {
                    chart.observe_trades(trades, received)
                }
                WsEvent::Candle(candle)
                    if candle.s == binding.symbol && candle.i == binding.interval =>
                {
                    chart.observe_candle(candle, received)
                }
                WsEvent::Disconnected(_)
                | WsEvent::Reconnected(_)
                | WsEvent::MessageDropped { .. }
                | WsEvent::VenueError { .. }
                | WsEvent::SubscriptionQuarantined { .. } => chart.interrupt(received),
                _ => return None,
            }
        }
        self.apply_failure(chart);
        Some(Projection {
            selection_id: binding.selection_id.clone(),
            chart: chart.projection(crate::now_ms()),
        })
    }

    pub(crate) fn begin_history(&self) -> Result<HistoryTicket, String> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            .map(LiveChart::begin_history)
            .ok_or_else(|| "chart selection superseded".into())
    }

    pub(crate) fn finish_history(
        &self,
        ticket: HistoryTicket,
        outcome: Result<HistorySnapshot, String>,
    ) -> Result<Projection, String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let chart = state.as_mut().ok_or("chart selection superseded")?;
        match chart.finish_history(ticket, outcome, crate::now_ms()) {
            Ok(()) | Err(oppen_core::live_chart::ChartError::Invalid(_)) => {}
            Err(error) => return Err(error.to_string()),
        }
        self.apply_failure(chart);
        Ok(Projection {
            selection_id: self.binding.selection_id.clone(),
            chart: chart.projection(crate::now_ms()),
        })
    }

    fn apply_failure(&self, chart: &mut LiveChart) {
        if let Some(detail) = self
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            chart.observation_failed(detail);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChartFailure {
    pub binding: ChartBinding,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChartBinding {
    pub network: Network,
    pub generation: String,
    pub selection_id: String,
    pub symbol: String,
    pub interval: String,
}

pub(crate) struct ChartTransport {
    binding: ChartBinding,
    pool: Option<WsPool>,
    consumer: Option<JoinHandle<Result<(), String>>>,
    failure: Arc<Mutex<Option<String>>>,
    drained: Option<Result<(), String>>,
    diagnostics: Arc<crate::channel_health::Diagnostics>,
}

impl ChartTransport {
    pub(crate) fn start(
        binding: ChartBinding,
        failure: Arc<Mutex<Option<String>>>,
        apply: impl FnMut(&ChartBinding, Option<&WsEvent>, u64) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        let (pool, events) = WsPool::new(WsPoolConfig {
            network: binding.network,
            max_connections: 1,
            ..WsPoolConfig::default()
        })
        .map_err(|error| format!("chart socket: {error}"))?;
        Ok(Self::from_pool(binding, pool, events, failure, apply))
    }

    // Also used by real loopback tests. The producer never reads mutable selection state.
    pub(crate) fn from_pool(
        binding: ChartBinding,
        pool: WsPool,
        mut events: EventReceiver,
        failure: Arc<Mutex<Option<String>>>,
        mut apply: impl FnMut(&ChartBinding, Option<&WsEvent>, u64) -> Result<(), String>
        + Send
        + 'static,
    ) -> Self {
        let task_failure = failure.clone();
        let diagnostics = Arc::new(crate::channel_health::Diagnostics::default());
        diagnostics.select(&[
            Subscription::Trades {
                coin: binding.symbol.clone(),
            },
            Subscription::Candle {
                coin: binding.symbol.clone(),
                interval: binding.interval.clone(),
            },
        ]);
        let task_diagnostics = diagnostics.clone();
        let producer_binding = binding.clone();
        let handle = tokio::runtime::Handle::current();
        let consumer = tokio::task::spawn_blocking(move || {
            handle.block_on(async {
                let mut clock = tokio::time::interval(std::time::Duration::from_millis(250));
                clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        biased;
                        frame = events.recv() => {
                            let Some(frame) = frame else { break; };
                            task_diagnostics.record(crate::channel_health::Owner::Chart, frame.event(), frame.received_at_ms());
                            match apply(&producer_binding, Some(frame.event()), frame.received_at_ms()) {
                                Ok(()) => frame.acknowledge(),
                                Err(error) => remember(&task_failure, error),
                            }
                        }
                        _ = clock.tick() => {
                            if let Err(error) = apply(&producer_binding, None, crate::now_ms()) {
                                remember(&task_failure, error);
                            }
                        }
                    }
                }
            });
            if let Err(error) = events.complete() {
                remember(&task_failure, format!("chart ingress completion: {error}"));
            }
            task_failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .map_or(Ok(()), Err)
        });
        // Once spawned, every partial subscription and error remains owned until drain.
        for subscription in [
            Subscription::Trades {
                coin: binding.symbol.clone(),
            },
            Subscription::Candle {
                coin: binding.symbol.clone(),
                interval: binding.interval.clone(),
            },
        ] {
            if let Err(error) = pool.subscribe(subscription) {
                remember(&failure, format!("chart subscription: {error}"));
            }
        }
        Self {
            binding,
            pool: Some(pool),
            consumer: Some(consumer),
            failure,
            drained: None,
            diagnostics,
        }
    }

    pub(crate) fn failure(&self) -> Option<ChartFailure> {
        if self.drained.is_none() && self.consumer.as_ref().is_some_and(JoinHandle::is_finished) {
            remember(
                &self.failure,
                "chart consumer terminated before shutdown".into(),
            );
        }
        self.failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .map(|detail| ChartFailure {
                binding: self.binding.clone(),
                detail,
            })
    }

    pub(crate) fn channel_health(
        &self,
        binding: &ChartBinding,
        at: u64,
    ) -> Option<crate::channel_health::PoolObservation> {
        if binding != &self.binding {
            return None;
        }
        let health = match &self.pool {
            Some(pool) => pool.try_health()?,
            None => Vec::new(),
        };
        let failure = self.failure.try_lock().ok()?.clone().or_else(|| {
            (self.drained.is_none() && self.consumer.as_ref().is_some_and(JoinHandle::is_finished))
                .then(|| "chart consumer terminated before shutdown".into())
        });
        self.diagnostics.sample(
            health,
            crate::channel_health::Owner::Chart,
            failure.as_deref(),
            at,
        )
    }

    /// The owning watch worker retains self if its observer disappears. Keep the
    /// consumer running through producer drain so bounded ingress cannot deadlock.
    pub(crate) async fn shutdown_and_drain(&mut self) -> Result<(), String> {
        if let Some(result) = &self.drained {
            return result.clone();
        }
        if let Some(pool) = &self.pool
            && let Err(error) = pool.shutdown_and_drain().await
        {
            remember(&self.failure, format!("chart socket drain: {error}"));
        }
        drop(self.pool.take());
        if let Some(task) = &mut self.consumer {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => remember(&self.failure, error),
                Err(error) => remember(&self.failure, format!("chart consumer: {error}")),
            }
        }
        self.consumer = None;
        let result = self
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .map_or(Ok(()), Err);
        self.drained = Some(result.clone());
        result
    }
}

fn remember(failure: &Mutex<Option<String>>, error: String) {
    failure
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert(error);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{net::TcpListener, sync::mpsc, time::Duration};
    use tokio_tungstenite::tungstenite::{self, Message};

    fn binding(selection: &str, symbol: &str) -> ChartBinding {
        ChartBinding {
            network: Network::Testnet,
            generation: "1".into(),
            selection_id: selection.into(),
            symbol: symbol.into(),
            interval: "1m".into(),
        }
    }

    pub(crate) fn print(symbol: &str, tid: u64, price: &str) -> serde_json::Value {
        serde_json::json!({"coin":symbol,"side":"B","px":price,"sz":"1","time":crate::now_ms(),"hash":"fixture","tid":tid})
    }

    pub(crate) struct Socket {
        pub(crate) port: u16,
        send: mpsc::Sender<(String, tokio::sync::oneshot::Sender<()>)>,
        pub(crate) task: JoinHandle<()>,
    }

    impl Socket {
        pub(crate) fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let (send, commands) = mpsc::channel::<(String, tokio::sync::oneshot::Sender<()>)>();
            let task = tokio::task::spawn_blocking(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                loop {
                    while let Ok((frame, sent)) = commands.try_recv() {
                        socket.send(Message::Text(frame)).unwrap();
                        let _ = sent.send(());
                    }
                    match socket.read() {
                        Ok(Message::Text(text)) => {
                            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                            if request["method"] == "subscribe" {
                                socket.send(Message::Text(serde_json::json!({"channel":"subscriptionResponse","data":request}).to_string())).unwrap();
                            }
                        }
                        Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
                        Err(tungstenite::Error::Protocol(
                            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                        )) => break,
                        Err(tungstenite::Error::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(tungstenite::Error::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::UnexpectedEof
                                    | std::io::ErrorKind::ConnectionReset
                            ) =>
                        {
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => panic!("fixture socket: {error}"),
                    }
                }
            });
            Self { port, send, task }
        }

        pub(crate) async fn trades(&self, prints: Vec<serde_json::Value>) {
            self.frame(serde_json::json!({"channel":"trades","data":prints}).to_string())
                .await;
        }

        pub(crate) async fn frame(&self, frame: String) {
            let (sent, reply) = tokio::sync::oneshot::channel();
            self.send.send((frame, sent)).unwrap();
            tokio::time::timeout(Duration::from_secs(3), reply)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_socket_retirement_fences_late_decoded_a_before_b_a_and_retains_drain() {
        let mut projections = Vec::new();
        for (selection, symbol) in [("1", "BTC"), ("2", "ETH"), ("3", "BTC")] {
            let socket = Socket::start();
            let owner = Arc::new(ChartOwner::new(binding(selection, symbol)).unwrap());
            let (pool, receiver) = WsPool::loopback_fixture(socket.port).unwrap();
            let (observed, mut updates) = tokio::sync::mpsc::unbounded_channel();
            let (blocked, blocked_rx) = tokio::sync::oneshot::channel();
            let mut blocked = Some(blocked);
            let (release, gate) = mpsc::channel();
            let producer = owner.clone();
            let mut emitted_revision = None;
            let mut transport = ChartTransport::from_pool(
                owner.binding.clone(),
                pool,
                receiver,
                owner.failure.clone(),
                move |captured, event, received| {
                    if let Some(WsEvent::Trades { trades, .. }) = event
                        && trades.iter().any(|trade| trade.tid == 2)
                    {
                        blocked.take().unwrap().send(()).unwrap();
                        gate.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                    if let Some(projection) = producer.observe(captured, event, received)
                        && projection.chart.latest_trade.is_some()
                        && emitted_revision.as_ref() != Some(&projection.chart.revision)
                    {
                        emitted_revision = Some(projection.chart.revision.clone());
                        observed.send(projection).unwrap();
                    }
                    Ok(())
                },
            );
            socket.trades(vec![print(symbol, 1, "100")]).await;
            let initial = tokio::time::timeout(Duration::from_secs(3), updates.recv())
                .await
                .unwrap()
                .unwrap();
            projections.push(initial);
            owner.retire();
            // Bytes are sent only after retirement, then decoded by the actual
            // old socket. Hold application so drain must retain that consumer.
            socket.trades(vec![print(symbol, 2, "999")]).await;
            tokio::time::timeout(Duration::from_secs(3), blocked_rx)
                .await
                .unwrap()
                .unwrap();
            let mut drain = Box::pin(transport.shutdown_and_drain());
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut drain)
                    .await
                    .is_err()
            );
            drop(drain); // A cancelled drain observer must not detach the consumer.
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(3), transport.shutdown_and_drain())
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), socket.task)
                .await
                .unwrap()
                .unwrap();
            assert!(updates.try_recv().is_err());
            assert!(owner.begin_history().is_err());
        }
        assert_eq!(
            projections
                .iter()
                .map(|p| p.selection_id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2", "3"]
        );
        assert!(
            projections
                .iter()
                .all(|p| p.chart.latest_trade.as_ref().unwrap().price == "100")
        );
        assert!(
            projections
                .iter()
                .all(|p| p.chart.forming.as_ref().unwrap().volume == "1")
        );
    }

    #[test]
    fn history_tickets_cannot_erase_live_or_cross_selection() {
        let owner = ChartOwner::new(binding("1", "BTC")).unwrap();
        let older = owner.begin_history().unwrap();
        let latest = owner.begin_history().unwrap();
        let trade = serde_json::from_value(print("BTC", 1, "100")).unwrap();
        let event = WsEvent::Trades {
            coin: "BTC".into(),
            trades: vec![trade],
        };
        let live = owner
            .observe(&owner.binding, Some(&event), crate::now_ms())
            .unwrap();
        assert!(
            owner
                .finish_history(older, Err("old REST failure".into()))
                .is_err()
        );
        let kept = owner
            .finish_history(latest, Err("current REST failure".into()))
            .unwrap();
        assert_eq!(kept.chart.latest_trade, live.chart.latest_trade);
        assert_eq!(
            kept.chart.history_error.as_deref(),
            Some("current REST failure")
        );
        let pending = owner.begin_history().unwrap();
        owner.retire();
        let replacement = ChartOwner::new(binding("3", "BTC")).unwrap();
        assert!(
            replacement
                .finish_history(pending, Err("late A".into()))
                .is_err()
        );
        assert!(
            owner
                .observe(&owner.binding, Some(&event), crate::now_ms())
                .is_none()
        );
        assert!(
            replacement
                .observe(&owner.binding, Some(&event), crate::now_ms())
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiet_chart_clock_closes_observed_bucket_without_freshening_or_empty_bars() {
        let socket = Socket::start();
        let mut selected = binding("1", "BTC");
        // A short core interval bounds this clock test; the desktop interval UI
        // remains unchanged and no external venue is involved.
        selected.interval = "1s".into();
        let owner = Arc::new(ChartOwner::new(selected).unwrap());
        let (pool, events) = WsPool::loopback_fixture(socket.port).unwrap();
        let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let producer = owner.clone();
        let mut transport = ChartTransport::from_pool(
            owner.binding.clone(),
            pool,
            events,
            owner.failure.clone(),
            move |binding, event, received| {
                if let Some(projection) = producer.observe(binding, event, received) {
                    let _ = send.send(projection);
                }
                Ok(())
            },
        );
        socket.trades(vec![print("BTC", 1, "100")]).await;
        let (observed, elapsed) = tokio::time::timeout(Duration::from_secs(3), async {
            let mut observed = None;
            loop {
                let projection = updates.recv().await.unwrap();
                if projection.chart.latest_trade.is_none() {
                    continue;
                }
                observed.get_or_insert_with(|| projection.clone());
                if projection.chart.closed.len() == 1 {
                    break (observed.unwrap(), projection);
                }
            }
        })
        .await
        .unwrap();
        transport.shutdown_and_drain().await.unwrap();
        socket.task.await.unwrap();
        assert!(elapsed.chart.forming.is_none());
        assert_eq!(elapsed.chart.closed.len(), 1);
        assert_eq!(elapsed.chart.closed[0].volume, "1");
        assert_eq!(
            elapsed.chart.last_observation_received_at_ms,
            observed.chart.last_observation_received_at_ms
        );
        assert_eq!(elapsed.chart.latest_trade, observed.chart.latest_trade);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emission_failure_is_visible_before_drain_and_preserves_live_projection() {
        let socket = Socket::start();
        let owner = Arc::new(ChartOwner::new(binding("1", "BTC")).unwrap());
        let (pool, events) = WsPool::loopback_fixture(socket.port).unwrap();
        let (send, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let producer = owner.clone();
        let mut transport = ChartTransport::from_pool(
            owner.binding.clone(),
            pool,
            events,
            owner.failure.clone(),
            move |binding, event, received| {
                let projection = producer.observe(binding, event, received);
                if matches!(event, Some(WsEvent::Trades { .. })) {
                    return Err("synthetic chart emission failure".into());
                }
                if let Some(projection) = projection {
                    let _ = send.send(projection);
                }
                Ok(())
            },
        );
        socket.trades(vec![print("BTC", 1, "100")]).await;
        let projection = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let projection = updates.recv().await.unwrap();
                if projection.chart.observation_error.is_some() {
                    break projection;
                }
            }
        })
        .await
        .unwrap();
        let running_failure = transport.failure().unwrap();
        let drain = transport.shutdown_and_drain().await;
        socket.task.await.unwrap();
        assert_eq!(running_failure.binding, owner.binding);
        assert!(running_failure.detail.contains("emission failure"));
        assert!(drain.is_err());
        assert_eq!(projection.chart.latest_trade.unwrap().price, "100");
        assert_eq!(
            projection.chart.tape_status,
            oppen_core::live_chart::TapeStatus::Interrupted
        );
        assert_eq!(
            projection.chart.observation_error.as_deref(),
            Some("chart transport failed: synthetic chart emission failure")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consumer_panic_is_observable_while_owner_remains_retained() {
        let socket = Socket::start();
        let owner = Arc::new(ChartOwner::new(binding("1", "BTC")).unwrap());
        let (pool, events) = WsPool::loopback_fixture(socket.port).unwrap();
        let mut transport = ChartTransport::from_pool(
            owner.binding.clone(),
            pool,
            events,
            owner.failure.clone(),
            |_, event, _| {
                assert!(
                    !matches!(event, Some(WsEvent::Trades { .. })),
                    "synthetic chart consumer panic"
                );
                Ok(())
            },
        );
        socket.trades(vec![print("BTC", 1, "100")]).await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !transport.consumer.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let running_failure = transport.failure().unwrap();
        let ticket = owner.begin_history().unwrap();
        let held = owner.failure.lock().unwrap();
        assert!(
            transport
                .channel_health(&owner.binding, crate::now_ms())
                .is_none()
        );
        drop(held);
        let health = transport
            .channel_health(&owner.binding, crate::now_ms())
            .unwrap();
        let health = crate::channel_health::Clock::default()
            .project(
                owner.binding.clone(),
                crate::channel_health::PoolObservation::missing(),
                health,
                crate::now_ms(),
            )
            .unwrap();
        assert!(health.rows[3].consumer_failure.is_some());
        assert!(health.rows[4].consumer_failure.is_some());
        let projection = owner
            .finish_history(ticket, Err("history unavailable".into()))
            .unwrap();
        let drain = transport.shutdown_and_drain().await;
        socket.task.await.unwrap();
        assert!(
            running_failure
                .detail
                .contains("terminated before shutdown")
        );
        assert!(projection.chart.observation_error.is_some());
        assert!(drain.is_err());
    }
}
