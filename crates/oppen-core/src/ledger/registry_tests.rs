use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::ledger::Appended;

fn key() -> Arc<HmacKey> {
    Arc::new(HmacKey::from_bytes([42; 32]))
}

fn binding(id: u8) -> RegistryBinding {
    RegistryBinding {
        agent: AgentId::new("synthetic-agent"),
        container: Address::from_bytes([id; 20]),
        vault_address: None,
        wallet: AgentWallet {
            generation: u32::from(id - 1),
            address: Address::from_bytes([id + 100; 20]),
            approved_at_ms: 50,
            valid_until_ms: 1_000,
        },
    }
}

fn fixture() -> (TempDir, Arc<Ledger>, RegistryJournal) {
    let dir = TempDir::new().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = RegistryJournal::open(ledger.clone(), key()).unwrap();
    (dir, ledger, journal)
}

fn append_payload(
    ledger: &Ledger,
    payload: &Value,
    kind: EventKind,
    idem: &str,
    publish: bool,
) -> Appended {
    let mut guard = ledger.lock().unwrap();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let appended = crate::ledger::append_keyed_in_tx(
        &tx,
        &NewEvent {
            kind,
            ts_ms: 100,
            agent_id: None,
            payload,
            snapshot: None,
        },
        idem,
    )
    .unwrap()
    .unwrap();
    tx.commit().unwrap();
    if publish {
        ledger.note_head(&appended).unwrap();
    }
    appended
}

fn grant_payload(ledger: &Ledger) -> Value {
    let head = ledger.chain_head().unwrap();
    signed(
        &key(),
        Envelope {
            version: 1,
            network: ledger.network,
            seq: head.seq + 1,
            prev_hash: head.hash,
            at_ms: 100,
            authority: Authority::RegistryGranted {
                binding: binding(1),
            },
        },
    )
    .unwrap()
}

#[test]
fn registry_authority_is_operator_only_without_stalling_agent_cursors() {
    let (_dir, ledger, journal) = fixture();
    let route = journal.grant(binding(1), 100).unwrap();
    let global = ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 101,
            agent_id: None,
            payload: &json!({"synthetic": true}),
            snapshot: None,
        })
        .unwrap();
    journal.retire(&route, 102).unwrap();
    let head = ledger.chain_head().unwrap();
    let agent = ledger.agent_view(route.binding.agent.as_str());
    for (seq, kind) in [
        (route.binding_seq, EventKind::RegistryGranted),
        (head.seq, EventKind::RegistryRetired),
    ] {
        assert!(agent.event(seq).unwrap().is_none());
        let operator = ledger.event(seq).unwrap().unwrap();
        assert_eq!(operator.kind, kind);
        assert!(operator.payload.is_some());
        assert!(operator.agent_id.is_none());
    }
    let operator = ledger.get_events(0, 100).unwrap();
    assert_eq!(
        operator
            .events
            .iter()
            .filter(|event| matches!(
                event.kind,
                EventKind::RegistryGranted | EventKind::RegistryRetired
            ))
            .count(),
        2
    );
    let short = agent.get_events(0, 2).unwrap();
    assert_eq!(short.events.len(), 1);
    assert_eq!(short.events[0].seq, global.seq);
    assert_eq!(short.events[0].kind, EventKind::OperatorAction);
    assert!(agent.event(global.seq).unwrap().is_some());
    assert_eq!(short.next_cursor, head.seq);
    assert_eq!(short.head_seq, head.seq);
    assert!(!short.resync_required);
    let full = agent.get_events(0, 1).unwrap();
    assert_eq!(full.events.len(), 1);
    let tail = agent.get_events(full.next_cursor, 1).unwrap();
    assert!(tail.events.is_empty());
    assert_eq!(tail.next_cursor, head.seq);
    assert!(!tail.resync_required);
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[test]
fn grant_retire_reopen_and_rebind_preserve_old_retirement() {
    let (dir, ledger, journal) = fixture();
    let agent = binding(1).agent;
    assert!(journal.route_for_agent(&agent).is_err());
    let first = journal.grant(binding(1), 100).unwrap();
    let head = ledger.chain_head().unwrap();
    assert_eq!(journal.grant(binding(1), 101).unwrap(), first);
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert_eq!(journal.route_for_agent(&agent).unwrap(), first);
    drop(journal);
    drop(ledger);
    let ledger = Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap());
    let journal = RegistryJournal::open(ledger.clone(), key()).unwrap();
    assert_eq!(journal.route_for_agent(&agent).unwrap(), first);
    assert!(journal.retire(&first, 102).unwrap());
    assert!(journal.route_for_agent(&agent).is_err());
    let second = journal.grant(binding(2), 103).unwrap();
    assert!(second.binding_seq > first.binding_seq);
    let head = ledger.chain_head().unwrap();
    assert!(!journal.retire(&first, 104).unwrap());
    assert_eq!(ledger.chain_head().unwrap(), head);
    assert_eq!(journal.route_for_agent(&agent).unwrap(), second);
    drop(journal);
    drop(ledger);
    let journal = RegistryJournal::open(
        Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap()),
        key(),
    )
    .unwrap();
    assert_eq!(journal.route_for_agent(&agent).unwrap(), second);
    assert!(journal.grant(binding(1), 105).is_err());
}

#[test]
fn live_agents_and_lifetime_container_and_signer_uniqueness() {
    let (_dir, ledger, journal) = fixture();
    let first = journal.grant(binding(1), 100).unwrap();
    let head = ledger.chain_head().unwrap();
    assert!(
        journal.grant(binding(2), 101).is_err(),
        "live agent cannot be rebound"
    );
    for collision in ["container", "signer"] {
        let mut other = binding(2);
        other.agent = AgentId::new("other");
        if collision == "container" {
            other.container = first.binding.container;
        } else {
            other.wallet.address = first.binding.wallet.address;
        }
        assert!(journal.grant(other, 101).is_err(), "{collision}");
    }
    assert_eq!(ledger.chain_head().unwrap(), head);
    journal.retire(&first, 102).unwrap();
    for collision in ["container", "signer"] {
        let mut other = binding(2);
        other.agent = AgentId::new("other");
        if collision == "container" {
            other.container = first.binding.container;
        } else {
            other.wallet.address = first.binding.wallet.address;
        }
        assert!(
            journal.grant(other, 103).is_err(),
            "retired {collision} cannot be reused"
        );
    }
    assert!(journal.grant(binding(2), 104).is_ok());
}

#[test]
fn explicit_vault_option_is_required_and_never_inferred() {
    let (_dir, _ledger, journal) = fixture();
    let mut value = serde_json::to_value(binding(1)).unwrap();
    value.as_object_mut().unwrap().remove("vault_address");
    assert!(serde_json::from_value::<RegistryBinding>(value).is_err());
    let mut explicit = binding(1);
    explicit.vault_address = Some(explicit.container);
    let granted = journal.grant(explicit.clone(), 100).unwrap();
    assert_eq!(granted.binding, explicit);
    assert_eq!(
        journal
            .route_for_agent(&explicit.agent)
            .unwrap()
            .binding
            .vault_address,
        Some(explicit.container)
    );
    assert!(
        journal.grant(binding(1), 101).is_err(),
        "None cannot silently replace Some"
    );
}

#[test]
fn invalid_wallet_agent_container_and_time_are_rejected_before_append() {
    for field in [
        "agent",
        "zero_container",
        "zero_signer",
        "owner_signer",
        "vault",
        "generation",
        "approval",
        "expired",
        "future",
        "overflow",
    ] {
        let (_dir, ledger, journal) = fixture();
        let mut value = binding(1);
        match field {
            "agent" => value.agent = AgentId::new("bad/agent"),
            "zero_container" => value.container = Address::ZERO,
            "zero_signer" => value.wallet.address = Address::ZERO,
            "owner_signer" => value.wallet.address = value.container,
            "vault" => value.vault_address = Some(Address::from_bytes([9; 20])),
            "generation" => value.wallet.generation = MAX_GENERATION + 1,
            "approval" => value.wallet.approved_at_ms = value.wallet.valid_until_ms,
            "expired" => value.wallet.valid_until_ms = 100,
            "future" => value.wallet.approved_at_ms = 101,
            "overflow" => value.wallet.valid_until_ms = u64::MAX,
            _ => unreachable!(),
        }
        assert!(journal.grant(value, 100).is_err(), "{field}");
        assert_eq!(ledger.chain_head().unwrap().seq, 0);
    }
}

#[test]
fn held_guard_route_check_requires_exact_live_identity_and_actual_signer() {
    let (_dir, ledger, journal) = fixture();
    let route = journal.grant(binding(1), 100).unwrap();
    let mut guard = journal.ledger().lock().unwrap();
    let tx = guard
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .unwrap();
    assert!(
        journal
            .verify_route_in(&tx, &route, route.binding.wallet.address)
            .is_ok()
    );
    assert!(
        journal
            .verify_route_in(&tx, &route, Address::from_bytes([9; 20]))
            .is_err()
    );
    for field in [
        "network",
        "seq",
        "agent",
        "container",
        "vault",
        "signer",
        "generation",
        "approval",
        "expiry",
    ] {
        let mut changed = route.clone();
        match field {
            "network" => changed.network = Network::Mainnet,
            "seq" => changed.binding_seq += 1,
            "agent" => changed.binding.agent = AgentId::new("other"),
            "container" => changed.binding.container = Address::from_bytes([2; 20]),
            "vault" => changed.binding.vault_address = Some(changed.binding.container),
            "signer" => changed.binding.wallet.address = Address::from_bytes([9; 20]),
            "generation" => changed.binding.wallet.generation += 1,
            "approval" => changed.binding.wallet.approved_at_ms += 1,
            "expiry" => changed.binding.wallet.valid_until_ms += 1,
            _ => unreachable!(),
        }
        assert!(
            journal
                .verify_route_in(&tx, &changed, route.binding.wallet.address)
                .is_err(),
            "{field}"
        );
    }
    drop(tx);
    drop(guard);
    journal.retire(&route, 101).unwrap();
    let guard = ledger.lock().unwrap();
    assert!(
        journal
            .verify_route_in(&guard, &route, route.binding.wallet.address)
            .is_err()
    );
}

#[test]
fn expired_wallet_keeps_cleanup_route_authority_and_can_be_retired() {
    let (_dir, ledger, journal) = fixture();
    let route = journal.grant(binding(1), 100).unwrap();
    let after_expiry = route.binding.wallet.valid_until_ms + 1;
    assert!(matches!(
        route.binding.wallet.expiry(after_expiry),
        crate::keys::ExpiryState::Expired { .. }
    ));
    assert_eq!(
        journal.route_for_agent(&route.binding.agent).unwrap(),
        route
    );
    {
        let mut guard = ledger.lock().unwrap();
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .unwrap();
        assert!(
            journal
                .verify_route_in(&tx, &route, route.binding.wallet.address)
                .is_ok(),
            "identity verification must not impose order-only expiry policy on cleanup"
        );
    }
    assert!(journal.retire(&route, after_expiry).unwrap());
    assert!(journal.route_for_agent(&route.binding.agent).is_err());
    let projection = ledger
        .sub_account(&route.binding.container.to_string())
        .unwrap()
        .unwrap();
    assert!(!projection.active);
    assert!(projection.recorded);
}

#[test]
fn unsigned_legacy_binding_never_becomes_route_authority() {
    let (_dir, ledger, journal) = fixture();
    ledger
        .append(&NewEvent {
            kind: EventKind::OperatorAction,
            ts_ms: 100,
            agent_id: Some("synthetic-agent"),
            payload: &serde_json::to_value(binding(1)).unwrap(),
            snapshot: None,
        })
        .unwrap();
    assert!(journal.route_for_agent(&binding(1).agent).is_err());
}

#[test]
fn forged_one_row_grant_is_refused_even_after_anchor_publication() {
    let (_dir, ledger, journal) = fixture();
    let mut payload = grant_payload(&ledger);
    payload["mac"] = json!(hex::encode([0; 32]));
    let forged = append_payload(
        &ledger,
        &payload,
        EventKind::RegistryGranted,
        &format!("registry_container:{}", binding(1).container),
        false,
    );
    assert!(ledger.verify().unwrap().is_intact());
    assert!(journal.route_for_agent(&binding(1).agent).is_err());
    ledger.note_head(&forged).unwrap();
    assert!(ledger.verify().unwrap().is_intact());
    assert!(journal.route_for_agent(&binding(1).agent).is_err());
    assert!(journal.grant(binding(2), 101).is_err());
}

#[test]
fn all_grant_fields_are_mac_bound_and_nested_unknown_fields_fail_closed() {
    for field in [
        "version",
        "network",
        "seq",
        "prev_hash",
        "time",
        "agent",
        "container",
        "vault",
        "missing_vault",
        "wallet",
        "generation",
        "approval",
        "expiry",
        "unknown_wallet",
        "missing_mac",
    ] {
        let (_dir, ledger, journal) = fixture();
        let mut payload = grant_payload(&ledger);
        let grant = &mut payload["envelope"]["authority"]["binding"];
        match field {
            "agent" => grant["agent"] = json!("other"),
            "container" => grant["container"] = json!(Address::from_bytes([2; 20])),
            "vault" => grant["vault_address"] = json!(binding(1).container),
            "missing_vault" => {
                grant.as_object_mut().unwrap().remove("vault_address");
            }
            "wallet" => grant["wallet"]["address"] = json!(Address::from_bytes([102; 20])),
            "generation" => grant["wallet"]["generation"] = json!(1),
            "approval" => grant["wallet"]["approved_at_ms"] = json!(51),
            "expiry" => grant["wallet"]["valid_until_ms"] = json!(1001),
            "unknown_wallet" => grant["wallet"]["extra"] = json!(true),
            "version" => payload["envelope"]["version"] = json!(2),
            "network" => payload["envelope"]["network"] = json!(Network::Mainnet),
            "seq" => payload["envelope"]["seq"] = json!(2),
            "prev_hash" => payload["envelope"]["prev_hash"] = json!(hex::encode([0; 32])),
            "time" => payload["envelope"]["at_ms"] = json!(101),
            "missing_mac" => {
                payload.as_object_mut().unwrap().remove("mac");
            }
            _ => unreachable!(),
        }
        append_payload(
            &ledger,
            &payload,
            EventKind::RegistryGranted,
            &format!("registry_container:{}", binding(1).container),
            true,
        );
        assert!(ledger.verify().unwrap().is_intact());
        assert!(
            journal.route_for_agent(&binding(1).agent).is_err(),
            "{field}"
        );
    }
}

#[test]
fn retirement_requires_exact_grant_and_authenticated_hash_link() {
    let (_dir, ledger, journal) = fixture();
    let route = journal.grant(binding(1), 90).unwrap();
    let mut changed = route.clone();
    changed.binding.wallet.generation += 1;
    assert!(journal.retire(&changed, 100).is_err());
    changed = route.clone();
    changed.network = Network::Mainnet;
    assert!(journal.retire(&changed, 100).is_err());
    let head = ledger.chain_head().unwrap();
    let mut payload = signed(
        &key(),
        Envelope {
            version: 1,
            network: ledger.network,
            seq: head.seq + 1,
            prev_hash: head.hash,
            at_ms: 100,
            authority: Authority::RegistryRetired {
                binding_seq: route.binding_seq,
                binding_hash: ledger.event(route.binding_seq).unwrap().unwrap().hash,
            },
        },
    )
    .unwrap();
    payload["envelope"]["authority"]["binding_hash"] = json!(hex::encode([0; 32]));
    append_payload(
        &ledger,
        &payload,
        EventKind::RegistryRetired,
        &format!("registry_retired:{}", route.binding_seq),
        true,
    );
    assert!(ledger.verify().unwrap().is_intact());
    assert!(journal.route_for_agent(&binding(1).agent).is_err());
}

#[test]
fn missing_redacted_corrupt_and_wrong_key_history_never_yields_a_route() {
    for target in ["grant", "retirement", "hash", "key", "deleted"] {
        let (_dir, ledger, journal) = fixture();
        let route = journal.grant(binding(1), 100).unwrap();
        journal.retire(&route, 101).unwrap();
        match target {
            "grant" => {
                ledger.redact(route.binding_seq, "synthetic", 102).unwrap();
            }
            "retirement" => {
                ledger.redact(2, "synthetic", 102).unwrap();
            }
            "hash" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("UPDATE events SET hash = 'bad' WHERE seq = 1", [])
                    .unwrap();
            }
            "key" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("UPDATE events SET idem_key = NULL WHERE seq = 1", [])
                    .unwrap();
            }
            "deleted" => {
                ledger
                    .lock()
                    .unwrap()
                    .execute("DELETE FROM events", [])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            journal.route_for_agent(&binding(1).agent).is_err(),
            "{target}"
        );
        assert!(RegistryJournal::open(ledger, key()).is_err(), "{target}");
    }
    let (dir, ledger, journal) = fixture();
    journal.grant(binding(1), 100).unwrap();
    let head = ledger.chain_head().unwrap();
    assert!(RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([9; 32]))).is_err());
    assert_eq!(ledger.chain_head().unwrap(), head);
    let unanchored = Arc::new(
        Ledger::open_anchored(&dir.path().join("unanchored.db"), Network::Testnet, None).unwrap(),
    );
    assert!(RegistryJournal::open(unanchored, key()).is_err());
}

#[test]
fn independently_opened_journals_serialize_conflicting_grants() {
    let (dir, ledger, journal) = fixture();
    let other = RegistryJournal::open(
        Arc::new(Ledger::open(dir.path(), Network::Testnet).unwrap()),
        key(),
    )
    .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = [journal, other]
        .into_iter()
        .enumerate()
        .map(|(index, journal)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                journal.grant(binding(index as u8 + 1), 100)
            })
        })
        .collect();
    barrier.wait();
    let winners: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().unwrap().ok())
        .collect();
    assert_eq!(winners.len(), 1);
    assert_eq!(ledger.chain_head().unwrap().seq, 1);
    let journal = RegistryJournal::open(ledger, key()).unwrap();
    assert_eq!(
        journal.route_for_agent(&binding(1).agent).unwrap(),
        winners[0]
    );
}

fn metadata(binding: &RegistryBinding) -> SubAccount {
    SubAccount {
        address: binding.container.to_string(),
        name: "operator supplied name".into(),
        owner: None,
        recorded: false,
        provisioned_by_oppen: false,
        active: true,
        created_ts_ms: 12,
    }
}

#[test]
fn legacy_case_aliases_cannot_hide_retirement_or_foreign_ownership() {
    for alias in [
        format!("0x{}", "AB".repeat(20)),
        format!("0x{}", "Ab".repeat(20)),
    ] {
        for conflict in ["retired", "foreign_owner"] {
            let (_dir, ledger, journal) = fixture();
            let mut proposed = binding(1);
            proposed.container = Address::from_bytes([0xab; 20]);
            let mut legacy = metadata(&proposed);
            legacy.address = alias.clone();
            if conflict == "retired" {
                legacy.active = false;
            } else {
                legacy.owner = Some(Owner {
                    owner_type: OwnerType::Agent,
                    owner_id: "other".into(),
                });
            }
            crate::ledger::write_sub_account_on(&ledger.lock().unwrap(), &legacy).unwrap();
            assert!(journal.grant(proposed, 100).is_err(), "{alias}: {conflict}");
            assert_eq!(ledger.chain_head().unwrap().seq, 0);
            assert_eq!(ledger.sub_accounts().unwrap(), vec![legacy]);
        }
    }
}

#[test]
fn unique_case_alias_adoption_and_retirement_preserve_the_legacy_row() {
    for alias in [
        format!("0x{}", "AB".repeat(20)),
        format!("0x{}", "aB".repeat(20)),
    ] {
        for owned in [false, true] {
            let (_dir, ledger, journal) = fixture();
            let mut proposed = binding(1);
            proposed.container = Address::from_bytes([0xab; 20]);
            let mut legacy = metadata(&proposed);
            legacy.address = alias.clone();
            if owned {
                legacy.owner = Some(registry_owner(&proposed));
            }
            crate::ledger::write_sub_account_on(&ledger.lock().unwrap(), &legacy).unwrap();
            let route = journal.grant(proposed.clone(), 100).unwrap();
            assert_eq!(journal.route_for_agent(&proposed.agent).unwrap(), route);
            let rows = ledger.sub_accounts().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].address, legacy.address);
            assert_eq!(rows[0].name, legacy.name);
            assert_eq!(rows[0].created_ts_ms, legacy.created_ts_ms);
            assert!(rows[0].recorded && rows[0].active);
            assert_eq!(rows[0].owner, Some(registry_owner(&proposed)));
            assert!(journal.retire(&route, 101).unwrap());
            let rows = ledger.sub_accounts().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].address, legacy.address);
            assert_eq!(rows[0].created_ts_ms, legacy.created_ts_ms);
            assert!(rows[0].recorded && !rows[0].active);
            assert!(journal.route_for_agent(&proposed.agent).is_err());
            assert!(RegistryJournal::open(ledger, key()).is_ok());
        }
    }
}

#[test]
fn duplicate_case_aliases_block_adoption_and_verified_projection() {
    for already_granted in [false, true] {
        for conflict in ["retired", "foreign_owner", "same_owner"] {
            let (_dir, ledger, journal) = fixture();
            let mut proposed = binding(1);
            proposed.container = Address::from_bytes([0xab; 20]);
            let route = if already_granted {
                Some(journal.grant(proposed.clone(), 100).unwrap())
            } else {
                crate::ledger::write_sub_account_on(&ledger.lock().unwrap(), &metadata(&proposed))
                    .unwrap();
                None
            };
            let mut alias = metadata(&proposed);
            alias.address = format!("0x{}", "Ab".repeat(20));
            match conflict {
                "retired" => alias.active = false,
                "foreign_owner" => {
                    alias.owner = Some(Owner {
                        owner_type: OwnerType::Agent,
                        owner_id: "other".into(),
                    })
                }
                "same_owner" => alias.owner = Some(registry_owner(&proposed)),
                _ => unreachable!(),
            }
            crate::ledger::write_sub_account_on(&ledger.lock().unwrap(), &alias).unwrap();
            let head = ledger.chain_head().unwrap();
            assert!(
                journal.grant(proposed.clone(), 101).is_err(),
                "{already_granted}: {conflict}"
            );
            assert!(journal.route_for_agent(&proposed.agent).is_err());
            if let Some(route) = route {
                assert!(journal.retire(&route, 102).is_err());
            }
            assert_eq!(ledger.chain_head().unwrap(), head);
            assert_eq!(ledger.sub_accounts().unwrap().len(), 2);
            assert!(ledger.verify().unwrap().is_intact());
            assert!(RegistryJournal::open(ledger, key()).is_err());
        }
    }
}

#[test]
fn projection_checks_owner_and_active_through_a_single_case_alias() {
    for field in ["owner", "active"] {
        let (_dir, ledger, journal) = fixture();
        let mut proposed = binding(1);
        proposed.container = Address::from_bytes([0xab; 20]);
        let route = journal.grant(proposed, 100).unwrap();
        let guard = ledger.lock().unwrap();
        guard
            .execute(
                "UPDATE sub_accounts SET address = ?1",
                rusqlite::params![format!("0x{}", "AB".repeat(20))],
            )
            .unwrap();
        if field == "owner" {
            guard
                .execute("UPDATE sub_accounts SET owner_id = 'other'", [])
                .unwrap();
        } else {
            guard
                .execute("UPDATE sub_accounts SET active = 0", [])
                .unwrap();
        }
        drop(guard);
        assert!(
            journal.route_for_agent(&route.binding.agent).is_err(),
            "{field}"
        );
        assert!(journal.retire(&route, 101).is_err(), "{field}");
    }
}

#[test]
fn explicit_adoption_preserves_metadata_and_keeps_retired_history_recorded() {
    for owned in [false, true] {
        let (_dir, ledger, journal) = fixture();
        let binding = binding(1);
        let mut old = metadata(&binding);
        if owned {
            old.owner = Some(registry_owner(&binding));
        }
        ledger.upsert_sub_account(&old).unwrap();
        assert!(
            journal.route_for_agent(&binding.agent).is_err(),
            "legacy metadata is not authority"
        );
        let route = journal.grant(binding.clone(), 100).unwrap();
        let projection = ledger
            .sub_account(&binding.container.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(projection.name, old.name);
        assert_eq!(projection.created_ts_ms, old.created_ts_ms);
        assert!(!projection.provisioned_by_oppen);
        assert!(projection.active && projection.recorded);
        assert_eq!(projection.owner, Some(registry_owner(&binding)));
        let guard = ledger.lock().unwrap();
        assert!(managed_on(&guard, &binding.container.to_string()).unwrap());
        assert!(!managed_on(&guard, &Address::from_bytes([9; 20]).to_string()).unwrap());
        drop(guard);
        journal.retire(&route, 101).unwrap();
        let retired = ledger
            .sub_account(&binding.container.to_string())
            .unwrap()
            .unwrap();
        assert!(!retired.active);
        assert!(retired.recorded);
        assert_eq!(retired.name, old.name);
        assert_eq!(retired.created_ts_ms, old.created_ts_ms);
        let guard = ledger.lock().unwrap();
        assert!(managed_on(&guard, &binding.container.to_string()).unwrap());
    }
}

#[test]
fn grant_refuses_conflicting_or_retired_legacy_metadata() {
    for conflict in ["retired", "other_owner", "workflow", "ambiguous_agent"] {
        let (_dir, ledger, journal) = fixture();
        let proposed = binding(1);
        let mut row = metadata(&proposed);
        match conflict {
            "retired" => row.active = false,
            "other_owner" => {
                row.owner = Some(Owner {
                    owner_type: OwnerType::Agent,
                    owner_id: "other".into(),
                })
            }
            "workflow" => {
                row.owner = Some(Owner {
                    owner_type: OwnerType::Workflow,
                    owner_id: proposed.agent.as_str().into(),
                })
            }
            "ambiguous_agent" => {
                row.address = binding(2).container.to_string();
                row.owner = Some(registry_owner(&proposed));
            }
            _ => unreachable!(),
        }
        ledger.upsert_sub_account(&row).unwrap();
        assert!(journal.grant(proposed, 100).is_err(), "{conflict}");
        assert_eq!(ledger.chain_head().unwrap().seq, 0);
    }
}

#[test]
fn projection_tampering_refuses_routes_and_retired_recording_opt_out() {
    for field in [
        "owner",
        "active",
        "recorded",
        "retired_recorded",
        "missing",
        "ambiguous",
    ] {
        let (_dir, ledger, journal) = fixture();
        let route = journal.grant(binding(1), 100).unwrap();
        if field == "retired_recorded" {
            journal.retire(&route, 101).unwrap();
        }
        let guard = ledger.lock().unwrap();
        match field {
            "owner" => {
                guard
                    .execute("UPDATE sub_accounts SET owner_id = 'other'", [])
                    .unwrap();
            }
            "active" => {
                guard
                    .execute("UPDATE sub_accounts SET active = 0", [])
                    .unwrap();
            }
            "recorded" | "retired_recorded" => {
                guard
                    .execute("UPDATE sub_accounts SET recorded = 0", [])
                    .unwrap();
            }
            "missing" => {
                guard.execute("DELETE FROM sub_accounts", []).unwrap();
            }
            "ambiguous" => {
                let mut duplicate = metadata(&binding(2));
                duplicate.owner = Some(registry_owner(&binding(1)));
                crate::ledger::write_sub_account_on(&guard, &duplicate).unwrap();
            }
            _ => unreachable!(),
        }
        drop(guard);
        assert!(
            ledger.verify().unwrap().is_intact(),
            "metadata is not hash-chained"
        );
        assert!(
            journal.route_for_agent(&route.binding.agent).is_err(),
            "{field}"
        );
        assert!(RegistryJournal::open(ledger, key()).is_err(), "{field}");
    }
}

#[test]
fn projection_is_rolled_back_if_grant_append_cannot_commit() {
    let (_dir, ledger, journal) = fixture();
    append_payload(
        &ledger,
        &json!({"synthetic_orphan_key": true}),
        EventKind::OperatorAction,
        &format!("registry_container:{}", binding(1).container),
        true,
    );
    let head = ledger.chain_head().unwrap();
    assert!(journal.grant(binding(1), 100).is_err());
    assert!(
        ledger
            .sub_account(&binding(1).container.to_string())
            .unwrap()
            .is_none()
    );
    assert_eq!(ledger.chain_head().unwrap(), head);
}

#[test]
fn keyless_managed_scan_fails_closed_on_redacted_rows_for_any_address() {
    let (_dir, ledger, journal) = fixture();
    let route = journal.grant(binding(1), 100).unwrap();
    journal.retire(&route, 101).unwrap();
    ledger.redact(route.binding_seq, "synthetic", 102).unwrap();
    let guard = ledger.lock().unwrap();
    assert!(managed_on(&guard, &binding(1).container.to_string()).is_err());
    assert!(managed_on(&guard, &binding(2).container.to_string()).is_err());
}

#[test]
fn network_copy_cannot_authorize_an_identical_binding() {
    let (dir, ledger, _journal) = fixture();
    let payload = grant_payload(&ledger);
    let other = Arc::new(Ledger::open(dir.path(), Network::Mainnet).unwrap());
    let journal = RegistryJournal::open(other.clone(), key()).unwrap();
    append_payload(
        &other,
        &payload,
        EventKind::RegistryGranted,
        &format!("registry_container:{}", binding(1).container),
        true,
    );
    assert!(other.verify().unwrap().is_intact());
    assert!(matches!(journal.route_for_agent(&binding(1).agent),
        Err(RegistryError::Unavailable { detail }) if detail.contains("envelope")));
}

#[test]
fn one_row_anchor_failure_preserves_atomic_authority_and_projection_on_reopen() {
    use crate::ledger::{Anchor, HeadAnchor};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    #[derive(Debug)]
    struct TestAnchor {
        head: Arc<Mutex<Option<Anchor>>>,
        fail: Arc<AtomicBool>,
    }
    impl HeadAnchor for TestAnchor {
        fn load(&self) -> super::super::Result<Option<Anchor>> {
            Ok(self.head.lock().unwrap().clone())
        }
        fn store(&self, head: &Anchor) -> super::super::Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(LedgerError::Io(std::io::Error::other(
                    "synthetic anchor failure",
                )));
            }
            *self.head.lock().unwrap() = Some(head.clone());
            Ok(())
        }
    }
    for (retiring, retry) in [(false, false), (true, false), (false, true), (true, true)] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("anchor.db");
        let witnessed = Arc::new(Mutex::new(None));
        let fail = Arc::new(AtomicBool::new(false));
        let anchor = || {
            Box::new(TestAnchor {
                head: witnessed.clone(),
                fail: fail.clone(),
            }) as Box<dyn HeadAnchor>
        };
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let journal = RegistryJournal::open(ledger.clone(), key()).unwrap();
        let route = retiring.then(|| journal.grant(binding(1), 100).unwrap());
        let before = ledger.chain_head().unwrap();
        fail.store(true, Ordering::SeqCst);
        if let Some(route) = &route {
            assert!(journal.retire(route, 101).is_err());
        } else {
            assert!(journal.grant(binding(1), 100).is_err());
        }
        assert_eq!(ledger.chain_head().unwrap().seq, before.seq + 1);
        assert_eq!(*witnessed.lock().unwrap(), Some(before.clone()));
        if retiring {
            assert!(journal.route_for_agent(&binding(1).agent).is_err());
        } else {
            assert_eq!(
                journal.route_for_agent(&binding(1).agent).unwrap().binding,
                binding(1),
                "a post-commit anchor error does not revoke durable authority"
            );
        }
        if retry {
            let committed = ledger.chain_head().unwrap();
            if let Some(route) = &route {
                assert!(journal.retire(route, 102).is_err());
            } else {
                assert!(journal.grant(binding(1), 102).is_err());
            }
            assert_eq!(ledger.chain_head().unwrap(), committed);
            assert_eq!(*witnessed.lock().unwrap(), Some(before.clone()));
            fail.store(false, Ordering::SeqCst);
            if let Some(route) = &route {
                assert!(!journal.retire(route, 103).unwrap());
            } else {
                assert_eq!(journal.grant(binding(1), 103).unwrap().binding, binding(1));
            }
            assert_eq!(ledger.chain_head().unwrap(), committed);
            assert_eq!(
                *witnessed.lock().unwrap(),
                Some(committed),
                "same-handle idempotent success must publish the verified head"
            );
        }
        drop(journal);
        drop(ledger);
        fail.store(false, Ordering::SeqCst);
        let ledger =
            Arc::new(Ledger::open_anchored(&path, Network::Testnet, Some(anchor())).unwrap());
        let journal = RegistryJournal::open(ledger.clone(), key()).unwrap();
        let projection = ledger
            .sub_account(&binding(1).container.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(projection.active, !retiring);
        assert!(projection.recorded);
        if retiring {
            assert!(journal.route_for_agent(&binding(1).agent).is_err());
        } else {
            assert_eq!(
                journal.route_for_agent(&binding(1).agent).unwrap().binding,
                binding(1)
            );
        }
        assert!(ledger.verify().unwrap().is_intact());
        if !retry {
            continue;
        }
        {
            let mut guard = ledger.lock().unwrap();
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute(
                "DELETE FROM events WHERE seq > ?1",
                rusqlite::params![before.seq],
            )
            .unwrap();
            tx.execute(
                "UPDATE chain_head SET seq = ?1, hash = ?2 WHERE id = 0",
                rusqlite::params![before.seq, before.hash],
            )
            .unwrap();
            if retiring {
                tx.execute("UPDATE sub_accounts SET active = 1", [])
                    .unwrap();
            } else {
                tx.execute("DELETE FROM sub_accounts", []).unwrap();
            }
            tx.commit().unwrap();
        }
        assert!(
            !ledger.verify().unwrap().is_intact(),
            "rollback after successful retry must be detected"
        );
        assert!(journal.route_for_agent(&binding(1).agent).is_err());
    }
}
