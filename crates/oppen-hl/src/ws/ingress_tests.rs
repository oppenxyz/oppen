use super::*;
use crate::Address;

fn account_event() -> WsEvent {
    WsEvent::UserFills {
        user: Address::from_bytes([1; 20]),
        is_snapshot: false,
        fills: vec![],
    }
}

#[tokio::test]
async fn registration_precedes_poll_and_ack_never_restores_old_observation() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    let before = monitor.observation();
    assert!(monitor.admit(&before).is_ok());
    let send = tx.send(account_event(), 17);
    assert!(matches!(monitor.admit(&before), Err(IngressError::Pending)));
    let registered = monitor.observation();
    send.await.unwrap();
    let frame = rx.recv().await.unwrap();
    assert_eq!(frame.received_at_ms(), 17);
    assert_eq!(frame.event(), &account_event());
    assert!(matches!(
        monitor.admit(&registered),
        Err(IngressError::Pending)
    ));
    frame.acknowledge();
    assert!(matches!(monitor.admit(&before), Err(IngressError::Changed)));
    assert!(monitor.admit(&registered).is_ok());
    drop(tx);
    assert!(rx.recv().await.is_none());
    assert!(
        !monitor.replacement_eligible(),
        "EOF is not consumer completion"
    );
    rx.complete().unwrap();
    assert!(monitor.replacement_eligible());
    assert!(monitor.status().completed);
    assert!(matches!(
        monitor.admit(&registered),
        Err(IngressError::Closed)
    ));
}

#[tokio::test]
async fn capacity_wait_remains_pending_after_first_dequeue_and_ack() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    tx.send(account_event(), 1).await.unwrap();
    let second = tx.send(account_event(), 2);
    tokio::pin!(second);
    assert!(
        std::future::Future::poll(
            second.as_mut(),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        )
        .is_pending()
    );
    rx.recv().await.unwrap().acknowledge();
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Pending)
    ));
    second.await.unwrap();
    rx.recv().await.unwrap().acknowledge();
    assert!(monitor.admit(&monitor.observation()).is_ok());
}

#[tokio::test]
async fn dropped_send_even_unpolled_permanently_fails() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    drop(tx.send(account_event(), 1));
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Failed)
    ));
    tx.send(account_event(), 2).await.unwrap();
    let later = rx.recv().await.unwrap();
    assert_eq!(later.event(), &account_event());
    assert_eq!(later.received_at_ms(), 2);
    later.acknowledge();
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Failed)
    ));
    drop(tx);
    assert!(rx.recv().await.is_none());
    assert!(rx.complete().is_err());
    assert!(!monitor.replacement_eligible());
}

#[tokio::test]
async fn canceled_capacity_wait_fails_even_if_queued_frame_is_later_acknowledged() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    tx.send(account_event(), 1).await.unwrap();
    {
        let blocked = tx.send(account_event(), 2);
        tokio::pin!(blocked);
        assert!(
            std::future::Future::poll(
                blocked.as_mut(),
                &mut std::task::Context::from_waker(std::task::Waker::noop()),
            )
            .is_pending()
        );
    }
    rx.recv().await.unwrap().acknowledge();
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Failed)
    ));
}

#[tokio::test]
async fn dequeued_unacknowledged_frame_latches_abandonment() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    tx.send(account_event(), 1).await.unwrap();
    let frame = rx.recv().await.unwrap();
    drop(tx);
    assert!(rx.recv().await.is_none());
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Pending)
    ));
    drop(frame);
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Failed)
    ));
    assert!(rx.complete().is_err());
    assert!(!monitor.replacement_eligible());
}

#[tokio::test]
async fn failed_send_and_receiver_drop_cannot_be_reset() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    rx.close();
    assert!(tx.send(account_event(), 1).await.is_err());
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Failed)
    ));
    drop(tx);
    assert!(rx.recv().await.is_none());
    assert!(rx.complete().is_err());
    let (_tx, rx) = event_channel(1);
    let dropped = rx.monitor();
    drop(rx);
    assert!(matches!(
        dropped.admit(&dropped.observation()),
        Err(IngressError::Failed)
    ));
}

#[tokio::test]
async fn completion_refuses_outstanding_work_and_surviving_producers() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    tx.send(account_event(), 1).await.unwrap();
    let frame = rx.recv().await.unwrap();
    drop(tx);
    assert!(rx.recv().await.is_none());
    assert!(rx.complete().is_err());
    frame.acknowledge();
    assert!(!monitor.replacement_eligible());
    let (_tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    rx.close();
    assert!(rx.recv().await.is_none());
    assert!(rx.complete().is_err());
    assert!(!monitor.replacement_eligible());
}

#[test]
fn blocking_consumer_completes_only_after_acknowledgment() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(tx.send(account_event(), 19))
        .unwrap();
    drop(tx);
    let frame = rx.blocking_recv().unwrap();
    assert_eq!(frame.received_at_ms(), 19);
    frame.acknowledge();
    assert!(rx.blocking_recv().is_none());
    assert!(!monitor.replacement_eligible());
    rx.complete().unwrap();
    assert!(monitor.replacement_eligible());
}

#[tokio::test]
async fn foreign_observation_and_unknown_control_do_not_authorize() {
    let (_other_tx, other_rx) = event_channel(1);
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    assert!(matches!(
        monitor.admit(&other_rx.monitor().observation()),
        Err(IngressError::Changed)
    ));
    tx.send(
        WsEvent::MessageDropped {
            connection: super::super::ConnectionId(0),
            channel: "unknown".into(),
            reason: "invalid".into(),
        },
        1,
    )
    .await
    .unwrap();
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Pending)
    ));
    rx.recv().await.unwrap().acknowledge();
    assert!(monitor.admit(&monitor.observation()).is_ok());
}

#[tokio::test]
async fn market_receipt_is_not_account_work_but_abandonment_still_fails() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    let before = monitor.observation();
    tx.send(
        WsEvent::Bbo {
            coin: "BTC".into(),
            venue_time_ms: 10,
            bid: None,
            ask: None,
        },
        20,
    )
    .await
    .unwrap();
    assert!(monitor.admit(&before).is_ok());
    let frame = rx.recv().await.unwrap();
    assert_eq!(frame.received_at_ms(), 20);
    drop(frame);
    assert!(matches!(monitor.admit(&before), Err(IngressError::Failed)));
}

#[test]
fn owned_admission_serializes_new_ingress_without_holding_a_mutex_across_work() {
    let (tx, mut rx) = event_channel(1);
    let monitor = rx.monitor();
    let before = monitor.observation();
    let permit = monitor.admit(&before).unwrap();
    let (entered, entering) = std::sync::mpsc::channel();
    let (registered, registration) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        entered.send(()).unwrap();
        let send = tx.send(account_event(), 31);
        registered.send(()).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(send)
            .unwrap();
    });
    entering
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(
        registration
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while monitor.status().pending == 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(monitor.status().pending, 1);
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Pending)
    ));
    drop(permit);
    registration
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    worker.join().unwrap();
    assert_eq!(monitor.status().pending, 1);
    rx.blocking_recv().unwrap().acknowledge();
    assert!(matches!(monitor.admit(&before), Err(IngressError::Changed)));
    assert!(rx.blocking_recv().is_none());
    rx.complete().unwrap();
}

#[test]
fn panicking_admitted_work_leaves_monitor_failed() {
    let (_tx, rx) = event_channel(1);
    let monitor = rx.monitor();
    let permit = monitor.admit(&monitor.observation()).unwrap();
    assert!(
        std::panic::catch_unwind(move || {
            let _held = permit;
            panic!("synthetic admitted work failure");
        })
        .is_err()
    );
    assert!(monitor.status().failure.is_some());
    assert!(matches!(
        monitor.admit(&monitor.observation()),
        Err(IngressError::Failed)
    ));
    assert!(!monitor.replacement_eligible());
}
