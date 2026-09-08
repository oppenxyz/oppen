use super::*;
use oppen_core::keys::{AgentWallet, EntryName, HmacKey, KeyStoreError, SecretText};
use oppen_core::ledger::{Anchor, FileAnchor, HeadAnchor, LedgerError, RegistryBinding};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

pub(crate) fn edits() -> PolicyEdits {
    PolicyEdits {
        symbols: vec!["BTC".into(), "xyz:Mixed-1".into()],
        max_order_usd: "15".into(),
        max_position_usd: "25".into(),
        max_open_exposure_usd: "25".into(),
        max_leverage: 1,
        approval_required: true,
    }
}

#[derive(Debug)]
pub(crate) struct Publication {
    file: FileAnchor,
    pub fail: AtomicBool,
    pub panic: AtomicBool,
    gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

#[derive(Debug)]
struct TestAnchor(Arc<Publication>);

impl HeadAnchor for TestAnchor {
    fn load(&self) -> Result<Option<Anchor>, LedgerError> {
        self.0.file.load()
    }
    fn store(&self, anchor: &Anchor) -> Result<(), LedgerError> {
        let gate = self.0.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        if self.0.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("controlled anchor failure").into());
        }
        assert!(
            !self.0.panic.swap(false, Ordering::SeqCst),
            "controlled post-commit anchor panic"
        );
        self.0.file.store(anchor)
    }
}

impl Publication {
    pub(crate) fn block(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        *self.gate.lock().unwrap() = Some((entered, wait));
        (started, release)
    }
}

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub ledger: Arc<Ledger>,
    pub registry: Arc<RegistryJournal>,
    pub anchor: Arc<Publication>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(oppen_core::db_file_name(Network::Testnet));
        let anchor = Arc::new(Publication {
            file: FileAnchor::beside(&path),
            fail: AtomicBool::new(false),
            panic: AtomicBool::new(false),
            gate: Mutex::new(None),
        });
        let ledger = Arc::new(
            Ledger::open_anchored(
                &path,
                Network::Testnet,
                Some(Box::new(TestAnchor(anchor.clone()))),
            )
            .unwrap(),
        );
        let registry = Arc::new(
            RegistryJournal::open(ledger.clone(), Arc::new(HmacKey::from_bytes([11; 32]))).unwrap(),
        );
        registry
            .grant(
                RegistryBinding {
                    agent: Self::agent(),
                    container: Self::account(),
                    vault_address: None,
                    wallet: AgentWallet {
                        generation: 0,
                        address: Address::from_bytes([9; 20]),
                        approved_at_ms: 1,
                        valid_until_ms: 100_000,
                    },
                },
                1,
            )
            .unwrap();
        Self {
            dir,
            ledger,
            registry,
            anchor,
        }
    }
    pub(crate) fn agent() -> AgentId {
        AgentId::new("setup-agent")
    }
    pub(crate) fn account() -> Address {
        Address::from_bytes([7; 20])
    }
    pub(crate) fn review(&self, id: u64) -> PreparedReview {
        PreparedReview::review(
            self.dir.path(),
            id,
            self.registry.clone(),
            Self::agent(),
            Self::account(),
            edits(),
            true,
            true,
            100,
        )
        .unwrap()
    }
    fn initialize(&self, state: PersistedState) {
        let review = LegacyPolicyReview::open(
            self.dir.path().join("guardrails-testnet.db"),
            Network::Testnet,
            2,
        )
        .unwrap();
        PolicyJournal::new(self.registry.clone())
            .initialize(&review, state, 2)
            .unwrap();
    }
}

struct Keys(Option<&'static str>);
impl KeyStore for Keys {
    fn network(&self) -> Network {
        Network::Testnet
    }
    fn read(&self, entry: &EntryName) -> Result<Option<SecretText>, KeyStoreError> {
        assert_eq!(
            format!("{entry:?}"),
            r#"EntryName { service: "xyz.oppen.testnet", account: "guardrail-hmac" }"#
        );
        Ok(self.0.map(|byte| SecretText::new(byte.repeat(32))))
    }
    fn write(&self, _: &EntryName, _: &str) -> Result<(), KeyStoreError> {
        panic!("setup cannot create keys")
    }
    fn remove(&self, _: &EntryName) -> Result<(), KeyStoreError> {
        panic!("setup cannot remove keys")
    }
}

#[test]
fn edits_are_string_money_bounded_and_preserve_symbol_case() {
    let mut config = AgentGuardrails::default();
    edits().apply(&mut config).unwrap();
    assert!(config.symbols.contains("xyz:Mixed-1"));
    let mut empty = edits();
    empty.symbols.clear();
    empty.apply(&mut config).unwrap();
    assert!(config.symbols.is_empty());
    for symbols in [
        vec![""],
        vec![" BTC"],
        vec!["BTC/USDC"],
        vec!["x:y:z"],
        vec!["BTC", "BTC"],
        vec!["a:\n"],
        vec!["é"],
    ] {
        let mut input = edits();
        input.symbols = symbols.into_iter().map(str::to_owned).collect();
        assert_eq!(input.validate().unwrap_err().kind, ErrorKind::Validation);
    }
    for value in ["0", "-1", "15.01", "NaN", ""] {
        let mut input = edits();
        input.max_order_usd = value.into();
        assert!(input.validate().is_err());
    }
    for value in ["0", "-1", "25.01"] {
        let mut input = edits();
        input.max_open_exposure_usd = value.into();
        assert!(input.validate().is_err());
    }
    let mut input = edits();
    input.max_position_usd = "0".into();
    assert!(input.validate().is_err());
    input.max_position_usd = "25.01".into();
    assert!(input.validate().is_err());
    let mut input = edits();
    input.max_leverage = 2;
    assert!(input.validate().is_err());
    let mut input = edits();
    input.approval_required = false;
    assert!(input.validate().is_err());
    assert!(serde_json::from_value::<PolicyEdits>(json!({"symbols":["BTC"],"max_order_usd":15,"max_position_usd":"100","max_open_exposure_usd":"25","max_leverage":1,"approval_required":true})).is_err());
}

#[test]
fn empty_source_requires_both_assertions_then_persists_only_paused_policy() {
    let fixture = Fixture::new();
    let head = fixture.ledger.chain_head().unwrap();
    for (empty, stopped) in [(false, false), (false, true), (true, false)] {
        assert!(
            PreparedReview::review(
                fixture.dir.path(),
                1,
                fixture.registry.clone(),
                Fixture::agent(),
                Fixture::account(),
                edits(),
                empty,
                stopped,
                100
            )
            .is_err()
        );
    }
    let review = fixture.review(1);
    assert_eq!(fixture.ledger.chain_head().unwrap(), head);
    assert!(review.view.before.is_none());
    assert!(!review.view.legacy.as_ref().unwrap().file_present);
    assert!(review.view.proposed.is_globally_paused());
    let revision = review.persist().unwrap();
    assert_eq!(revision, head.seq + 1);
    assert_eq!(review.persist().unwrap(), revision);
    let page = fixture.ledger.get_events(head.seq, 100).unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind.as_str(), "policy_initialized");
    let reopened = Arc::new(Ledger::open(fixture.dir.path(), Network::Testnet).unwrap());
    let registry =
        Arc::new(RegistryJournal::open(reopened, Arc::new(HmacKey::from_bytes([11; 32]))).unwrap());
    assert_eq!(
        PolicyJournal::new(registry).current().unwrap().state,
        review.view.proposed
    );
}

#[test]
fn edit_preserves_complete_unselected_policy_and_all_stops() {
    let fixture = Fixture::new();
    let mut original = PersistedState::paused(2);
    let other = AgentId::new("unrelated");
    let mut selected = AgentGuardrails {
        reduce_only: true,
        ..AgentGuardrails::default()
    };
    selected.order_rate.count = 2;
    selected.risk.max_risk_usd = Some("3".parse().unwrap());
    original
        .guardrails
        .insert(Fixture::agent(), selected.clone());
    original.guardrails.insert(
        other,
        AgentGuardrails {
            approval_required: false,
            ..AgentGuardrails::default()
        },
    );
    original.account_limits.max_daily_loss_usd = Some("4".parse().unwrap());
    let mut kill = serde_json::to_value(&original.kill).unwrap();
    // Reuse the serialized global engagement's exact core representation.
    kill["agents"]["setup-agent"] = kill["global"].clone();
    original.kill = serde_json::from_value(kill).unwrap();
    fixture.initialize(original.clone());
    let review = fixture.review(1);
    assert_eq!(review.view.before, Some(original.clone()));
    let mut expected = original.clone();
    edits()
        .apply(expected.guardrails.get_mut(&Fixture::agent()).unwrap())
        .unwrap();
    assert_eq!(review.view.proposed, expected);
    assert_eq!(review.view.proposed.kill, original.kill);
    review.persist().unwrap();
    assert_eq!(
        PolicyJournal::new(fixture.registry)
            .current()
            .unwrap()
            .state,
        expected
    );
}

#[test]
fn unpaused_signed_policy_and_changed_revision_or_route_refuse() {
    let fixture = Fixture::new();
    fixture.initialize(PersistedState::paused(2));
    let journal = PolicyJournal::new(fixture.registry.clone());
    let review = fixture.review(1);
    let current = journal.current().unwrap();
    let mut changed = current.state.clone();
    changed.account_limits.max_drawdown_usd = Some("1".parse().unwrap());
    journal
        .replace(current.revision, changed.clone(), 101)
        .unwrap();
    let head = fixture.ledger.chain_head().unwrap();
    assert_eq!(review.persist().unwrap_err().kind, ErrorKind::Conflict);
    assert_eq!(fixture.ledger.chain_head().unwrap(), head);
    let fresh = fixture.review(2);
    fixture.registry.retire(&fresh.view.route, 102).unwrap();
    assert_eq!(fresh.persist().unwrap_err().kind, ErrorKind::Conflict);

    let fixture = Fixture::new();
    fixture.initialize(PersistedState::paused(2));
    let journal = PolicyJournal::new(fixture.registry.clone());
    let current = journal.current().unwrap();
    journal
        .replace(current.revision, PersistedState::default(), 3)
        .unwrap();
    assert!(
        PreparedReview::review(
            fixture.dir.path(),
            1,
            fixture.registry.clone(),
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            100
        )
        .is_err()
    );
}

#[test]
fn missing_keys_ledger_registry_and_malformed_legacy_never_initialize() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        PreparedReview::open_with_keys(
            dir.path(),
            1,
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            &Keys(Some("0b"))
        )
        .is_err()
    );
    assert!(
        !dir.path()
            .join(oppen_core::db_file_name(Network::Testnet))
            .exists()
    );
    let ledger = Ledger::open(dir.path(), Network::Testnet).unwrap();
    for key in [None, Some("0b")] {
        assert!(
            PreparedReview::open_with_keys(
                dir.path(),
                1,
                Fixture::agent(),
                Fixture::account(),
                edits(),
                true,
                true,
                &Keys(key)
            )
            .is_err()
        );
    }
    assert_eq!(ledger.chain_head().unwrap().seq, 0);
    let fixture = Fixture::new();
    assert!(
        PreparedReview::open_with_keys(
            fixture.dir.path(),
            1,
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            &Keys(Some("0c"))
        )
        .is_err()
    );
    std::fs::write(
        fixture.dir.path().join("guardrails-testnet.db"),
        b"not sqlite",
    )
    .unwrap();
    let head = fixture.ledger.chain_head().unwrap();
    assert!(
        PreparedReview::review(
            fixture.dir.path(),
            1,
            fixture.registry.clone(),
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            100
        )
        .is_err()
    );
    assert_eq!(fixture.ledger.chain_head().unwrap(), head);
    let legacy_path = fixture.dir.path().join("guardrails-testnet.db");
    std::fs::remove_file(&legacy_path).unwrap();
    drop(Ledger::open_at(&legacy_path, Network::Testnet).unwrap());
    assert!(
        PreparedReview::review(
            fixture.dir.path(),
            2,
            fixture.registry.clone(),
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            100
        )
        .is_err()
    );
    assert_eq!(fixture.ledger.chain_head().unwrap(), head);
}

#[test]
fn invalid_authenticated_policy_never_falls_back_to_empty_legacy() {
    let fixture = Fixture::new();
    fixture.initialize(PersistedState::paused(2));
    let policy_seq = fixture.ledger.chain_head().unwrap().seq;
    fixture
        .ledger
        .redact(policy_seq, "controlled required policy redaction", 3)
        .unwrap();
    let before = fixture.ledger.chain_head().unwrap();
    assert!(
        PreparedReview::review(
            fixture.dir.path(),
            1,
            fixture.registry.clone(),
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            100
        )
        .is_err()
    );
    assert_eq!(fixture.ledger.chain_head().unwrap(), before);
}

#[test]
fn publication_failure_keeps_exact_retry_and_never_duplicates_initialization() {
    let fixture = Fixture::new();
    let review = fixture.review(1);
    let before = fixture.ledger.chain_head().unwrap();
    fixture.anchor.fail.store(true, Ordering::SeqCst);
    assert_eq!(review.persist().unwrap_err().kind, ErrorKind::Uncertain);
    let committed = fixture.ledger.chain_head().unwrap();
    assert_eq!(committed.seq, before.seq + 1);
    assert_eq!(review.persist().unwrap_err().kind, ErrorKind::Uncertain);
    assert_eq!(fixture.ledger.chain_head().unwrap(), committed);
    fixture.anchor.fail.store(false, Ordering::SeqCst);
    assert_eq!(review.persist().unwrap(), committed.seq);
    assert_eq!(fixture.anchor.file.load().unwrap().unwrap(), committed);
    assert_eq!(
        PolicyJournal::new(fixture.registry)
            .current()
            .unwrap()
            .state,
        review.view.proposed
    );
}

#[test]
fn actual_setup_opener_requires_existing_anchor_without_adopting_it() {
    let fixture = Fixture::new();
    let before = fixture.ledger.chain_head().unwrap();
    let review = PreparedReview::open_with_keys(
        fixture.dir.path(),
        1,
        Fixture::agent(),
        Fixture::account(),
        edits(),
        true,
        true,
        &Keys(Some("0b")),
    )
    .unwrap();
    assert!(review.view.proposed.is_globally_paused());
    assert_eq!(fixture.ledger.chain_head().unwrap(), before);
    drop(review);
    let anchor = FileAnchor::beside(
        &fixture
            .dir
            .path()
            .join(oppen_core::db_file_name(Network::Testnet)),
    );
    std::fs::remove_file(anchor.path()).unwrap();
    assert!(
        PreparedReview::open_with_keys(
            fixture.dir.path(),
            2,
            Fixture::agent(),
            Fixture::account(),
            edits(),
            true,
            true,
            &Keys(Some("0b"))
        )
        .is_err()
    );
    assert!(!anchor.path().exists());
    assert_eq!(fixture.ledger.chain_head().unwrap(), before);
}
