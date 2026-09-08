//! A native activation review pins pairing identity and actual listener drain.

use super::*;

#[tokio::test]
async fn activation_admission_pins_revocation_without_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let mut runtime =
        Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    let serving = Serving::start(&runtime).await;
    let binding = binding(&runtime);
    let admission = serving.control.activation_admission(&binding).unwrap();
    assert_eq!(admission.binding(), &binding);
    let held = admission.check().unwrap();
    assert!(runtime.pairings.try_write().is_err());
    drop(held);

    let replacement = {
        let mut pairings = runtime.pairings.write().unwrap();
        assert!(pairings.revoke(admission.pairing_id()).unwrap());
        pairings.issue(binding.clone()).unwrap()
    };
    assert!(
        admission.check().is_err(),
        "a replacement cannot revive a review"
    );
    let next = serving.control.activation_admission(&binding).unwrap();
    assert_eq!(next.pairing_id(), replacement.id);
    assert!(next.check().is_ok());
    // Only transport teardown uses the new token; the original pin stays revoked.
    runtime.token = replacement.reveal().to_owned();
    let mut wrong = binding.clone();
    wrong.account = Address::from_bytes([99; 20]);
    assert!(serving.control.activation_admission(&wrong).is_err());
    serving.control.close();
    assert!(next.check().is_err());
    assert!(serving.control.activation_admission(&binding).is_err());
    drop(next);
    drop(admission);
    serving.finish().await;
    assert!(venue.submissions().is_empty());
    runtime.shutdown().await;
    venue.shutdown().await;
}

#[tokio::test]
async fn activation_admission_retains_listener_until_actual_owner_drops() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let runtime = Runtime::open(dir.path(), venue.port(), Arc::new(FixtureKeys::default())).await;
    let mut serving = Serving::start(&runtime).await;
    let admission = serving
        .control
        .activation_admission(&binding(&runtime))
        .unwrap();
    serving.control.close();
    serving.stop.cancel();
    assert!(admission.check().is_err());
    assert!(
        timeout(Duration::from_millis(50), serving.task.as_mut().unwrap())
            .await
            .is_err(),
        "closing admission does not complete retained work"
    );
    drop(admission);
    serving.finish().await;
    assert!(venue.submissions().is_empty());
    runtime.shutdown().await;
    venue.shutdown().await;
}
