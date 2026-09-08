//! ES19: ownership of desktop feeds and reads, not trading activation.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use oppen_core::guardrail::AgentId;
use oppen_hl::Network;
use oppen_mcp::auth::Binding;
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;

use crate::feed::ConsoleFeed;
use crate::local_reads::{LocalReads, ReadKind};
use crate::mcp_runtime::{McpPhase, McpStatus, OwnedMcp, SharedStatus, status_lock};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FeedBinding {
    pub network: Network,
    pub generation: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Start(Network, String),
        Watch(String, String),
        Drain,
        Joined,
    }

    // Only the feed boundary is controlled. Admission, replacement, retained
    // spawn_blocking startup, cancellation and terminal delivery are real.
    pub(super) struct Driver {
        events: UnboundedSender<Event>,
        starts: AtomicUsize,
        start_gate: Mutex<Option<mpsc::Receiver<()>>>,
        drain_gate: Mutex<Option<mpsc::Receiver<()>>>,
        fail_watch: AtomicBool,
        panic_watch: AtomicBool,
        fail_drain: AtomicBool,
        reported_failure: Mutex<Option<String>>,
    }

    impl Driver {
        fn new() -> (Arc<Self>, UnboundedReceiver<Event>) {
            let (events, received) = unbounded_channel();
            (
                Arc::new(Self {
                    events,
                    starts: AtomicUsize::new(0),
                    start_gate: Mutex::new(None),
                    drain_gate: Mutex::new(None),
                    fail_watch: AtomicBool::new(false),
                    panic_watch: AtomicBool::new(false),
                    fail_drain: AtomicBool::new(false),
                    reported_failure: Mutex::new(None),
                }),
                received,
            )
        }

        pub(super) fn start(
            self: &Arc<Self>,
            network: Network,
            generation: String,
        ) -> Result<ControlledFeed, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let _ = self.events.send(Event::Start(network, generation));
            if let Some(gate) = self.start_gate.lock().unwrap().take() {
                let _ = gate.recv();
            }
            let gate = self.drain_gate.lock().unwrap().take();
            let driver = self.clone();
            let work = tauri::async_runtime::spawn_blocking(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                if driver.fail_drain.load(Ordering::SeqCst) {
                    Err("controlled consumer failure".into())
                } else {
                    Ok(())
                }
            });
            Ok(ControlledFeed {
                driver: self.clone(),
                work: Some(work),
            })
        }
    }

    pub(super) struct ControlledFeed {
        driver: Arc<Driver>,
        work: Option<tauri::async_runtime::JoinHandle<Result<(), String>>>,
    }

    impl ControlledFeed {
        pub(super) fn failure(&self) -> Option<String> {
            self.driver.reported_failure.lock().unwrap().clone()
        }

        pub(super) fn watch(&self, coin: &str, interval: &str) -> Result<(), String> {
            assert!(
                !self.driver.panic_watch.load(Ordering::SeqCst),
                "controlled watch panic"
            );
            if self.driver.fail_watch.load(Ordering::SeqCst) {
                return Err("controlled watch refusal".into());
            }
            let _ = self
                .driver
                .events
                .send(Event::Watch(coin.into(), interval.into()));
            Ok(())
        }

        pub(super) async fn shutdown_and_drain(&mut self) -> Result<(), String> {
            let _ = self.driver.events.send(Event::Drain);
            let result = if let Some(work) = self.work.as_mut() {
                work.await.map_err(|error| error.to_string())?
            } else {
                Ok(())
            };
            self.work = None;
            let _ = self.driver.events.send(Event::Joined);
            result
        }
    }

    fn runtime() -> Runtime {
        // No filesystem access, application handle, keychain or network.
        Runtime::new(PathBuf::from("/unused-desktop-owner-fixture"))
    }

    fn watch(
        runtime: &Runtime,
        driver: &Arc<Driver>,
        network: Network,
        account: Option<&str>,
        coin: &str,
    ) -> oneshot::Receiver<Result<FeedBinding, RuntimeError>> {
        runtime
            .submit_watch(
                FeedSource::Controlled(driver.clone()),
                Selection {
                    network,
                    account: account.map(str::to_owned),
                    coin: coin.into(),
                    interval: "1m".into(),
                },
            )
            .expect("watch admission")
    }

    async fn event(events: &mut UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn reply(reply: oneshot::Receiver<Result<FeedBinding, RuntimeError>>) -> FeedBinding {
        tokio::time::timeout(Duration::from_secs(3), reply)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn drained(runtime: &Runtime) -> Result<(), RuntimeError> {
        tokio::time::timeout(Duration::from_secs(3), runtime.shutdown())
            .await
            .expect("actual work completed")
    }

    async fn pending(runtime: &Runtime) {
        // Dropping this observer must not cancel the retained drain operation.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), runtime.shutdown())
                .await
                .is_err()
        );
        assert_eq!(runtime.status().phase, RuntimePhase::Stopping);
    }

    #[tokio::test]
    async fn first_same_network_feed_does_not_wait_for_keychain_but_stop_does() {
        let runtime = runtime();
        let (release, gate) = mpsc::channel::<()>();
        let (started, observing) = oneshot::channel();
        drop(
            runtime
                .local_read(Network::Testnet, ReadKind::Keychain, move || {
                    let _ = started.send(());
                    let _ = gate.recv();
                })
                .unwrap(),
        );
        observing.await.unwrap();
        let lease = runtime.read_lease(Network::Testnet).unwrap();
        let (driver, mut events) = Driver::new();
        let binding = reply(watch(&runtime, &driver, Network::Testnet, None, "BTC")).await;
        assert_eq!(binding.generation, "1");
        assert_eq!(
            event(&mut events).await,
            Event::Start(Network::Testnet, "1".into())
        );
        assert_eq!(
            event(&mut events).await,
            Event::Watch("BTC".into(), "1m".into())
        );
        for kind in [ReadKind::Pilot, ReadKind::Operator] {
            assert_eq!(
                runtime
                    .local_read(Network::Testnet, kind, || 7)
                    .unwrap()
                    .await
                    .unwrap(),
                7
            );
        }
        // The original slot and HTTP lease remain owned, not replaced.
        assert!(matches!(
            runtime.local_read(Network::Testnet, ReadKind::Keychain, || ()),
            Err(RuntimeError::Busy)
        ));
        pending(&runtime).await;
        drop(release);
        pending(&runtime).await;
        drop(lease);
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn first_different_network_feed_waits_for_existing_reads() {
        let runtime = runtime();
        let (release, gate) = mpsc::channel::<()>();
        let (started, observing) = oneshot::channel();
        drop(
            runtime
                .local_read(Network::Testnet, ReadKind::Keychain, move || {
                    let _ = started.send(());
                    let _ = gate.recv();
                })
                .unwrap(),
        );
        observing.await.unwrap();
        let lease = runtime.read_lease(Network::Testnet).unwrap();
        let (driver, mut events) = Driver::new();
        let response = watch(&runtime, &driver, Network::Mainnet, None, "BTC");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );
        assert!(matches!(
            runtime.local_read(Network::Testnet, ReadKind::Pilot, || ()),
            Err(RuntimeError::Busy)
        ));
        drop(release);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );
        drop(lease);
        let binding = reply(response).await;
        assert_eq!(binding.network, Network::Mainnet);
        assert_eq!(
            event(&mut events).await,
            Event::Start(Network::Mainnet, "1".into())
        );
        assert_eq!(
            runtime
                .local_read(Network::Mainnet, ReadKind::Keychain, || 9)
                .unwrap()
                .await
                .unwrap(),
            9
        );
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn same_context_keeps_generation_feed_and_read_slots() {
        let runtime = runtime();
        let (driver, mut events) = Driver::new();
        let first = reply(watch(
            &runtime,
            &driver,
            Network::Testnet,
            Some("account-a"),
            "BTC",
        ))
        .await;
        assert_eq!(
            event(&mut events).await,
            Event::Start(Network::Testnet, "1".into())
        );
        assert_eq!(
            event(&mut events).await,
            Event::Watch("BTC".into(), "1m".into())
        );
        let (release, gate) = mpsc::channel::<()>();
        let (started, observing) = oneshot::channel();
        let read = runtime
            .local_read(Network::Testnet, ReadKind::Operator, move || {
                let _ = started.send(());
                let _ = gate.recv();
            })
            .unwrap();
        observing.await.unwrap();
        drop(read);
        let second = reply(watch(
            &runtime,
            &driver,
            Network::Testnet,
            Some("account-a"),
            "ETH",
        ))
        .await;
        assert_eq!(first, second);
        assert_eq!(
            event(&mut events).await,
            Event::Watch("ETH".into(), "1m".into())
        );
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
        let interval_change = runtime
            .submit_watch(
                FeedSource::Controlled(driver.clone()),
                Selection {
                    network: Network::Testnet,
                    account: Some("account-a".into()),
                    coin: "ETH".into(),
                    interval: "5m".into(),
                },
            )
            .unwrap();
        assert_eq!(reply(interval_change).await, first);
        assert_eq!(
            event(&mut events).await,
            Event::Watch("ETH".into(), "5m".into())
        );
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            runtime.local_read(Network::Testnet, ReadKind::Operator, || ()),
            Err(RuntimeError::Busy)
        ));
        drop(release);
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn superseded_blocked_start_survives_dropped_ipc_and_only_latest_watch_runs() {
        let runtime = runtime();
        let (driver, mut events) = Driver::new();
        let (release, gate) = mpsc::channel::<()>();
        *driver.start_gate.lock().unwrap() = Some(gate);
        let first = watch(&runtime, &driver, Network::Testnet, None, "BTC");
        assert_eq!(
            event(&mut events).await,
            Event::Start(Network::Testnet, "1".into())
        );
        drop(first);
        let middle = watch(&runtime, &driver, Network::Testnet, None, "ETH");
        let latest = watch(&runtime, &driver, Network::Testnet, None, "SOL");
        assert!(matches!(
            middle.await.unwrap(),
            Err(RuntimeError::Superseded)
        ));
        drop(release);
        assert_eq!(reply(latest).await.generation, "1");
        assert_eq!(
            event(&mut events).await,
            Event::Watch("SOL".into(), "1m".into())
        );
        assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn stop_joins_blocked_start_then_feed_work_without_observer() {
        let runtime = runtime();
        let (driver, mut events) = Driver::new();
        let (start_release, start_gate) = mpsc::channel::<()>();
        let (drain_release, drain_gate) = mpsc::channel::<()>();
        *driver.start_gate.lock().unwrap() = Some(start_gate);
        *driver.drain_gate.lock().unwrap() = Some(drain_gate);
        let observer = watch(&runtime, &driver, Network::Testnet, None, "BTC");
        assert_eq!(
            event(&mut events).await,
            Event::Start(Network::Testnet, "1".into())
        );
        drop(observer);
        pending(&runtime).await;
        assert!(matches!(
            runtime.submit_watch(
                FeedSource::Controlled(driver.clone()),
                Selection {
                    network: Network::Mainnet,
                    account: None,
                    coin: "ETH".into(),
                    interval: "1m".into(),
                }
            ),
            Err(RuntimeError::Stopping)
        ));
        assert!(matches!(
            runtime.read_lease(Network::Testnet),
            Err(RuntimeError::Stopping)
        ));
        drop(start_release);
        assert_eq!(event(&mut events).await, Event::Drain);
        pending(&runtime).await;
        drop(drain_release);
        drained(&runtime).await.unwrap();
        assert_eq!(event(&mut events).await, Event::Joined);
        assert_eq!(runtime.status().phase, RuntimePhase::Stopped);
    }

    #[tokio::test]
    async fn replacement_drains_previous_context_and_increments_generation() {
        let runtime = runtime();
        let (old, mut old_events) = Driver::new();
        let (release, gate) = mpsc::channel::<()>();
        *old.drain_gate.lock().unwrap() = Some(gate);
        let first = reply(watch(&runtime, &old, Network::Testnet, Some("a"), "BTC")).await;
        event(&mut old_events).await;
        event(&mut old_events).await;
        let (new, mut new_events) = Driver::new();
        let next = watch(&runtime, &new, Network::Testnet, Some("b"), "ETH");
        assert_eq!(event(&mut old_events).await, Event::Drain);
        assert_eq!(new.starts.load(Ordering::SeqCst), 0);
        assert!(matches!(
            runtime.read_lease(Network::Testnet),
            Err(RuntimeError::Busy)
        ));
        drop(release);
        let second = reply(next).await;
        assert_ne!(first.generation, second.generation);
        assert_eq!(event(&mut old_events).await, Event::Joined);
        assert_eq!(
            event(&mut new_events).await,
            Event::Start(Network::Testnet, "2".into())
        );
        assert_eq!(
            event(&mut new_events).await,
            Event::Watch("ETH".into(), "1m".into())
        );
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn completed_feed_failure_still_waits_local_and_async_reads() {
        let runtime = runtime();
        let (driver, mut events) = Driver::new();
        let (drain_release, drain_gate) = mpsc::channel::<()>();
        *driver.drain_gate.lock().unwrap() = Some(drain_gate);
        reply(watch(&runtime, &driver, Network::Testnet, None, "BTC")).await;
        event(&mut events).await;
        event(&mut events).await;
        let lease = runtime.read_lease(Network::Testnet).unwrap();
        let (release, gate) = mpsc::channel::<()>();
        let (started, observing) = oneshot::channel();
        drop(
            runtime
                .local_read(Network::Testnet, ReadKind::Keychain, move || {
                    let _ = started.send(());
                    let _ = gate.recv();
                })
                .unwrap(),
        );
        observing.await.unwrap();
        driver.fail_drain.store(true, Ordering::SeqCst);
        driver.fail_watch.store(true, Ordering::SeqCst);
        let failed = watch(&runtime, &driver, Network::Testnet, None, "ETH");
        assert!(matches!(
            failed.await.unwrap(),
            Err(RuntimeError::Failed(_))
        ));
        assert_eq!(event(&mut events).await, Event::Drain);
        drop(drain_release);
        assert_eq!(event(&mut events).await, Event::Joined);
        pending(&runtime).await;
        drop(release);
        pending(&runtime).await;
        drop(lease);
        assert!(matches!(drained(&runtime).await,
            Err(RuntimeError::Failed(detail)) if detail == "controlled consumer failure"));
        assert_eq!(runtime.status().phase, RuntimePhase::StoppedWithError);
        assert!(runtime.control().claim_exit());
        assert!(runtime.exit_allowed());
    }

    #[tokio::test]
    async fn panicked_watch_is_joined_and_remaining_feed_work_is_drained() {
        let runtime = runtime();
        let (driver, mut events) = Driver::new();
        let (release, gate) = mpsc::channel::<()>();
        *driver.drain_gate.lock().unwrap() = Some(gate);
        reply(watch(&runtime, &driver, Network::Testnet, None, "BTC")).await;
        event(&mut events).await;
        event(&mut events).await;
        driver.panic_watch.store(true, Ordering::SeqCst);
        let failed = watch(&runtime, &driver, Network::Testnet, None, "ETH");
        assert!(failed.await.is_err());
        assert_eq!(event(&mut events).await, Event::Drain);
        pending(&runtime).await;
        drop(release);
        assert!(drained(&runtime).await.is_err());
        assert_eq!(event(&mut events).await, Event::Joined);
    }

    #[tokio::test]
    async fn context_replacement_waits_both_local_and_http_read_lifetimes() {
        let runtime = runtime();
        let (old, mut events) = Driver::new();
        reply(watch(&runtime, &old, Network::Testnet, None, "BTC")).await;
        event(&mut events).await;
        event(&mut events).await;
        let lease = runtime.read_lease(Network::Testnet).unwrap();
        let (release, gate) = mpsc::channel::<()>();
        let (started, observing) = oneshot::channel();
        drop(
            runtime
                .local_read(Network::Testnet, ReadKind::Pilot, move || {
                    let _ = started.send(());
                    let _ = gate.recv();
                })
                .unwrap(),
        );
        observing.await.unwrap();
        let (new, mut new_events) = Driver::new();
        let response = watch(&runtime, &new, Network::Mainnet, None, "ETH");
        assert_eq!(event(&mut events).await, Event::Drain);
        assert_eq!(event(&mut events).await, Event::Joined);
        assert_eq!(new.starts.load(Ordering::SeqCst), 0);
        drop(release);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), new_events.recv())
                .await
                .is_err()
        );
        drop(lease);
        let binding = reply(response).await;
        assert_eq!(binding.network, Network::Mainnet);
        assert_eq!(binding.generation, "2");
        assert!(matches!(
            runtime.read_lease(Network::Testnet),
            Err(RuntimeError::ContextChanged)
        ));
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn delayed_failure_cannot_reverse_completed_shutdown() {
        for failed_drain in [false, true] {
            let runtime = runtime();
            let (driver, mut events) = Driver::new();
            let (release, gate) = mpsc::channel::<()>();
            *driver.drain_gate.lock().unwrap() = Some(gate);
            driver.fail_drain.store(failed_drain, Ordering::SeqCst);
            reply(watch(&runtime, &driver, Network::Testnet, None, "BTC")).await;
            event(&mut events).await;
            event(&mut events).await;
            *driver.reported_failure.lock().unwrap() = Some("delayed feed failure".into());
            // Pause status delivery after its feed lookup, then let the real
            // owner finish draining before delivering the captured failure.
            let observed = runtime
                .0
                .feed
                .lock()
                .await
                .as_ref()
                .unwrap()
                .failure()
                .unwrap();
            runtime.begin_stop();
            assert_eq!(event(&mut events).await, Event::Drain);
            drop(release);
            assert_eq!(event(&mut events).await, Event::Joined);
            let completed = drained(&runtime).await;
            assert_eq!(completed.is_err(), failed_drain);
            runtime.fail(observed.clone());
            assert_eq!(runtime.status().phase, RuntimePhase::StoppedWithError);
            let expected = if failed_drain {
                "controlled consumer failure"
            } else {
                &observed
            };
            for _ in 0..2 {
                assert!(matches!(drained(&runtime).await,
                    Err(RuntimeError::Failed(detail)) if detail == expected));
            }
            assert!(runtime.control().claim_exit());
            assert!(runtime.exit_allowed());
        }
    }

    #[tokio::test]
    async fn dropped_mcp_start_observer_keeps_blocking_start_owned_through_stop() {
        let runtime = runtime();
        let account = oppen_hl::Address::from_bytes([7; 20]).to_string();
        let (release, gate) = mpsc::channel::<()>();
        let (started, observing) = oneshot::channel();
        let response = runtime
            .launch_mcp(
                "fixture-agent".into(),
                account,
                move |_, _, _, _| async move {
                    tauri::async_runtime::spawn_blocking(move || {
                        let _ = started.send(());
                        let _ = gate.recv();
                    })
                    .await
                    .unwrap();
                    Err("controlled authority startup failure".into())
                },
            )
            .unwrap();
        observing.await.unwrap();
        drop(response);
        assert_eq!(runtime.mcp_status().phase, McpPhase::Starting);
        let (driver, _) = Driver::new();
        assert!(matches!(
            runtime.submit_watch(
                FeedSource::Controlled(driver),
                Selection {
                    network: Network::Mainnet,
                    account: None,
                    coin: "BTC".into(),
                    interval: "1m".into(),
                }
            ),
            Err(RuntimeError::Busy)
        ));
        pending(&runtime).await;
        assert_eq!(runtime.mcp_status().phase, McpPhase::Stopping);
        drop(release);
        assert!(
            matches!(drained(&runtime).await, Err(RuntimeError::Failed(detail))
            if detail == "controlled authority startup failure")
        );
        assert_eq!(runtime.mcp_status().phase, McpPhase::Failed);
        assert_eq!(runtime.status().phase, RuntimePhase::StoppedWithError);
    }

    #[tokio::test]
    async fn mcp_start_rejects_existing_feed_account_before_startup_work() {
        let runtime = runtime();
        let (driver, _) = Driver::new();
        let account = oppen_hl::Address::from_bytes([7; 20]).to_string();
        reply(watch(
            &runtime,
            &driver,
            Network::Testnet,
            Some(&account),
            "BTC",
        ))
        .await;
        let other = oppen_hl::Address::from_bytes([8; 20]).to_string();
        let refused = runtime.launch_mcp("fixture-agent".into(), other, |_, _, _, _| async {
            panic!("mismatched context must not load authority");
        });
        assert!(matches!(refused, Err(RuntimeError::ContextChanged)));
        assert_eq!(runtime.mcp_status().phase, McpPhase::Idle);
        drained(&runtime).await.unwrap();
    }

    #[tokio::test]
    async fn update_claim_excludes_exit_even_after_drain_and_drop_never_reopens() {
        let runtime = runtime();
        let guard = runtime.begin_update().unwrap();
        assert!(!runtime.control().claim_exit());
        assert!(matches!(runtime.begin_update(), Err(RuntimeError::Busy)));
        guard.shutdown().await.unwrap();
        assert_eq!(runtime.status().phase, RuntimePhase::Stopped);
        assert!(!runtime.exit_allowed());
        assert!(!runtime.control().claim_exit());
        drop(guard);
        assert!(runtime.control().claim_exit());
        assert!(runtime.exit_allowed());
        assert!(matches!(
            runtime.read_lease(Network::Testnet),
            Err(RuntimeError::Stopping)
        ));
        assert!(matches!(runtime.begin_update(), Err(RuntimeError::Busy)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimePhase {
    Running,
    Replacing,
    Stopping,
    Stopped,
    StoppedWithError,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimeStatus {
    pub phase: RuntimePhase,
    pub binding: Option<FeedBinding>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub(crate) enum RuntimeError {
    Stopping,
    Busy,
    Superseded,
    ContextChanged,
    Failed(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopping => f.write_str("desktop runtime is stopping"),
            Self::Busy => f.write_str("desktop runtime is replacing its feed or a read is busy"),
            Self::Superseded => f.write_str("a newer selection superseded this watch"),
            Self::ContextChanged => f.write_str("desktop network context changed"),
            Self::Failed(detail) => f.write_str(detail),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Selection {
    network: Network,
    account: Option<String>,
    coin: String,
    interval: String,
}

impl Selection {
    fn same_context(&self, other: &Self) -> bool {
        self.network == other.network && self.account == other.account
    }
}

type WatchReply = oneshot::Sender<Result<FeedBinding, RuntimeError>>;

#[derive(Clone)]
enum FeedSource {
    Desktop(tauri::AppHandle),
    #[cfg(test)]
    Controlled(Arc<tests::Driver>),
}

// The smaller variant exists only for controlled lifecycle tests.
#[cfg_attr(test, allow(clippy::large_enum_variant))]
enum OwnedFeed {
    Desktop(ConsoleFeed),
    #[cfg(test)]
    Controlled(tests::ControlledFeed),
}

impl OwnedFeed {
    fn failure(&self) -> Option<String> {
        match self {
            Self::Desktop(feed) => feed.failure(),
            #[cfg(test)]
            Self::Controlled(feed) => feed.failure(),
        }
    }

    fn watch(&self, coin: &str, interval: &str) -> Result<(), String> {
        match self {
            Self::Desktop(feed) => feed.watch(coin, interval),
            #[cfg(test)]
            Self::Controlled(feed) => feed.watch(coin, interval),
        }
    }

    async fn shutdown_and_drain(&mut self) -> Result<(), String> {
        match self {
            Self::Desktop(feed) => feed.shutdown_and_drain().await,
            #[cfg(test)]
            Self::Controlled(feed) => feed.shutdown_and_drain().await,
        }
    }

    fn last_tick_ms(&self, network: Network) -> Option<u64> {
        match self {
            Self::Desktop(feed) => feed.serves(network).then(|| feed.last_tick_ms()).flatten(),
            #[cfg(test)]
            Self::Controlled(_) => None,
        }
    }
}

struct PendingWatch {
    source: FeedSource,
    selection: Selection,
    serial: u64,
    reply: WatchReply,
}

struct Control {
    phase: RuntimePhase,
    terminal: bool,
    detail: Option<String>,
    network: Option<Network>,
    generation: u64,
    request_serial: u64,
    binding: Option<FeedBinding>,
    selection: Option<Selection>,
    desired: Option<PendingWatch>,
    worker_running: bool,
    worker: Option<tauri::async_runtime::JoinHandle<()>>,
    drain: Option<tauri::async_runtime::JoinHandle<()>>,
    reads: LocalReads,
    active_reads: usize,
    terminal_mode: Option<TerminalMode>,
    exit_task: Option<tauri::async_runtime::JoinHandle<()>>,
    mcp_binding: Option<Binding>,
    mcp_worker: Option<tauri::async_runtime::JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalMode {
    Exit,
    Update,
}

impl Control {
    fn claim_exit(&mut self) -> bool {
        if self.terminal_mode == Some(TerminalMode::Update) || self.exit_task.is_some() {
            return false;
        }
        self.terminal_mode = Some(TerminalMode::Exit);
        self.terminal = true;
        self.reads.begin_stop();
        true
    }
}

/// Keep inside the retained updater task through actual installer completion.
pub(crate) struct UpdateGuard(Runtime);

impl UpdateGuard {
    pub(crate) async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.0.shutdown().await
    }
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        let mut control = self.0.control();
        if control.terminal_mode == Some(TerminalMode::Update) {
            control.terminal_mode = None;
        }
        // Dropping an update claim never reopens read/watch admission.
    }
}

struct Inner {
    data_dir: PathBuf,
    control: Mutex<Control>,
    feed: AsyncMutex<Option<OwnedFeed>>,
    changed: Notify,
    mcp: AsyncMutex<Option<OwnedMcp>>,
    mcp_status: SharedStatus,
    mcp_stop: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct Runtime(Arc<Inner>);

/// Lives in the actual async read future, not a spawned timeout wrapper.
pub(crate) struct ReadLease(Runtime);

impl Drop for ReadLease {
    fn drop(&mut self) {
        let mut control = self.0.control();
        control.active_reads -= 1;
        drop(control);
        self.0.0.changed.notify_waiters();
    }
}

impl Runtime {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self(Arc::new(Inner {
            data_dir,
            control: Mutex::new(Control {
                phase: RuntimePhase::Running,
                terminal: false,
                detail: None,
                network: None,
                generation: 0,
                request_serial: 0,
                binding: None,
                selection: None,
                desired: None,
                worker_running: false,
                worker: None,
                drain: None,
                reads: LocalReads::default(),
                active_reads: 0,
                terminal_mode: None,
                exit_task: None,
                mcp_binding: None,
                mcp_worker: None,
            }),
            feed: AsyncMutex::new(None),
            changed: Notify::new(),
            mcp: AsyncMutex::new(None),
            mcp_status: Arc::new(Mutex::new(McpStatus::idle())),
            mcp_stop: CancellationToken::new(),
        }))
    }

    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.0.control.lock().unwrap_or_else(|poison| {
            let mut control = poison.into_inner();
            control.terminal = true;
            control.phase = RuntimePhase::Stopping;
            control.detail = Some("desktop owner lock poisoned".into());
            control.reads.begin_stop();
            control
        })
    }

    pub(crate) fn data_dir(&self) -> &std::path::Path {
        &self.0.data_dir
    }

    pub(crate) fn status(&self) -> RuntimeStatus {
        self.mcp_status();
        let failure = self
            .0
            .feed
            .try_lock()
            .ok()
            .and_then(|feed| feed.as_ref().and_then(OwnedFeed::failure));
        if let Some(detail) = failure {
            self.fail(detail);
        }
        let control = self.control();
        RuntimeStatus {
            phase: control.phase,
            binding: control.binding.clone(),
            detail: control.detail.clone(),
        }
    }

    /// Cached only: this path never opens authority or touches the keychain.
    pub(crate) fn mcp_status(&self) -> McpStatus {
        let status = status_lock(&self.0.mcp_status).clone();
        if status.phase == McpPhase::Failed {
            self.fail(
                status
                    .detail
                    .clone()
                    .unwrap_or_else(|| "MCP owner failed".into()),
            );
        }
        status
    }

    pub(crate) fn start_mcp(
        &self,
        agent: String,
        account: String,
    ) -> Result<oneshot::Receiver<Result<McpStatus, RuntimeError>>, RuntimeError> {
        self.launch_mcp(agent, account, OwnedMcp::start)
    }

    pub(super) fn launch_mcp<F, Fut>(
        &self,
        agent: String,
        account: String,
        start: F,
    ) -> Result<oneshot::Receiver<Result<McpStatus, RuntimeError>>, RuntimeError>
    where
        F: FnOnce(PathBuf, Binding, SharedStatus, CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<OwnedMcp, String>> + Send + 'static,
    {
        if agent.trim().is_empty() {
            return Err(RuntimeError::Failed("an existing agent is required".into()));
        }
        let binding = Binding {
            agent: AgentId::new(agent),
            account: account
                .parse()
                .map_err(|error| RuntimeError::Failed(format!("invalid MCP account: {error}")))?,
        };
        self.status();
        let (reply, observing) = oneshot::channel();
        let mut control = self.control();
        Self::admit(&mut control, Network::Testnet)?;
        if control.mcp_binding.is_some() {
            return Err(RuntimeError::Busy);
        }
        if control.selection.as_ref().is_some_and(|selection| {
            selection.network != Network::Testnet
                || selection
                    .account
                    .as_deref()
                    .and_then(|account| account.parse().ok())
                    != Some(binding.account)
        }) {
            return Err(RuntimeError::ContextChanged);
        }
        control.mcp_binding = Some(binding.clone());
        *status_lock(&self.0.mcp_status) = McpStatus::starting(&binding);
        let runtime = self.clone();
        control.mcp_worker = Some(tauri::async_runtime::spawn(async move {
            let owner = runtime.clone();
            let result = tauri::async_runtime::spawn(async move {
                start(
                    owner.0.data_dir.clone(),
                    binding,
                    owner.0.mcp_status.clone(),
                    owner.0.mcp_stop.clone(),
                )
                .await
            })
            .await
            .map_err(|error| format!("MCP startup owner: {error}"))
            .and_then(|result| result);
            match result {
                Ok(owned) => {
                    *runtime.0.mcp.lock().await = Some(owned);
                    let response = if runtime.control().terminal {
                        Err(RuntimeError::Stopping)
                    } else {
                        Ok(runtime.mcp_status())
                    };
                    let _ = reply.send(response);
                }
                Err(detail) => {
                    {
                        let mut status = status_lock(&runtime.0.mcp_status);
                        status.phase = McpPhase::Failed;
                        status.detail = Some(detail.clone());
                        status.listener = None;
                    }
                    runtime.fail(detail.clone());
                    let _ = reply.send(Err(RuntimeError::Failed(detail)));
                }
            }
        }));
        Ok(observing)
    }

    fn admit(control: &mut Control, network: Network) -> Result<(), RuntimeError> {
        if control.terminal {
            return Err(RuntimeError::Stopping);
        }
        if control.phase != RuntimePhase::Running {
            return Err(RuntimeError::Busy);
        }
        if control.network.is_some_and(|current| current != network) {
            return Err(RuntimeError::ContextChanged);
        }
        control.network = Some(network);
        Ok(())
    }

    pub(crate) fn read_lease(&self, network: Network) -> Result<ReadLease, RuntimeError> {
        self.status();
        let mut control = self.control();
        Self::admit(&mut control, network)?;
        control.active_reads += 1;
        Ok(ReadLease(self.clone()))
    }

    pub(crate) fn local_read<T: Send + 'static>(
        &self,
        network: Network,
        kind: ReadKind,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Result<tauri::async_runtime::JoinHandle<T>, RuntimeError> {
        self.status();
        let mut control = self.control();
        Self::admit(&mut control, network)?;
        control
            .reads
            .spawn(kind, read)
            .map_err(|error| match error {
                crate::local_reads::ReadError::Busy => RuntimeError::Busy,
                crate::local_reads::ReadError::Stopping => RuntimeError::Stopping,
            })
    }

    /// Submission is synchronous. Only its reply belongs to the IPC caller.
    pub(crate) fn watch(
        &self,
        app: tauri::AppHandle,
        network: Network,
        account: Option<String>,
        coin: String,
        interval: String,
    ) -> Result<oneshot::Receiver<Result<FeedBinding, RuntimeError>>, RuntimeError> {
        self.submit_watch(
            FeedSource::Desktop(app),
            Selection {
                network,
                account,
                coin,
                interval,
            },
        )
    }

    fn submit_watch(
        &self,
        source: FeedSource,
        selection: Selection,
    ) -> Result<oneshot::Receiver<Result<FeedBinding, RuntimeError>>, RuntimeError> {
        self.status();
        let (reply, receiver) = oneshot::channel();
        let mut control = self.control();
        if control.terminal {
            return Err(RuntimeError::Stopping);
        }
        if control.mcp_binding.as_ref().is_some_and(|binding| {
            selection.network != Network::Testnet
                || selection
                    .account
                    .as_deref()
                    .and_then(|account| account.parse().ok())
                    != Some(binding.account)
        }) {
            return Err(RuntimeError::Busy);
        }
        if !control.worker_running
            && control.selection.as_ref() == Some(&selection)
            && let Some(binding) = &control.binding
        {
            let _ = reply.send(Ok(binding.clone()));
            return Ok(receiver);
        }
        control.request_serial = control
            .request_serial
            .checked_add(1)
            .ok_or_else(|| RuntimeError::Failed("watch request sequence exhausted".into()))?;
        let pending = PendingWatch {
            source,
            selection,
            serial: control.request_serial,
            reply,
        };
        if let Some(previous) = control.desired.replace(pending) {
            let _ = previous.reply.send(Err(RuntimeError::Superseded));
        }
        control.phase = RuntimePhase::Replacing;
        if !control.worker_running {
            control.worker_running = true;
            let previous = control.worker.take();
            let runtime = self.clone();
            control.worker = Some(tauri::async_runtime::spawn(async move {
                if let Some(previous) = previous
                    && let Err(error) = previous.await
                {
                    runtime.fail(format!("feed owner task: {error}"));
                    return;
                }
                // Retain and observe the actual operation even if it panics.
                let operation = runtime.clone();
                if let Err(error) =
                    tauri::async_runtime::spawn(async move { operation.run_watches().await }).await
                {
                    runtime.fail(format!("feed operation task: {error}"));
                }
            }));
        }
        Ok(receiver)
    }

    async fn run_watches(&self) {
        loop {
            let pending = {
                let mut control = self.control();
                if control.terminal {
                    control.worker_running = false;
                    return;
                }
                match control.desired.take() {
                    Some(pending) => pending,
                    None => {
                        control.worker_running = false;
                        return;
                    }
                }
            };
            let result = self.replace_feed(&pending).await;
            let mut control = self.control();
            let result = match result {
                Ok(_) if control.terminal => Err(RuntimeError::Stopping),
                Ok(_) if control.request_serial != pending.serial => Err(RuntimeError::Superseded),
                Ok(Some(binding)) => {
                    control.selection = Some(pending.selection);
                    control.binding = Some(binding.clone());
                    control.phase = RuntimePhase::Running;
                    Ok(binding)
                }
                Ok(None) => Err(RuntimeError::Superseded),
                Err(error) => {
                    control.terminal = true;
                    control.phase = RuntimePhase::Stopping;
                    control.detail = Some(error.to_string());
                    control.reads.begin_stop();
                    if let Some(desired) = control.desired.take() {
                        let _ = desired.reply.send(Err(RuntimeError::Stopping));
                    }
                    Err(error)
                }
            };
            let _ = pending.reply.send(result);
            drop(control);
            self.0.changed.notify_waiters();
            if self.control().terminal {
                self.begin_stop();
            }
        }
    }

    async fn replace_feed(
        &self,
        pending: &PendingWatch,
    ) -> Result<Option<FeedBinding>, RuntimeError> {
        let mut slot = self.0.feed.lock().await;
        let existing = {
            let control = self.control();
            if control.terminal || control.request_serial != pending.serial {
                return Ok(None);
            }
            control
                .selection
                .as_ref()
                .filter(|selection| selection.same_context(&pending.selection))
                .and(control.binding.clone())
        };
        if let (Some(feed), Some(binding)) = (slot.as_ref(), existing) {
            feed.watch(&pending.selection.coin, &pending.selection.interval)
                .map_err(RuntimeError::Failed)?;
            return Ok(Some(binding));
        }
        let reads = {
            let control = self.control();
            // Attaching the first market feed does not replace a same-network
            // read context. In particular, a wallet probe cannot gate markets.
            let initial_attach = slot.is_none()
                && control.selection.is_none()
                && control
                    .network
                    .is_none_or(|network| network == pending.selection.network);
            if initial_attach {
                None
            } else {
                control.reads.begin_stop();
                Some(control.reads.clone())
            }
        };
        let failure = match slot.as_mut() {
            Some(feed) => feed.shutdown_and_drain().await.err(),
            None => None,
        };
        *slot = None;
        if let Some(reads) = reads {
            reads.drain().await;
            self.drain_async_reads().await;
            let mut control = self.control();
            if !control.terminal {
                // Even a superseded replacement has closed the previous owner.
                // Admission remains blocked until the latest watch completes.
                control.reads = LocalReads::default();
            }
        }
        if let Some(error) = failure {
            return Err(RuntimeError::Failed(error));
        }
        let binding = {
            let mut control = self.control();
            if control.terminal || control.request_serial != pending.serial {
                return Ok(None);
            }
            control.generation = control
                .generation
                .checked_add(1)
                .ok_or_else(|| RuntimeError::Failed("feed generation exhausted".into()))?;
            control.network = Some(pending.selection.network);
            control.binding = None;
            FeedBinding {
                network: pending.selection.network,
                generation: control.generation.to_string(),
            }
        };
        let source = pending.source.clone();
        let dir = self.0.data_dir.clone();
        let selection = pending.selection.clone();
        let generation = binding.generation.clone();
        let feed = tauri::async_runtime::spawn_blocking(move || match source {
            FeedSource::Desktop(app) => {
                ConsoleFeed::start(&app, &dir, selection.network, generation, selection.account)
                    .map(OwnedFeed::Desktop)
            }
            #[cfg(test)]
            FeedSource::Controlled(driver) => driver
                .start(selection.network, generation)
                .map(OwnedFeed::Controlled),
        })
        .await
        .map_err(|error| RuntimeError::Failed(format!("feed start task: {error}")))?
        .map_err(RuntimeError::Failed)?;
        *slot = Some(feed);
        {
            let mut control = self.control();
            // Retain the actual context even if a newer watch arrived during start.
            control.selection = Some(pending.selection.clone());
            control.binding = Some(binding.clone());
            if control.terminal || control.request_serial != pending.serial {
                return Ok(None);
            }
        }
        slot.as_ref()
            .expect("feed ownership installed")
            .watch(&pending.selection.coin, &pending.selection.interval)
            .map_err(RuntimeError::Failed)?;
        Ok(Some(binding))
    }

    fn fail(&self, detail: String) {
        let mut control = self.control();
        control.terminal = true;
        if matches!(
            control.phase,
            RuntimePhase::Stopped | RuntimePhase::StoppedWithError
        ) {
            // A status reader may have captured this failure before drain
            // completed. Completion is factual and cannot be reversed.
            control.phase = RuntimePhase::StoppedWithError;
            control.detail.get_or_insert(detail);
        } else {
            control.phase = RuntimePhase::Stopping;
            control.detail = Some(detail);
        }
        control.reads.begin_stop();
        if let Some(pending) = control.desired.take() {
            let _ = pending.reply.send(Err(RuntimeError::Stopping));
        }
        drop(control);
        self.0.changed.notify_waiters();
        self.begin_stop();
    }

    pub(crate) fn begin_update(&self) -> Result<UpdateGuard, RuntimeError> {
        {
            let mut control = self.control();
            if control.terminal_mode.is_some() {
                return Err(RuntimeError::Busy);
            }
            if control.terminal {
                return Err(RuntimeError::Stopping);
            }
            control.terminal_mode = Some(TerminalMode::Update);
            control.terminal = true;
            control.reads.begin_stop();
        }
        self.begin_stop();
        Ok(UpdateGuard(self.clone()))
    }

    /// Called only after preventing the ordinary Tauri exit request.
    pub(crate) fn request_exit(&self, app: tauri::AppHandle, code: i32) {
        let mut control = self.control();
        if !control.claim_exit() {
            return;
        }
        let runtime = self.clone();
        control.exit_task = Some(tauri::async_runtime::spawn(async move {
            let _ = runtime.shutdown().await;
            if runtime.exit_allowed() {
                app.exit(code);
            }
        }));
    }

    pub(crate) fn exit_allowed(&self) -> bool {
        let control = self.control();
        control.terminal_mode == Some(TerminalMode::Exit)
            && matches!(
                control.phase,
                RuntimePhase::Stopped | RuntimePhase::StoppedWithError
            )
    }

    pub(crate) fn begin_stop(&self) {
        let mut control = self.control();
        control.terminal = true;
        control.reads.begin_stop();
        self.0.mcp_stop.cancel();
        {
            let mut status = status_lock(&self.0.mcp_status);
            if matches!(status.phase, McpPhase::Starting | McpPhase::Listening) {
                status.phase = McpPhase::Stopping;
            }
        }
        if matches!(
            control.phase,
            RuntimePhase::Stopped | RuntimePhase::StoppedWithError
        ) || control.drain.is_some()
        {
            return;
        }
        control.phase = RuntimePhase::Stopping;
        control.binding = None;
        if let Some(pending) = control.desired.take() {
            let _ = pending.reply.send(Err(RuntimeError::Stopping));
        }
        let worker = control.worker.take();
        let mcp_worker = control.mcp_worker.take();
        let runtime = self.clone();
        control.drain = Some(tauri::async_runtime::spawn(async move {
            if let Some(worker) = mcp_worker
                && let Err(error) = worker.await
            {
                runtime.fail(format!("MCP startup join: {error}"));
            }
            {
                let mut mcp = runtime.0.mcp.lock().await;
                if let Some(owned) = mcp.as_mut()
                    && let Err(error) = owned.shutdown_and_drain().await
                {
                    runtime.fail(error);
                }
                *mcp = None;
            }
            if let Some(worker) = worker
                && let Err(error) = worker.await
            {
                runtime.fail(format!("feed owner join: {error}"));
            }
            let reads = runtime.control().reads.clone();
            let mut slot = runtime.0.feed.lock().await;
            if let Some(feed) = slot.as_mut()
                && let Err(error) = feed.shutdown_and_drain().await
            {
                runtime.fail(error);
            }
            *slot = None;
            drop(slot);
            reads.drain().await;
            runtime.drain_async_reads().await;
            let mut control = runtime.control();
            control.phase = if control.detail.is_none() {
                RuntimePhase::Stopped
            } else {
                RuntimePhase::StoppedWithError
            };
            drop(control);
            runtime.0.changed.notify_waiters();
        }));
    }

    async fn drain_async_reads(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.control().active_reads == 0 {
                return;
            }
            changed.await;
        }
    }

    /// Cancel-safe waiter. The owner retains the actual drain task.
    pub(crate) async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.begin_stop();
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let status = self.status();
            if status.phase == RuntimePhase::Stopped {
                return Ok(());
            }
            if status.phase == RuntimePhase::StoppedWithError {
                return Err(RuntimeError::Failed(
                    status
                        .detail
                        .unwrap_or_else(|| "desktop work failed while draining".into()),
                ));
            }
            changed.await;
        }
    }

    pub(crate) fn last_tick_ms(&self, network: Network) -> Option<u64> {
        let feed = self.0.feed.try_lock().ok()?;
        feed.as_ref()?.last_tick_ms(network)
    }
}
