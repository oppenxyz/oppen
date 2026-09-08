//! ES39 retained pre-MCP consent. Authority remains in the core pilot journal.

use oppen_core::alert::AlertStore;
use oppen_core::features::quotes::QuoteCache;
use oppen_core::feed::{FeedSession, pump::FeedPump};
use oppen_core::guardrail::{AgentId, Refusal, Unevaluable};
use oppen_core::keys::{KeyStore, KeychainKeyStore};
use oppen_core::ledger::{
    Ledger, PairingJournal, PilotConsentError, PilotConsentEvidence, PilotConsentOutcome,
    PilotJournal, RegistryJournal,
};
use oppen_core::ledger::{
    PilotConsentAttestation, PilotConsentCorrelation, PilotConsentDisplay, PilotConsentReceipt,
    PilotState, PilotStatus,
};
use oppen_core::reconcile::{ReconcileSource, VenueSource};
use oppen_hl::Address;
use oppen_hl::ws::{EventReceiver, Subscription, WsPool, WsPoolConfig};
use oppen_hl::{InfoClient, Network};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attestations {
    pub typed_account: String,
    pub never_used_for_in_scope_trading: bool,
    pub dedicated_account_exclusive_use: bool,
    pub original_baseline_and_no_reset_confirmed: bool,
}

impl Attestations {
    pub(crate) fn confirmed(self, account: Address) -> Result<PilotConsentAttestation, Error> {
        if self.typed_account.parse::<Address>().ok() != Some(account)
            || !self.never_used_for_in_scope_trading
            || !self.dedicated_account_exclusive_use
            || !self.original_baseline_and_no_reset_confirmed
        {
            return Err(Error::Refused {
                detail: "type the full reviewed account and explicitly confirm every statement"
                    .into(),
            });
        }
        Ok(PilotConsentAttestation {
            dedicated_exclusive_account: self.dedicated_account_exclusive_use,
            never_used_for_trading: self.never_used_for_in_scope_trading,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Reviewing,
    ReviewReady,
    Confirming,
    Authorized,
    Existing,
    Refused,
    Uncertain,
    RecoveryRequired,
    Closed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Operation {
    Review,
    Confirm { review_id: String },
    Discard { review_id: String },
    Reconcile { review_id: String },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Review {
    pub id: String,
    pub display: PilotConsentDisplay,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Error {
    Refused {
        detail: String,
    },
    Uncertain {
        correlation: Option<Box<PilotConsentCorrelation>>,
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Resolution {
    Committed {
        receipt: Box<PilotConsentReceipt>,
        current_pilot: PilotState,
    },
    NotCommitted {
        correlation: PilotConsentCorrelation,
    },
    Unknown {
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Status {
    pub owner_id: String,
    pub agent: String,
    pub account: Address,
    pub operation_seq: u64,
    pub last_operation: Option<Operation>,
    pub phase: Phase,
    pub review: Option<Review>,
    pub existing: Option<PilotStatus>,
    pub receipt: Option<PilotConsentReceipt>,
    pub resolution: Option<Resolution>,
    pub error: Option<Error>,
}

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

enum Command {
    Confirm(Attestations),
    Discard,
    Reconcile,
}

pub(crate) struct Control {
    state: Mutex<Status>,
    commands: mpsc::Sender<Command>,
    stop: CancellationToken,
    finished: AtomicBool,
    task: tokio::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    resources: Mutex<Resources>,
}

#[derive(Default)]
struct Resources {
    authority: Option<Arc<Authority>>,
    ingress: Option<Ingress>,
}

fn error(detail: impl ToString) -> Error {
    Error::Refused {
        detail: detail.to_string(),
    }
}

fn now_ms() -> u64 {
    u64::try_from(oppen_core::ledger::now_ms()).unwrap_or(u64::MAX)
}

impl Control {
    pub(crate) fn launch(
        dir: PathBuf,
        agent: AgentId,
        account: Address,
        ready: oneshot::Receiver<Result<(), String>>,
    ) -> Result<Arc<Self>, Error> {
        Self::launch_with(
            dir,
            agent,
            account,
            Arc::new(KeychainKeyStore::new(Network::Testnet)),
            InfoClient::new(Network::Testnet).map_err(error)?,
            VenueSource::new(Network::Testnet).map_err(error)?,
            || {
                WsPool::new(WsPoolConfig {
                    network: Network::Testnet,
                    ..WsPoolConfig::default()
                })
            },
            ready,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn launch_with<S, F>(
        dir: PathBuf,
        agent: AgentId,
        account: Address,
        keys: Arc<dyn KeyStore>,
        info: InfoClient,
        source: S,
        pool: F,
        ready: oneshot::Receiver<Result<(), String>>,
    ) -> Result<Arc<Self>, Error>
    where
        S: ReconcileSource + Send + 'static,
        F: FnOnce() -> Result<(WsPool, EventReceiver), oppen_hl::ws::PoolError> + Send + 'static,
    {
        let owner = NEXT_OWNER
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| error("consent owner IDs exhausted"))?
            .to_string();
        let (send, recv) = mpsc::channel(1);
        let control = Arc::new(Self {
            state: Mutex::new(Status {
                owner_id: owner,
                agent: agent.to_string(),
                account,
                operation_seq: 1,
                last_operation: Some(Operation::Review),
                phase: Phase::Reviewing,
                review: None,
                existing: None,
                receipt: None,
                resolution: None,
                error: None,
            }),
            commands: send,
            stop: CancellationToken::new(),
            finished: AtomicBool::new(false),
            task: tokio::sync::Mutex::new(None),
            resources: Mutex::new(Resources::default()),
        });
        let mut task = control
            .task
            .try_lock()
            .map_err(|_| error("consent owner unavailable"))?;
        let worker = control.clone();
        *task = Some(tauri::async_runtime::spawn(async move {
            match ready.await {
                Ok(Ok(())) => {}
                result => {
                    let detail = match result {
                        Ok(Err(detail)) => detail,
                        Err(error) => error.to_string(),
                        _ => unreachable!(),
                    };
                    worker.fail(error(detail));
                    worker.finished.store(true, Ordering::SeqCst);
                    return;
                }
            }
            if worker.stop.is_cancelled() {
                worker.state().phase = Phase::Closed;
                worker.finished.store(true, Ordering::SeqCst);
                return;
            }
            let executor = tokio::runtime::Handle::current();
            let actual = worker.clone();
            let result = tauri::async_runtime::spawn_blocking(move || {
                executor.block_on(async move {
                    actual
                        .run(
                            &dir,
                            agent,
                            account,
                            keys.as_ref(),
                            info,
                            source,
                            pool,
                            recv,
                        )
                        .await
                })
            })
            .await;
            if let Err(join) = result {
                let mut state = worker.state();
                state.phase = Phase::RecoveryRequired;
                state.error = Some(Error::Uncertain {
                    correlation: state
                        .review
                        .as_ref()
                        .map(|r| Box::new(r.display.correlation.clone())),
                    detail: format!("consent owner task: {join}"),
                });
            }
            let (ingress, authority) = {
                let mut resources = worker.resources.lock().unwrap_or_else(|e| e.into_inner());
                (resources.ingress.take(), resources.authority.take())
            };
            if let Some(ingress) = ingress
                && let Err(detail) = ingress.drain().await
            {
                let correlation = worker
                    .state()
                    .review
                    .as_ref()
                    .map(|r| r.display.correlation.clone());
                worker.recovery_required(correlation, detail);
            }
            if let Err(join) = tauri::async_runtime::spawn_blocking(move || drop(authority)).await {
                worker.recovery_required(None, join.to_string());
            }
            worker.finished.store(true, Ordering::SeqCst);
        }));
        drop(task);
        Ok(control)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Status> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) fn snapshot(&self) -> Status {
        self.state().clone()
    }
    pub(crate) fn owned(&self) -> bool {
        !self.finished.load(Ordering::SeqCst)
            || (!self.stop.is_cancelled()
                && matches!(
                    self.state().phase,
                    Phase::Uncertain | Phase::RecoveryRequired
                ))
    }

    fn live(&self) -> Result<(), Refusal> {
        if self.stop.is_cancelled() {
            Err(Unevaluable::PolicyAuthority {
                detail: "native consent owner closed".into(),
            }
            .into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn confirm(
        &self,
        owner: &str,
        id: &str,
        attestations: Attestations,
    ) -> Result<Status, Error> {
        self.command(owner, id, Command::Confirm(attestations))
    }
    pub(crate) fn discard(&self, owner: &str, id: &str) -> Result<Status, Error> {
        self.command(owner, id, Command::Discard)
    }
    pub(crate) fn reconcile(&self, owner: &str, id: &str) -> Result<Status, Error> {
        self.command(owner, id, Command::Reconcile)
    }

    fn command(&self, owner: &str, id: &str, command: Command) -> Result<Status, Error> {
        self.live().map_err(error)?;
        let mut state = self.state();
        if self.finished.load(Ordering::SeqCst)
            || state.owner_id != owner
            || state.review.as_ref().is_none_or(|r| r.id != id)
        {
            return Err(error(
                "consent review owner changed or is no longer retained",
            ));
        }
        let operation = match &command {
            Command::Confirm(_) if state.phase == Phase::ReviewReady => Operation::Confirm {
                review_id: id.into(),
            },
            Command::Discard if state.phase == Phase::ReviewReady => Operation::Discard {
                review_id: id.into(),
            },
            Command::Reconcile if state.phase == Phase::Uncertain => Operation::Reconcile {
                review_id: id.into(),
            },
            _ => {
                return Err(error(
                    "consent work active, consumed, or requires exact outcome recovery",
                ));
            }
        };
        let seq = state
            .operation_seq
            .checked_add(1)
            .ok_or_else(|| error("consent operations exhausted"))?;
        self.commands.try_send(command).map_err(error)?;
        state.operation_seq = seq;
        state.last_operation = Some(operation);
        state.phase = Phase::Confirming;
        state.error = None;
        Ok(state.clone())
    }

    pub(crate) fn close(&self) {
        self.stop.cancel();
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), String> {
        self.close();
        let mut task = self.task.lock().await;
        let result = if let Some(task) = task.as_mut() {
            task.await.map_err(|e| e.to_string())
        } else {
            Ok(())
        };
        *task = None;
        result?;
        if matches!(
            self.state().phase,
            Phase::Uncertain | Phase::RecoveryRequired
        ) {
            return Err("pilot consent outcome remains uncertain".into());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run<S, F>(
        &self,
        dir: &Path,
        agent: AgentId,
        account: Address,
        keys: &dyn KeyStore,
        info: InfoClient,
        source: S,
        pool: F,
        mut commands: mpsc::Receiver<Command>,
    ) where
        S: ReconcileSource + Send + 'static,
        F: FnOnce() -> Result<(WsPool, EventReceiver), oppen_hl::ws::PoolError>,
    {
        let prepared = Authority::open(dir, keys);
        let authority = match prepared {
            Ok(value) => Arc::new(value),
            Err(failure) => {
                self.fail(failure);
                return;
            }
        };
        self.resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .authority = Some(authority.clone());
        match authority.registry.route_for_agent(&agent) {
            Ok(route)
                if route.network == Network::Testnet && route.binding.container == account => {}
            Ok(_) => {
                self.fail(error("consent identity differs from authenticated route"));
                return;
            }
            Err(failure) => {
                self.fail(error(failure));
                return;
            }
        }
        match authority.pilot.status(account) {
            Ok(Some(existing)) => {
                let mut state = self.state();
                state.existing = Some(existing);
                state.phase = Phase::Existing;
                return;
            }
            Err(failure) => {
                let existing = oppen_core::ledger::EventViews::new(authority.ledger.clone())
                    .pilot_status(account)
                    .ok()
                    .flatten();
                self.state().existing = existing;
                self.fail(error(failure));
                return;
            }
            Ok(None) => {}
        }
        if let Err(failure) = self.live() {
            self.fail(error(failure));
            return;
        }
        let market = match info.meta_and_asset_ctxs().await {
            Ok(market) if market.is_aligned() => market,
            Ok(_) => {
                self.fail(error("misaligned consent market context"));
                return;
            }
            Err(failure) => {
                self.fail(error(failure));
                return;
            }
        };
        let Some(asset) = market.meta().universe.first() else {
            self.fail(error(
                "no venue asset available for monitored consent freshness",
            ));
            return;
        };
        if let Err(failure) = self.live() {
            self.fail(error(failure));
            return;
        }
        let ingress = Ingress::start(
            dir,
            authority.ledger.clone(),
            account,
            source,
            pool,
            asset.name.clone(),
        );
        let ingress = match ingress {
            Ok(value) => value,
            Err(failure) => {
                self.fail(failure);
                return;
            }
        };
        let feed = ingress.feed.clone();
        let startup_error = ingress.startup_error.clone();
        self.resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ingress = Some(ingress);
        if let Some(failure) = startup_error {
            self.fail(error(failure));
            return;
        }
        let review = async {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    self.live().map_err(error)?;
                    let state = feed.state();
                    if let Some(failure) = state.failure {
                        return Err(error(failure));
                    }
                    if state.reconciled {
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| error("initial consent reconciliation did not complete"))??;
            let observation = authority
                .pilot
                .begin_authorization_review(&agent, account, feed.clone(), &now_ms)
                .map_err(error)?;
            self.live().map_err(error)?;
            let evidence = crate::account_evidence::gather(&info, observation.route())
                .await
                .map_err(error)?;
            self.live().map_err(error)?;
            authority
                .pilot
                .review_authorization(
                    observation,
                    PilotConsentEvidence { account: evidence },
                    &now_ms,
                )
                .map_err(error)
        }
        .await;
        let mut review = match review {
            Ok(review) => {
                let mut state = self.state();
                state.review = Some(Review {
                    id: "1".into(),
                    display: review.display().clone(),
                });
                state.phase = Phase::ReviewReady;
                Some(review)
            }
            Err(failure) => {
                self.fail(failure);
                None
            }
        };
        let correlation = review
            .as_ref()
            .map(|review| review.display().correlation.clone());
        if review.is_some() {
            loop {
                let command = tokio::select! {
                    biased;
                    () = self.stop.cancelled() => break,
                    command = commands.recv() => match command { Some(value) => value, None => break },
                };
                match command {
                    Command::Discard => {
                        self.state().phase = Phase::Closed;
                        break;
                    }
                    Command::Confirm(attestations) => {
                        let Some(retained) = review.take() else {
                            self.fail(error("consent review already consumed"));
                            break;
                        };
                        let result = async {
                            let claims = attestations.confirmed(account)?;
                            self.live().map_err(error)?;
                            let fresh = crate::account_evidence::gather(
                                &info,
                                &retained.display().correlation.route,
                            )
                            .await
                            .map_err(error)?;
                            self.live().map_err(error)?;
                            authority
                                .pilot
                                .confirm_authorization(
                                    retained,
                                    PilotConsentEvidence { account: fresh },
                                    claims,
                                    &now_ms,
                                    &|| self.live(),
                                )
                                .map_err(|failure| match failure {
                                    PilotConsentError::Refused { detail } => {
                                        Error::Refused { detail }
                                    }
                                    PilotConsentError::Uncertain {
                                        correlation,
                                        detail,
                                    } => Error::Uncertain {
                                        correlation: Some(correlation),
                                        detail,
                                    },
                                })
                        }
                        .await;
                        match result {
                            Ok(receipt) => {
                                let mut state = self.state();
                                state.receipt = Some(receipt);
                                state.phase = Phase::Authorized;
                                break;
                            }
                            Err(failure @ Error::Uncertain { .. }) => self.fail(failure),
                            Err(failure) => {
                                self.fail(failure);
                                break;
                            }
                        }
                    }
                    Command::Reconcile => {
                        let Some(correlation) = correlation.as_ref() else {
                            self.fail(error("exact consent correlation unavailable"));
                            break;
                        };
                        match authority.pilot.authorization_outcome(correlation) {
                            Ok(PilotConsentOutcome::Committed {
                                receipt,
                                current_pilot,
                            }) => {
                                let receipt = *receipt;
                                let mut state = self.state();
                                state.receipt = Some(receipt.clone());
                                state.resolution = Some(Resolution::Committed {
                                    receipt: Box::new(receipt),
                                    current_pilot,
                                });
                                state.phase = Phase::Authorized;
                                break;
                            }
                            Ok(PilotConsentOutcome::Absent) => {
                                let mut state = self.state();
                                state.resolution = Some(Resolution::NotCommitted {
                                    correlation: correlation.clone(),
                                });
                                state.phase = Phase::Refused;
                                break;
                            }
                            outcome => {
                                let detail = match outcome {
                                    Ok(PilotConsentOutcome::Unknown { detail }) => detail,
                                    Err(failure) => failure.to_string(),
                                    _ => unreachable!(),
                                };
                                let mut state = self.state();
                                state.resolution = Some(Resolution::Unknown { detail });
                                state.phase = Phase::Uncertain;
                            }
                        }
                    }
                }
            }
        }
        drop(review);
        if self.stop.is_cancelled() {
            let mut state = self.state();
            if !matches!(
                state.phase,
                Phase::Uncertain | Phase::RecoveryRequired | Phase::Authorized
            ) {
                state.phase = Phase::Closed;
            }
        }
    }

    fn fail(&self, failure: Error) {
        let mut state = self.state();
        state.phase = if matches!(failure, Error::Uncertain { .. }) {
            Phase::Uncertain
        } else {
            Phase::Refused
        };
        state.error = Some(failure);
    }

    fn recovery_required(&self, correlation: Option<PilotConsentCorrelation>, detail: String) {
        let mut state = self.state();
        state.phase = Phase::RecoveryRequired;
        state.error = Some(Error::Uncertain {
            correlation: correlation.map(Box::new),
            detail,
        });
    }
}

struct Authority {
    ledger: Arc<Ledger>,
    registry: Arc<RegistryJournal>,
    pilot: PilotJournal,
    _pairings: PairingJournal,
}
impl Authority {
    fn open(dir: &Path, keys: &dyn KeyStore) -> Result<Self, Error> {
        if keys.network() != Network::Testnet {
            return Err(error("testnet authentication required"));
        }
        let ledger = Arc::new(Ledger::open_existing(dir, Network::Testnet).map_err(error)?);
        let hmac = Arc::new(
            keys.load_hmac_key()
                .map_err(error)?
                .ok_or_else(|| error("existing authentication key required"))?,
        );
        let pairings = PairingJournal::open(ledger.clone(), hmac.clone()).map_err(error)?;
        let registry = Arc::new(RegistryJournal::open(ledger.clone(), hmac).map_err(error)?);
        Ok(Self {
            ledger,
            pilot: PilotJournal::new(registry.clone()),
            registry,
            _pairings: pairings,
        })
    }
}

struct Ingress {
    feed: Arc<FeedSession>,
    pool: Arc<WsPool>,
    quiesce: oneshot::Sender<oneshot::Sender<()>>,
    done: oneshot::Sender<()>,
    task: tauri::async_runtime::JoinHandle<Result<(), String>>,
    startup_error: Option<String>,
}
impl Ingress {
    fn start<S, F>(
        dir: &Path,
        ledger: Arc<Ledger>,
        account: Address,
        source: S,
        pool: F,
        market_coin: String,
    ) -> Result<Self, Error>
    where
        S: ReconcileSource + Send + 'static,
        F: FnOnce() -> Result<(WsPool, EventReceiver), oppen_hl::ws::PoolError>,
    {
        let alerts = AlertStore::open(dir.join("alerts-testnet.db")).map_err(error)?;
        let (pool, mut events) = pool().map_err(error)?;
        let pool = Arc::new(pool);
        let feed = Arc::new(FeedSession::new());
        let pump_pool = pool.clone();
        let session = feed.clone();
        let (quiesce, quiescing) = oneshot::channel();
        let (done, stopping) = oneshot::channel();
        let executor = tokio::runtime::Handle::current();
        let task = tauri::async_runtime::spawn_blocking(move || {
            executor.block_on(async move {
                let quotes = QuoteCache::new();
                let pump = FeedPump::new(
                    &session,
                    &ledger,
                    account,
                    source,
                    &alerts,
                    &quotes,
                    pump_pool.as_ref(),
                )
                .map_err(|e| e.to_string())?;
                pump.run_until_shutdown(&mut events, quiescing, stopping)
                    .await;
                drop(pump);
                drop(pump_pool);
                events.complete().map_err(|e| e.to_string())?;
                if let Some(failure) = session.state().failure {
                    return Err(failure);
                }
                Ok(())
            })
        });
        let mut startup_error = None;
        for subscription in [
            Subscription::UserFills { user: account },
            Subscription::OrderUpdates { user: account },
            Subscription::ActiveAssetCtx { coin: market_coin },
        ] {
            if let Err(failure) = pool.subscribe(subscription) {
                startup_error = Some(failure.to_string());
                break;
            }
        }
        Ok(Self {
            feed,
            pool,
            quiesce,
            done,
            task,
            startup_error,
        })
    }

    async fn drain(self) -> Result<(), String> {
        let (ack, acknowledged) = oneshot::channel();
        let quiesced = self.quiesce.send(ack).is_ok();
        let observed = acknowledged.await.is_ok();
        let sockets = self
            .pool
            .shutdown_and_drain()
            .await
            .map_err(|e| e.to_string());
        drop(self.pool);
        let _ = self.done.send(());
        let pump = self.task.await.map_err(|e| e.to_string())?;
        sockets?;
        pump?;
        if !quiesced || !observed {
            return Err("consent pump ended before quiescence".into());
        }
        Ok(())
    }
}
