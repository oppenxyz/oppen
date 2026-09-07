//! Registry refusal through real MCP without any venue read or signer access.

use super::*;

#[tokio::test]
async fn wrong_pairing_retired_and_changed_routes_refuse_before_venue_or_signing() {
    for case in ["wrong_pairing", "retired", "changed"] {
        let dir = tempfile::tempdir().unwrap();
        let venue = Venue::start().await;
        let keys = Arc::new(FixtureKeys::default());
        let mut runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
        let agent = AgentId::new("fixture-agent");
        let route = runtime.registry.route_for_agent(&agent).unwrap();
        if case == "wrong_pairing" {
            runtime.token = runtime
                .pairings
                .write()
                .unwrap()
                .issue(Binding {
                    agent: agent.clone(),
                    account: Address::from_bytes([8; 20]),
                })
                .unwrap()
                .reveal()
                .to_owned();
        } else {
            assert!(runtime.registry.retire(&route, now_ms()).unwrap());
            if case == "changed" {
                let mut binding = route.binding.clone();
                binding.container = Address::from_bytes([8; 20]);
                binding.wallet.address = Address::from_bytes([7; 20]);
                runtime.registry.grant(binding, now_ms()).unwrap();
            }
        }
        let info_before = venue.info_count();
        let reads_before = keys.read_heads.lock().unwrap().len();
        for (tool, params) in [
            ("place", place(Cloid::from_bytes([91; 16]).as_str(), "0.12")),
            ("cancel", json!({"oid": 1, "reason": "registry refusal"})),
            ("cancel_all", json!({"reason": "registry refusal"})),
            (
                "close_position",
                json!({"symbol": "TEST", "reason": "registry refusal"}),
            ),
            (
                "preflight",
                json!({"symbol": "TEST", "is_buy": true, "size": "0.12", "limit_px": "100"}),
            ),
            ("get_state", json!({})),
            ("get_order_status", json!({"oid": 1})),
        ] {
            let reply = runtime.call(tool, params).await;
            let code = &reply["protocol_error"]["data"]["code"];
            assert!(
                code == "unavailable" || code == "guardrail_reject",
                "{case}/{tool}: {reply}"
            );
            assert_eq!(venue.info_count(), info_before, "{case}/{tool} read venue");
            assert!(venue.submissions().is_empty(), "{case}/{tool} submitted");
            assert_eq!(
                keys.read_heads.lock().unwrap().len(),
                reads_before,
                "{case}/{tool} read key store"
            );
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
        }
        runtime.shutdown().await;
        venue.shutdown().await;
    }
}

#[tokio::test]
async fn physical_restart_replays_registry_grant_without_minting_another() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Venue::start().await;
    let keys = Arc::new(FixtureKeys::default());
    let runtime = Runtime::open(dir.path(), venue.port(), keys.clone()).await;
    let route = runtime
        .registry
        .route_for_agent(&AgentId::new("fixture-agent"))
        .unwrap();
    runtime.shutdown().await;
    let runtime = Runtime::open(dir.path(), venue.port(), keys).await;
    assert_eq!(
        runtime
            .registry
            .route_for_agent(&AgentId::new("fixture-agent"))
            .unwrap(),
        route
    );
    let events = runtime.ledger.get_events(0, 100).unwrap();
    assert_eq!(
        events
            .events
            .iter()
            .filter(|event| event.kind == EventKind::RegistryGranted)
            .count(),
        1
    );
    runtime.shutdown().await;
    venue.shutdown().await;
}
