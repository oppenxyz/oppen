//! Native review uses the actual listener owner and the guarded HTTP path.

use super::*;
use crate::server::{BoundServer, OperatorControl};
use oppen_core::ledger::{Anchor, HeadAnchor, LedgerError};
use oppen_hl::wire::{OrderWire, WireFloat};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[path = "approvals/cancellations.rs"]
mod cancellations;

struct Serving {
    control: OperatorControl,
    supervision: crate::server::SupervisionControl,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Serving {
    async fn start(runtime: &Runtime) -> Self {
        let bound = BoundServer::bind(0, &runtime.gateway, &runtime.pairings)
            .await
            .unwrap();
        let control = bound.operator_control();
        let supervision = bound.supervision_control();
        let mut status = bound.supervision_status();
        let stop = CancellationToken::new();
        let task = tokio::spawn(bound.serve(
            runtime.gateway.clone(),
            runtime.pairings.clone(),
            stop.clone(),
        ));
        let serving = Self {
            control,
            supervision,
            stop,
            task: Some(task),
        };
        timeout(Duration::from_secs(5), async {
            loop {
                let current = status.borrow_and_update().clone();
                if current.completed_sequence > 0 && !current.in_progress {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .expect("initial supervision did not finish");
        serving
    }

    async fn finish(mut self) {
        self.stop.cancel();
        timeout(Duration::from_secs(5), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.control.close();
        self.stop.cancel();
    }
}

fn binding(runtime: &Runtime) -> Binding {
    Binding {
        agent: AgentId::new("fixture-agent"),
        account: runtime.account,
    }
}

async fn enable_approval(runtime: &Runtime) {
    let engine = &runtime.gateway.inner.engine;
    let agent = binding(runtime).agent;
    let mut policy = engine.guardrails(&agent).unwrap();
    policy.approval_required = true;
    engine
        .operator_set_guardrails(&agent, policy, now_ms())
        .unwrap();
    runtime.activate_orders().await;
}

async fn mint(runtime: &Runtime, byte: u8) -> String {
    let reply = runtime
        .call(
            "place",
            place(Cloid::from_bytes([byte; 16]).as_str(), "0.12"),
        )
        .await;
    assert_eq!(reply["status"], "pending_approval", "{reply}");
    reply["approval_id"].as_str().unwrap().to_owned()
}

fn count(runtime: &Runtime, kind: EventKind) -> usize {
    runtime
        .ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .count()
}

fn unavailable(error: rmcp::ErrorData) {
    assert_eq!(
        error.data.as_ref().unwrap()["code"],
        "unavailable",
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocation_during_key_loading_refuses_before_crypto_and_records_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let mut runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = mint(&runtime, 133).await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let pinned = review.pairing_id();
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    *keys.read_gate.lock().unwrap() = Some(KeyReadGate {
        entered: entered.clone(),
        release: wait,
    });
    let control = serving.control.clone();
    let confirming = tokio::spawn(async move { control.confirm(review).await });
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("confirmation did not reach key loading");
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 1);
    assert!(
        runtime
            .gateway
            .inner
            .submissions
            .state(runtime.account)
            .unwrap()
            .pending
            .is_some()
    );
    assert!(venue.submissions().is_empty());
    let pairings = runtime.pairings.clone();
    let binding = binding(&runtime);
    runtime.token = timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let mut store = pairings.write().unwrap();
            let replacement = store.issue(binding).unwrap();
            assert!(store.revoke(pinned).unwrap());
            replacement.reveal().to_owned()
        }),
    )
    .await
    .expect("key loading held pairing admission before final authorization")
    .unwrap();
    release.send(()).unwrap();
    let denied = timeout(Duration::from_secs(3), confirming)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(denied["status"], "rejected", "{denied}");
    assert_eq!(
        denied["refusal"]["unevaluable"], "route_authority",
        "{denied}"
    );
    assert!(venue.submissions().is_empty());
    assert!(
        runtime
            .gateway
            .inner
            .submissions
            .state(runtime.account)
            .unwrap()
            .pending
            .is_none()
    );
    let events = runtime.ledger.get_events(0, 1000).unwrap().events;
    let resolved: Vec<_> = events
        .iter()
        .filter(|event| event.kind == EventKind::SubmissionResolved)
        .collect();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].payload.as_ref().unwrap()["outcome"]["resolution"],
        "not_sent"
    );
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
    assert!(
        runtime
            .gateway
            .inner
            .engine
            .pending_proposals(now_ms())
            .unwrap()
            .is_empty()
    );
    assert!(runtime.ledger.verify().unwrap().is_intact());
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn native_prepare_is_non_executing_and_confirm_matches_display_once() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = mint(&runtime, 130).await;
    let pending = runtime
        .gateway
        .inner
        .engine
        .pending_proposals(now_ms())
        .unwrap();
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let second = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let oppen_core::guardrail::ApprovalReviewDisplay::Order(display) = review.display().clone()
    else {
        panic!("expected order review");
    };
    assert_eq!(display.proposal_id, id);
    assert_eq!(display.account, runtime.account);
    assert_eq!(display.agent, binding(&runtime).agent);
    assert_eq!(display.route.binding.container, runtime.account);
    assert_eq!(display.px, Decimal::from(100));
    assert_eq!(display.sz, Decimal::new(12, 2));
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 0);
    assert_eq!(
        runtime
            .gateway
            .inner
            .engine
            .pending_proposals(now_ms())
            .unwrap(),
        pending
    );
    assert!(keys.read_heads.lock().unwrap().is_empty());
    assert!(venue.submissions().is_empty());

    let expected = oppen_hl::Action::Order {
        orders: vec![OrderWire {
            a: display.asset_index,
            b: display.is_buy,
            p: WireFloat::from_decimal(display.px).unwrap(),
            s: WireFloat::from_decimal(display.sz).unwrap(),
            r: display.reduce_only,
            t: display.order_type,
            c: Some(display.cloid.clone()),
        }],
        grouping: display.grouping,
        builder: display.builder,
    };
    let confirmed = serving.control.confirm(review).await.unwrap();
    assert_eq!(confirmed["status"], "resting", "{confirmed}");
    assert_eq!(venue.submissions().len(), 1);
    assert_eq!(
        venue.submissions()[0]["action"],
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 1);
    assert_eq!(count(&runtime, EventKind::SubmissionResolved), 0);
    let pending = runtime
        .gateway
        .inner
        .submissions
        .state(runtime.account)
        .unwrap()
        .pending
        .expect("resting reply retains submission evidence until an authoritative query");
    assert_eq!(pending.cloid(), &display.cloid);
    let reads = keys.read_heads.lock().unwrap().len();
    assert!(reads > 0);
    if let Ok(repeated) = serving.control.confirm(second).await {
        assert_eq!(repeated["status"], "rejected", "{repeated}");
    }
    assert_eq!(venue.submissions().len(), 1);
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 1);
    assert_eq!(keys.read_heads.lock().unwrap().len(), reads);
    assert!(runtime.ledger.verify().unwrap().is_intact());
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn native_confirm_does_not_replace_a_revoked_review_pairing_with_a_live_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let mut runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    enable_approval(&runtime).await;
    let serving = Serving::start(&runtime).await;
    let id = mint(&runtime, 131).await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let pinned = review.pairing_id();
    let pairings = runtime.pairings.clone();
    let binding = binding(&runtime);
    runtime.token = tokio::task::spawn_blocking(move || {
        let mut pairings = pairings.write().unwrap();
        let replacement = pairings.issue(binding).unwrap();
        assert_ne!(replacement.id, pinned);
        assert!(pairings.revoke(pinned).unwrap());
        assert!(pairings.authenticate(replacement.reveal()).is_ok());
        replacement.reveal().to_owned()
    })
    .await
    .unwrap();
    unavailable(serving.control.confirm(review).await.unwrap_err());
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 0);
    assert!(keys.read_heads.lock().unwrap().is_empty());
    assert!(venue.submissions().is_empty());
    assert_eq!(
        runtime
            .gateway
            .inner
            .engine
            .pending_proposals(now_ms())
            .unwrap()
            .len(),
        1
    );
    serving.finish().await;
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[derive(Debug, Default)]
struct HeldAnchor {
    head: Mutex<Option<Anchor>>,
    wait: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    entered: tokio::sync::Notify,
    signing_wait: Mutex<Option<(crate::server::Pairings, KeyReadGate)>>,
}

#[derive(Debug)]
struct AnchorHandle(Arc<HeldAnchor>);

impl HeadAnchor for AnchorHandle {
    fn load(&self) -> Result<Option<Anchor>, LedgerError> {
        let signing_gate = {
            let mut waiting = self.0.signing_wait.lock().unwrap();
            if waiting.as_ref().is_some_and(|(pairings, _)| {
                matches!(
                    pairings.try_write(),
                    Err(std::sync::TryLockError::WouldBlock)
                )
            }) {
                waiting.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = signing_gate {
            gate.entered.notify_one();
            let _ = gate.release.recv_timeout(Duration::from_secs(15));
        }
        if let Some(wait) = self.0.wait.lock().unwrap().take() {
            self.0.entered.notify_one();
            let _ = wait.recv_timeout(Duration::from_secs(15));
        }
        Ok(self.0.head.lock().unwrap().clone())
    }
    fn store(&self, head: &Anchor) -> Result<(), LedgerError> {
        *self.0.head.lock().unwrap() = Some(head.clone());
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_during_ledger_wait_refuses_at_final_signing_admission_and_drains() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let anchor = Arc::new(HeldAnchor::default());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        keys.clone(),
        Some(Box::new(AnchorHandle(anchor.clone()))),
    )
    .await;
    enable_approval(&runtime).await;
    let mut serving = Serving::start(&runtime).await;
    let id = mint(&runtime, 134).await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.signing_wait.lock().unwrap() = Some((
        runtime.pairings.clone(),
        KeyReadGate {
            entered: entered.clone(),
            release: wait,
        },
    ));
    let control = serving.control.clone();
    let confirming = tokio::spawn(async move { control.confirm(review).await });
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("signing did not acquire pinned pairing guard before ledger replay");
    assert!(matches!(
        runtime.pairings.try_write(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    assert!(venue.submissions().is_empty());
    let closing = serving.control.clone();
    timeout(
        Duration::from_millis(500),
        tokio::task::spawn_blocking(move || closing.close()),
    )
    .await
    .expect("close blocked on signing guard")
    .unwrap();
    unavailable(
        serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .err()
            .expect("closed admission"),
    );
    serving.stop.cancel();
    assert!(
        timeout(Duration::from_millis(100), serving.task.as_mut().unwrap())
            .await
            .is_err(),
        "parent returned while admitted signing still owned ledger"
    );
    release.send(()).unwrap();
    let reply = timeout(Duration::from_secs(3), confirming)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply["status"], "rejected", "{reply}");
    assert_eq!(
        reply["refusal"]["unevaluable"], "route_authority",
        "{reply}"
    );
    serving.finish().await;
    assert!(venue.submissions().is_empty());
    assert_eq!(count(&runtime, EventKind::SubmissionSigned), 0);
    assert_eq!(count(&runtime, EventKind::SubmissionAccepted), 0);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 1);
    let resolved: Vec<_> = runtime
        .ledger
        .get_events(0, 1000)
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.kind == EventKind::SubmissionResolved)
        .collect();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].payload.as_ref().unwrap()["outcome"]["resolution"],
        "not_sent"
    );
    let pending = runtime
        .gateway
        .inner
        .submissions
        .state(runtime.account)
        .unwrap()
        .pending;
    assert!(
        pending.is_none(),
        "final refusal must resolve the unsubmitted reservation"
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn native_route_timeout_retains_actual_worker_until_parent_drain() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let anchor = Arc::new(HeldAnchor::default());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        keys.clone(),
        Some(Box::new(AnchorHandle(anchor.clone()))),
    )
    .await;
    enable_approval(&runtime).await;
    // Wait for the initial sweep before arming the native route read.
    let mut serving = Serving::start(&runtime).await;
    let id = mint(&runtime, 135).await;
    let info_before = venue.info_count();
    assert_eq!(runtime.gateway.inner.decision_worker.available_permits(), 1);
    assert_eq!(runtime.gateway.inner.route_reader.available_permits(), 1);
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.wait.lock().unwrap() = Some(wait);
    let control = serving.control.clone();
    let bound = binding(&runtime);
    let proposal = id.clone();
    let started = tokio::time::Instant::now();
    let prepare = tokio::spawn(async move { control.prepare(&bound, &proposal).await });
    timeout(Duration::from_secs(2), anchor.entered.notified())
        .await
        .unwrap();
    assert_eq!(runtime.gateway.inner.route_reader.available_permits(), 0);
    assert_eq!(
        runtime.gateway.inner.decision_worker.available_permits(),
        1,
        "a supervisor/decision worker must not be the stalled read"
    );

    serving.control.close();
    unavailable(
        serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .err()
            .expect("closed native admission"),
    );
    // Stop periodic supervision before its next tick. Only the native route
    // closure is stalled, not an independently admitted supervisor worker.
    serving.stop.cancel();
    assert!(
        timeout(Duration::from_millis(100), serving.task.as_mut().unwrap())
            .await
            .is_err()
    );
    // The timeout above drops a drain observer, not the retained server task.
    let error = timeout(Duration::from_secs(6), prepare)
        .await
        .expect("native route deadline did not fire")
        .unwrap()
        .err()
        .expect("blocked route returned a review");
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert!(error.message.contains("registry read timeout"), "{error:?}");
    unavailable(error);
    assert_eq!(runtime.gateway.inner.route_reader.available_permits(), 0);
    assert_eq!(runtime.gateway.inner.decision_worker.available_permits(), 1);
    assert_eq!(
        venue.info_count(),
        info_before,
        "timed-out prepare continued into venue reads"
    );
    assert!(
        timeout(Duration::from_millis(200), serving.task.as_mut().unwrap())
            .await
            .is_err(),
        "server drained after caller timeout while native route worker was still alive"
    );
    assert!(keys.read_heads.lock().unwrap().is_empty());
    assert!(venue.submissions().is_empty());

    release.send(()).unwrap();
    serving.finish().await;
    assert_eq!(runtime.gateway.inner.route_reader.available_permits(), 1);
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 0);
    assert_eq!(venue.info_count(), info_before);
    assert!(keys.read_heads.lock().unwrap().is_empty());
    assert!(venue.submissions().is_empty());
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn parent_stop_closes_operator_admission_and_drains_abandoned_prepare_worker() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let anchor = Arc::new(HeldAnchor::default());
    let runtime = Runtime::open_with_anchor(
        dir.path(),
        venue.port(),
        keys.clone(),
        Some(Box::new(AnchorHandle(anchor.clone()))),
    )
    .await;
    enable_approval(&runtime).await;
    let mut serving = Serving::start(&runtime).await;
    let id = mint(&runtime, 132).await;
    let review = serving
        .control
        .prepare(&binding(&runtime), &id)
        .await
        .unwrap();
    let gate = venue.hold_info("portfolio");
    let control = serving.control.clone();
    let bound = binding(&runtime);
    let proposal = id.clone();
    let prepare = tokio::spawn(async move { control.prepare(&bound, &proposal).await });
    timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    *anchor.wait.lock().unwrap() = Some(wait);
    let ledger = runtime.ledger.clone();
    let holder = tokio::task::spawn_blocking(move || ledger.verify().unwrap());
    timeout(Duration::from_secs(2), anchor.entered.notified())
        .await
        .unwrap();
    gate.release.notify_one();
    timeout(Duration::from_secs(2), async {
        while runtime.gateway.inner.decision_worker.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native preparation did not reach tracked decision worker");
    let closing = serving.control.clone();
    timeout(
        Duration::from_millis(500),
        tokio::task::spawn_blocking(move || closing.close()),
    )
    .await
    .expect("native close waited on retained preparation work")
    .unwrap();
    assert!(
        timeout(Duration::from_millis(100), serving.task.as_mut().unwrap())
            .await
            .is_err(),
        "closing native admission also stopped the listener"
    );
    unavailable(
        serving
            .control
            .prepare(&binding(&runtime), &id)
            .await
            .err()
            .expect("stopped admission"),
    );
    unavailable(serving.control.confirm(review).await.unwrap_err());
    serving.stop.cancel();
    prepare.abort();
    assert!(
        prepare
            .await
            .err()
            .expect("abandoned waiter")
            .is_cancelled()
    );
    assert!(
        timeout(Duration::from_millis(100), serving.task.as_mut().unwrap())
            .await
            .is_err(),
        "dropped waiter released actual worker ownership"
    );
    release.send(()).unwrap();
    holder.await.unwrap();
    serving.finish().await;
    assert_eq!(runtime.gateway.inner.decision_worker.available_permits(), 1);
    assert_eq!(count(&runtime, EventKind::ApprovalClaimed), 0);
    assert_eq!(count(&runtime, EventKind::SubmissionStarted), 0);
    assert!(keys.read_heads.lock().unwrap().is_empty());
    assert!(venue.submissions().is_empty());
    runtime.shutdown().await;
    venue.shutdown().await;
}
