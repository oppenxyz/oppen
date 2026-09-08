//! Public market transport observations, never execution or account authority.
use std::{collections::VecDeque, sync::Mutex};

use oppen_hl::ws::{FeedHealth, Subscription, WsEvent};
use serde::Serialize;

use crate::chart_transport::ChartBinding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Owner {
    Selected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Channel {
    Context,
    Bbo,
    Depth,
    Trades,
    Candles,
}

const CHANNELS: [Channel; 5] = [
    Channel::Context,
    Channel::Bbo,
    Channel::Depth,
    Channel::Trades,
    Channel::Candles,
];

impl Channel {
    fn owner(self) -> Owner {
        Owner::Selected
    }
    fn subscription(self, binding: &ChartBinding) -> Subscription {
        let coin = binding.symbol.clone();
        match self {
            Self::Context => Subscription::ActiveAssetCtx { coin },
            Self::Bbo => Subscription::Bbo { coin },
            Self::Depth => Subscription::L2Book { coin },
            Self::Trades => Subscription::Trades { coin },
            Self::Candles => Subscription::Candle {
                coin,
                interval: binding.interval.clone(),
            },
        }
    }
    fn of(sub: &Subscription) -> Option<Self> {
        match sub {
            Subscription::ActiveAssetCtx { .. } => Some(Self::Context),
            Subscription::Bbo { .. } => Some(Self::Bbo),
            Subscription::L2Book { .. } => Some(Self::Depth),
            Subscription::Trades { .. } => Some(Self::Trades),
            Subscription::Candle { .. } => Some(Self::Candles),
            _ => None,
        }
    }
    fn wire(name: &str) -> Option<Self> {
        match name {
            "activeAssetCtx" => Some(Self::Context),
            "bbo" => Some(Self::Bbo),
            "l2Book" => Some(Self::Depth),
            "trades" => Some(Self::Trades),
            "candle" => Some(Self::Candles),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Disconnect,
    Reconnect,
    Quarantine,
    ParseLoss,
    VenueError,
    ConsumerFailure,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Diagnostic {
    pub subscription_key: Option<String>,
    pub owner: Owner,
    pub connection_id: Option<String>,
    pub channel: Option<Channel>,
    pub kind: Kind,
    pub received_at_ms: u64,
    pub detail: String,
}

#[derive(Default, Clone)]
struct Records {
    active: Vec<String>,
    recent: VecDeque<Diagnostic>,
    omitted: u64,
    loss: [Option<(String, Diagnostic)>; 5],
    pool_loss: [Option<Diagnostic>; 6],
    failure: Option<String>,
}

#[derive(Default)]
pub(crate) struct Diagnostics(Mutex<Records>);

impl Diagnostics {
    pub(crate) fn select(&self, subscriptions: &[Subscription]) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.active = subscriptions
            .iter()
            .filter(|sub| Channel::of(sub).is_some())
            .take(5)
            .map(Subscription::key)
            .collect();
        let active = state.active.clone();
        for loss in &mut state.loss {
            if loss.as_ref().is_some_and(|(key, _)| !active.contains(key)) {
                *loss = None;
            }
        }
    }
    pub(crate) fn record(&self, owner: Owner, event: &WsEvent, at: u64) {
        match event {
            WsEvent::MessageDropped { channel, .. }
                if matches!(channel.as_str(), "userFills" | "orderUpdates") =>
            {
                return;
            }
            WsEvent::Disconnected(event)
                if !event
                    .subscriptions
                    .iter()
                    .any(|sub| Channel::of(sub).is_some()) =>
            {
                return;
            }
            WsEvent::Reconnected(event)
                if !event
                    .resubscribed
                    .iter()
                    .any(|sub| Channel::of(sub).is_some()) =>
            {
                return;
            }
            _ => {}
        }
        let (connection, channel, kind, detail, affected) = match event {
            WsEvent::Disconnected(event) => (
                Some(event.connection.to_string()),
                None,
                Kind::Disconnect,
                event.reason.clone(),
                event
                    .subscriptions
                    .iter()
                    .filter_map(|sub| Channel::of(sub).map(|channel| (channel, sub.key())))
                    .collect::<Vec<_>>(),
            ),
            WsEvent::Reconnected(event) => (
                Some(event.connection.to_string()),
                None,
                Kind::Reconnect,
                "subscriptions resent; acknowledgment is separate".into(),
                Vec::new(),
            ),
            WsEvent::SubscriptionQuarantined {
                connection,
                subscription,
                ..
            } => {
                let Some(channel) = Channel::of(subscription) else {
                    return;
                };
                (
                    Some(connection.to_string()),
                    Some(channel),
                    Kind::Quarantine,
                    "subscription quarantined".into(),
                    vec![(channel, subscription.key())],
                )
            }
            WsEvent::MessageDropped {
                connection,
                channel,
                reason,
            } => {
                let channel = Channel::wire(channel);
                (
                    Some(connection.to_string()),
                    channel,
                    Kind::ParseLoss,
                    reason.clone(),
                    Vec::new(),
                )
            }
            WsEvent::VenueError {
                connection,
                message,
            } => (
                Some(connection.to_string()),
                None,
                Kind::VenueError,
                message.clone(),
                Vec::new(),
            ),
            _ => return,
        };
        let diagnostic = Diagnostic {
            subscription_key: None,
            owner,
            connection_id: connection,
            channel,
            kind,
            received_at_ms: at,
            detail: bounded(&detail),
        };
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        for (channel, key) in affected {
            if !state.active.contains(&key) {
                continue;
            }
            let mut loss = diagnostic.clone();
            loss.channel = Some(channel);
            loss.subscription_key = Some(key.clone());
            state.loss[channel as usize] = Some((key, loss));
        }
        if kind == Kind::ParseLoss {
            state.pool_loss[channel.map_or(5, |channel| channel as usize)] =
                Some(diagnostic.clone());
        }
        push(&mut state, diagnostic);
    }

    pub(crate) fn sample(
        &self,
        mut health: Vec<FeedHealth>,
        owner: Owner,
        failure: Option<&str>,
        at: u64,
    ) -> Option<PoolObservation> {
        let mut state = self.0.try_lock().ok()?;
        if state.failure.is_none()
            && let Some(failure) = failure
        {
            let detail = bounded(failure);
            state.failure = Some(detail.clone());
            push(
                &mut state,
                Diagnostic {
                    subscription_key: None,
                    owner,
                    connection_id: None,
                    channel: None,
                    kind: Kind::ConsumerFailure,
                    received_at_ms: at,
                    detail,
                },
            );
        }
        health.retain(|row| Channel::of(&row.subscription).is_some());
        let mut records = state.clone();
        let public = |diagnostic: &Diagnostic| {
            diagnostic.connection_id.as_ref().is_none_or(|connection| {
                health
                    .iter()
                    .any(|row| row.connection.to_string() == *connection)
            })
        };
        records.recent.retain(&public);
        for loss in &mut records.pool_loss {
            if loss.as_ref().is_some_and(|loss| !public(loss)) {
                *loss = None;
            }
        }
        Some(PoolObservation { health, records })
    }
}

fn bounded(detail: &str) -> String {
    detail.chars().take(512).collect()
}
fn push(state: &mut Records, diagnostic: Diagnostic) {
    if state.recent.len() == 16 {
        state.recent.pop_front();
        state.omitted = state.omitted.saturating_add(1);
    }
    state.recent.push_back(diagnostic);
}

pub(crate) struct PoolObservation {
    health: Vec<FeedHealth>,
    records: Records,
}

impl PoolObservation {
    pub(crate) fn missing() -> Self {
        Self {
            health: Vec::new(),
            records: Records::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Row {
    pub owner: Owner,
    pub channel: Channel,
    pub connection_id: Option<String>,
    pub subscribed: bool,
    pub connected: bool,
    pub acked: bool,
    pub quarantined: bool,
    pub last_received_at_ms: Option<u64>,
    pub age_ms: Option<u64>,
    pub threshold_ms: Option<u64>,
    pub age_budget_exceeded: Option<bool>,
    pub clock_uncertain: bool,
    pub last_loss: Option<Diagnostic>,
    pub consumer_failure: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Snapshot {
    pub pool_last_losses: Vec<Diagnostic>,
    pub binding: ChartBinding,
    pub revision: String,
    pub observed_at_ms: u64,
    pub clock_uncertain: bool,
    pub rows: Vec<Row>,
    pub diagnostics: Vec<Diagnostic>,
    pub omitted_diagnostics: u64,
}

#[derive(Default)]
pub(crate) struct Clock {
    revision: u64,
    greatest_time: u64,
}

impl Clock {
    pub(crate) fn project(
        &mut self,
        binding: ChartBinding,
        selected: PoolObservation,
        at: u64,
    ) -> Option<Snapshot> {
        self.revision = self.revision.checked_add(1)?;
        let rollback = at < self.greatest_time;
        self.greatest_time = self.greatest_time.max(at);
        let rows = CHANNELS
            .into_iter()
            .map(|channel| {
                let pool = &selected;
                let expected = channel.subscription(&binding);
                let health = pool
                    .health
                    .iter()
                    .find(|health| health.subscription == expected);
                let receipt = health.and_then(|health| health.last_message_ms);
                let uncertain = rollback || receipt.is_some_and(|receipt| receipt > at);
                let age = if uncertain {
                    None
                } else {
                    receipt.and_then(|receipt| at.checked_sub(receipt))
                };
                let threshold = health.and_then(|health| health.threshold_ms);
                Row {
                    owner: channel.owner(),
                    channel,
                    connection_id: health.map(|health| health.connection.to_string()),
                    subscribed: health.is_some(),
                    connected: health.is_some_and(|health| health.connected),
                    acked: health.is_some_and(|health| health.acked),
                    quarantined: health.is_some_and(|health| health.quarantined),
                    last_received_at_ms: receipt,
                    age_ms: age,
                    threshold_ms: threshold,
                    age_budget_exceeded: if uncertain {
                        None
                    } else {
                        threshold.map(|limit| age.is_none_or(|age| age > limit))
                    },
                    clock_uncertain: uncertain,
                    last_loss: pool.records.loss[channel as usize]
                        .as_ref()
                        .filter(|(key, _)| *key == expected.key())
                        .map(|(_, loss)| loss.clone()),
                    consumer_failure: pool.records.failure.clone(),
                }
            })
            .collect::<Vec<_>>();
        let diagnostics = selected.records.recent.into_iter().collect::<Vec<_>>();
        Some(Snapshot {
            pool_last_losses: selected.records.pool_loss.into_iter().flatten().collect(),
            binding,
            revision: self.revision.to_string(),
            observed_at_ms: at,
            clock_uncertain: rollback || rows.iter().any(|row| row.clock_uncertain),
            rows,
            diagnostics,
            omitted_diagnostics: selected.records.omitted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_hl::{
        Network,
        ws::{ConnectionId, Disconnected},
    };

    fn binding(symbol: &str, selection: &str) -> ChartBinding {
        ChartBinding {
            network: Network::Testnet,
            generation: "1".into(),
            selection_id: selection.into(),
            symbol: symbol.into(),
            interval: "1m".into(),
        }
    }
    fn health(
        channel: Channel,
        binding: &ChartBinding,
        received: Option<u64>,
        budget: Option<u64>,
    ) -> FeedHealth {
        FeedHealth {
            connection: ConnectionId::new(0),
            subscription: channel.subscription(binding),
            connected: true,
            acked: true,
            last_message_ms: received,
            age_ms: None,
            threshold_ms: budget,
            stale: false,
            quarantined: false,
        }
    }
    #[test]
    fn missing_and_independent_receipts_never_invent_budgets_or_freshness() {
        let binding = binding("BTC", "1");
        let records = Diagnostics::default();
        let mut clock = Clock::default();
        let sample = records
            .sample(
                vec![
                    health(Channel::Context, &binding, Some(9_999), Some(5_000)),
                    health(Channel::Bbo, &binding, Some(7_000), Some(2_000)),
                    health(Channel::Depth, &binding, Some(1_000), Some(15_000)),
                ],
                Owner::Selected,
                None,
                10_000,
            )
            .unwrap();
        let result = clock.project(binding.clone(), sample, 10_000).unwrap();
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.channel)
                .collect::<Vec<_>>(),
            CHANNELS
        );
        assert_eq!(result.rows[0].age_budget_exceeded, Some(false));
        assert_eq!(result.rows[1].age_budget_exceeded, Some(true));
        assert!(result.rows[1].connected && result.rows[1].acked);
        assert_eq!(result.rows[2].age_budget_exceeded, Some(false));
        assert!(!result.rows[3].subscribed);
        assert_eq!(result.rows[3].threshold_ms, None);
        assert_eq!(result.rows[3].age_budget_exceeded, None);
        let after_capture = records
            .sample(
                vec![health(Channel::Bbo, &binding, Some(10_001), Some(2_000))],
                Owner::Selected,
                None,
                10_000,
            )
            .unwrap();
        let result = clock
            .project(binding.clone(), after_capture, 10_002)
            .unwrap();
        assert!(!result.clock_uncertain);
        assert_eq!(result.rows[1].age_ms, Some(1));
        let rollback = clock
            .project(binding, PoolObservation::missing(), 9_999)
            .unwrap();
        assert!(rollback.clock_uncertain);
        assert!(
            rollback
                .rows
                .iter()
                .all(|row| row.age_ms.is_none() && row.age_budget_exceeded.is_none())
        );
        assert_eq!(rollback.revision, "3");
    }

    #[test]
    fn diagnostics_are_bounded_persistent_scoped_and_try_sample_never_waits() {
        let diagnostics = Diagnostics::default();
        let a = binding("BTC", "1");
        let b = binding("ETH", "2");
        diagnostics.select(&[Channel::Bbo.subscription(&a)]);
        diagnostics.record(
            Owner::Selected,
            &WsEvent::Disconnected(Box::new(Disconnected {
                connection: ConnectionId::new(0),
                at_ms: 10,
                last_message_ms: None,
                subscriptions: vec![Channel::Bbo.subscription(&a)],
                unacked: Vec::new(),
                reason: "lost BTC".into(),
            })),
            10,
        );
        diagnostics.record(
            Owner::Selected,
            &WsEvent::MessageDropped {
                connection: ConnectionId::new(0),
                channel: "bbo".into(),
                reason: "malformed without symbol".into(),
            },
            11,
        );
        for at in 12..42 {
            diagnostics.record(
                Owner::Selected,
                &WsEvent::VenueError {
                    connection: ConnectionId::new(0),
                    message: "x".repeat(600),
                },
                at,
            );
        }
        let held = diagnostics.0.lock().unwrap();
        assert!(
            diagnostics
                .sample(Vec::new(), Owner::Selected, None, 42)
                .is_none()
        );
        drop(held);
        let mut clock = Clock::default();
        let sample = diagnostics
            .sample(
                vec![health(Channel::Bbo, &b, Some(42), Some(2_000))],
                Owner::Selected,
                Some("terminal consumer failure"),
                42,
            )
            .unwrap();
        let result = clock.project(b, sample, 42).unwrap();
        assert!(result.rows[1].last_loss.is_none());
        assert_eq!(result.pool_last_losses.len(), 1);
        assert_eq!(result.diagnostics.len(), 16);
        assert_eq!(result.omitted_diagnostics, 17);
        assert!(
            result
                .diagnostics
                .iter()
                .all(|entry| entry.detail.chars().count() <= 512)
        );
        let sample = diagnostics
            .sample(
                vec![health(Channel::Bbo, &a, Some(43), Some(2_000))],
                Owner::Selected,
                None,
                43,
            )
            .unwrap();
        let result = clock.project(binding("BTC", "3"), sample, 43).unwrap();
        assert_eq!(
            result.rows[1].last_loss.as_ref().unwrap().detail,
            "lost BTC"
        );
        assert_eq!(
            result.rows[1].consumer_failure.as_deref(),
            Some("terminal consumer failure")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_socket_ack_observation_and_parse_loss_project_without_chart_freshness_invention()
     {
        use crate::chart_transport::{ChartOwner, ChartTransport, tests::Socket};
        use std::{sync::Arc, time::Duration};
        let socket = Socket::start();
        let binding = binding("BTC", "1");
        let owner = Arc::new(ChartOwner::new(binding.clone()).unwrap());
        let (pool, receiver) = oppen_hl::ws::WsPool::loopback_fixture(socket.port).unwrap();
        let mut transport = ChartTransport::from_pool(
            binding.clone(),
            pool,
            receiver,
            owner.failure.clone(),
            |_, _, _| Ok(()),
        );
        let mut clock = Clock::default();
        let before = clock
            .project(
                binding.clone(),
                transport.channel_health(&binding, crate::now_ms()).unwrap(),
                crate::now_ms(),
            )
            .unwrap();
        assert!(before.rows[3].last_received_at_ms.is_none());
        let acked = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(chart) = transport.channel_health(&binding, crate::now_ms()) {
                    let snapshot = clock
                        .project(binding.clone(), chart, crate::now_ms())
                        .unwrap();
                    if snapshot.rows[3].acked && snapshot.rows[4].acked {
                        break snapshot;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(acked.rows[3].last_received_at_ms.is_none());
        socket
            .frame(
                serde_json::json!({"channel":"trades","data":[{"coin":"BTC","px":"bad"}]})
                    .to_string(),
            )
            .await;
        let lost = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(chart) = transport.channel_health(&binding, crate::now_ms()) {
                    let snapshot = clock
                        .project(binding.clone(), chart, crate::now_ms())
                        .unwrap();
                    if !snapshot.pool_last_losses.is_empty() {
                        break snapshot;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        transport.shutdown_and_drain().await.unwrap();
        socket.task.await.unwrap();
        assert!(lost.rows[3].last_received_at_ms.is_none());
        assert_eq!(lost.pool_last_losses[0].kind, Kind::ParseLoss);
        assert!(lost.pool_last_losses[0].subscription_key.is_none());
    }

    #[test]
    fn delayed_other_symbol_loss_cannot_replace_active_loss_even_after_ring_eviction() {
        let records = Diagnostics::default();
        let eth = binding("ETH", "2");
        let btc = binding("BTC", "1");
        records.select(&[Channel::Bbo.subscription(&eth)]);
        for (selection, reason) in [(&eth, "ETH loss"), (&btc, "late BTC loss")] {
            records.record(
                Owner::Selected,
                &WsEvent::Disconnected(Box::new(Disconnected {
                    connection: ConnectionId::new(0),
                    at_ms: 10,
                    last_message_ms: None,
                    subscriptions: vec![Channel::Bbo.subscription(selection)],
                    unacked: Vec::new(),
                    reason: reason.into(),
                })),
                10,
            );
        }
        for at in 11..40 {
            records.record(
                Owner::Selected,
                &WsEvent::VenueError {
                    connection: ConnectionId::new(0),
                    message: "other warning".into(),
                },
                at,
            );
        }
        let sample = records
            .sample(
                vec![health(Channel::Bbo, &eth, Some(40), Some(2_000))],
                Owner::Selected,
                Some("sticky terminal"),
                40,
            )
            .unwrap();
        let snapshot = Clock::default().project(eth, sample, 40).unwrap();
        assert_eq!(
            snapshot.rows[1].last_loss.as_ref().unwrap().detail,
            "ETH loss"
        );
        assert!(
            snapshot
                .diagnostics
                .iter()
                .all(|event| event.kind == Kind::VenueError || event.kind == Kind::ConsumerFailure)
        );
        records.select(&[Channel::Bbo.subscription(&btc)]);
        let sample = records
            .sample(
                vec![health(Channel::Bbo, &btc, Some(40), Some(2_000))],
                Owner::Selected,
                None,
                40,
            )
            .unwrap();
        let snapshot = Clock::default().project(btc, sample, 40).unwrap();
        assert!(snapshot.rows[1].last_loss.is_none());
        assert_eq!(
            snapshot.rows[1].consumer_failure.as_deref(),
            Some("sticky terminal")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_unacknowledged_reconnects_quarantine_without_becoming_healthy() {
        use crate::chart_transport::{ChartOwner, ChartTransport};
        use std::{net::TcpListener, sync::Arc, time::Duration};
        use tokio_tungstenite::tungstenite::{self, Message};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (connected, mut connections) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel();
        let server = tokio::task::spawn_blocking(move || {
            for index in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                let mut subscriptions = 0;
                while subscriptions < 5 {
                    if let Message::Text(text) = socket.read().unwrap() {
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if request["method"] == "subscribe" {
                            subscriptions += 1;
                        }
                    }
                }
                connected.send(index).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                socket.close(None).unwrap();
            }
        });
        let binding = binding("BTC", "1");
        let owner = Arc::new(ChartOwner::new(binding.clone()).unwrap());
        let (pool, receiver) = oppen_hl::ws::WsPool::loopback_fixture(port).unwrap();
        let mut transport = ChartTransport::from_pool(
            binding.clone(),
            pool,
            receiver,
            owner.failure.clone(),
            |_, _, _| Ok(()),
        );
        let mut clock = Clock::default();
        for index in 0..3 {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), connections.recv())
                    .await
                    .unwrap(),
                Some(index)
            );
            let sample = transport.channel_health(&binding, crate::now_ms()).unwrap();
            let snapshot = clock
                .project(binding.clone(), sample, crate::now_ms())
                .unwrap();
            assert!(snapshot.rows[3].subscribed);
            assert!(!snapshot.rows[3].acked);
            assert!(snapshot.rows[3].last_received_at_ms.is_none());
            release.send(()).unwrap();
        }
        let quarantined = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(sample) = transport.channel_health(&binding, crate::now_ms()) {
                    let snapshot = clock
                        .project(binding.clone(), sample, crate::now_ms())
                        .unwrap();
                    if snapshot.rows[0].quarantined
                        && snapshot.rows[0]
                            .last_loss
                            .as_ref()
                            .is_some_and(|loss| loss.kind == Kind::Quarantine)
                    {
                        break snapshot;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let drain = transport.shutdown_and_drain().await;
        server.await.unwrap();
        drain.unwrap();
        let quarantined = quarantined.unwrap();
        assert!(!quarantined.rows[3].acked);
        assert!(!quarantined.rows[3].quarantined);
        assert!(quarantined.rows[0].quarantined);
        assert!(
            quarantined
                .diagnostics
                .iter()
                .any(|event| event.kind == Kind::Disconnect)
        );
        assert!(
            quarantined
                .diagnostics
                .iter()
                .any(|event| event.kind == Kind::Reconnect)
        );
    }
}
