//! ES23: reviewed, globally paused policy persistence. No execution assembly.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use oppen_core::guardrail::{
    AgentGuardrails, AgentId, LegacyPolicyEvidence, LegacyPolicyReview, PersistedState,
};
use oppen_core::keys::{KeyStore, KeychainKeyStore};
use oppen_core::ledger::{AuthorizedRoute, Ledger, PolicyError, PolicyJournal, RegistryJournal};
use oppen_hl::{Address, Network};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicyEdits {
    pub symbols: Vec<String>,
    pub max_order_usd: String,
    pub max_position_usd: String,
    pub max_open_exposure_usd: String,
    pub max_leverage: u32,
    pub approval_required: bool,
}

#[cfg(test)]
#[path = "policy_setup_tests.rs"]
pub(crate) mod tests;

impl PolicyEdits {
    fn apply(&self, config: &mut AgentGuardrails) -> Result<(), SetupError> {
        let component = |part: &str| {
            part.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        let mut symbols = BTreeSet::new();
        for symbol in &self.symbols {
            let parts = symbol.split(':').collect::<Vec<_>>();
            if parts.len() > 2
                || !parts.iter().all(|part| component(part))
                || !symbols.insert(symbol.clone())
            {
                return Err(SetupError::validation(
                    "symbols must be distinct canonical names, optionally namespace:name",
                ));
            }
        }
        config.max_order_usd = self
            .max_order_usd
            .parse()
            .map_err(|_| SetupError::validation("invalid max_order_usd decimal"))?;
        config.max_position_usd = self
            .max_position_usd
            .parse()
            .map_err(|_| SetupError::validation("invalid max_position_usd decimal"))?;
        let gross = self
            .max_open_exposure_usd
            .parse()
            .map_err(|_| SetupError::validation("invalid max_open_exposure_usd decimal"))?;
        config.risk.max_open_exposure_usd = Some(gross);
        if config.max_order_usd <= 0.into()
            || config.max_order_usd > 15.into()
            || config.max_position_usd <= 0.into()
            || config.max_position_usd > 25.into()
            || gross <= 0.into()
            || gross > 25.into()
            || self.max_leverage != 1
            || !self.approval_required
        {
            return Err(SetupError::validation(
                "positive limits required; order <= 15 USD, position and gross <= 25 USD, leverage = 1 and approval_required = true",
            ));
        }
        config.symbols = symbols;
        config.risk.max_leverage = self.max_leverage;
        config.approval_required = true;
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<(), SetupError> {
        self.apply(&mut AgentGuardrails::default())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Idle,
    Reviewing,
    ReviewReady,
    Persisting,
    Saved,
    Failed,
    Uncertain,
    RecoveryRequired,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    Prerequisite,
    Conflict,
    Validation,
    Unavailable,
    Uncertain,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SetupError {
    pub kind: ErrorKind,
    pub detail: String,
}

impl SetupError {
    pub(super) fn new(kind: ErrorKind, detail: impl ToString) -> Self {
        Self {
            kind,
            detail: detail.to_string(),
        }
    }
    pub(super) fn conflict(detail: impl ToString) -> Self {
        Self::new(ErrorKind::Conflict, detail)
    }
    fn prerequisite(detail: impl ToString) -> Self {
        Self::new(ErrorKind::Prerequisite, detail)
    }
    fn validation(detail: impl ToString) -> Self {
        Self::new(ErrorKind::Validation, detail)
    }
}

pub(super) fn validate_request(
    agent: &str,
    account: Address,
    writers_stopped: bool,
) -> Result<(), SetupError> {
    if agent.is_empty()
        || agent.len() > 64
        || !agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || account == Address::ZERO
    {
        return Err(SetupError::validation(
            "a valid agent ID and nonzero account are required",
        ));
    }
    if !writers_stopped {
        return Err(SetupError::prerequisite(
            "confirm legacy writers are stopped",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Review {
    pub id: u64,
    pub agent: AgentId,
    pub account: Address,
    pub route: AuthorizedRoute,
    pub before: Option<PersistedState>,
    pub proposed: PersistedState,
    pub expected_revision: Option<u64>,
    pub legacy: Option<LegacyPolicyEvidence>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Status {
    pub phase: Phase,
    pub review: Option<Review>,
    pub receipt_revision: Option<u64>,
    pub error: Option<SetupError>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            review: None,
            receipt_revision: None,
            error: None,
        }
    }
}

pub(super) struct PreparedReview {
    pub view: Review,
    journal: PolicyJournal,
    legacy: Option<LegacyPolicyReview>,
    at_ms: u64,
}

impl PreparedReview {
    pub(super) fn open(
        dir: &Path,
        id: u64,
        agent: AgentId,
        account: Address,
        edits: PolicyEdits,
        empty_source_confirmed: bool,
        writers_stopped: bool,
    ) -> Result<Self, SetupError> {
        Self::open_with_keys(
            dir,
            id,
            agent,
            account,
            edits,
            empty_source_confirmed,
            writers_stopped,
            &KeychainKeyStore::new(Network::Testnet),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_with_keys(
        dir: &Path,
        id: u64,
        agent: AgentId,
        account: Address,
        edits: PolicyEdits,
        empty_source_confirmed: bool,
        writers_stopped: bool,
        keys: &dyn KeyStore,
    ) -> Result<Self, SetupError> {
        edits.validate()?;
        validate_request(agent.as_str(), account, writers_stopped)?;
        if account == Address::ZERO || keys.network() != Network::Testnet {
            return Err(SetupError::prerequisite(
                "a nonzero testnet registry account is required",
            ));
        }
        let path = dir.join(oppen_core::db_file_name(Network::Testnet));
        if !path.is_file() {
            return Err(SetupError::prerequisite("existing testnet ledger required"));
        }
        let hmac = keys
            .load_hmac_key()
            .map_err(SetupError::prerequisite)?
            .ok_or_else(|| {
                SetupError::prerequisite("existing testnet authentication key required")
            })?;
        let ledger = Arc::new(
            Ledger::open_existing(dir, Network::Testnet).map_err(SetupError::prerequisite)?,
        );
        let registry = Arc::new(
            RegistryJournal::open(ledger, Arc::new(hmac)).map_err(SetupError::prerequisite)?,
        );
        let at_ms =
            u64::try_from(oppen_core::ledger::now_ms()).map_err(SetupError::prerequisite)?;
        Self::review(
            dir,
            id,
            registry,
            agent,
            account,
            edits,
            empty_source_confirmed,
            writers_stopped,
            at_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn review(
        dir: &Path,
        id: u64,
        registry: Arc<RegistryJournal>,
        agent: AgentId,
        account: Address,
        edits: PolicyEdits,
        empty_source_confirmed: bool,
        writers_stopped: bool,
        at_ms: u64,
    ) -> Result<Self, SetupError> {
        let route = registry
            .route_for_agent(&agent)
            .map_err(SetupError::prerequisite)?;
        if account == Address::ZERO
            || route.network != Network::Testnet
            || route.binding.container != account
        {
            return Err(SetupError::prerequisite(
                "requested identity differs from the authorized testnet route",
            ));
        }
        let journal = PolicyJournal::new(registry.clone());
        let (before, expected_revision, legacy) = match journal.current() {
            Ok(current) => {
                if !current.state.is_globally_paused() {
                    return Err(SetupError::prerequisite(
                        "existing authenticated policy must already be globally paused",
                    ));
                }
                (Some(current.state), Some(current.revision), None)
            }
            Err(PolicyError::MigrationRequired) => {
                if !writers_stopped {
                    return Err(SetupError::prerequisite(
                        "confirm legacy writers are stopped",
                    ));
                }
                let legacy = LegacyPolicyReview::open(
                    dir.join("guardrails-testnet.db"),
                    Network::Testnet,
                    at_ms,
                )
                .map_err(SetupError::prerequisite)?;
                let before = if legacy.evidence().file_present {
                    Some(legacy.state().map_err(SetupError::prerequisite)?)
                } else {
                    if !empty_source_confirmed {
                        return Err(SetupError::prerequisite(
                            "confirm the absent legacy source explicitly",
                        ));
                    }
                    None
                };
                (before, None, Some(legacy))
            }
            Err(error) => return Err(SetupError::prerequisite(error)),
        };
        let mut proposed = before
            .clone()
            .unwrap_or_else(|| PersistedState::paused(at_ms));
        proposed.engage_global_pause(at_ms);
        edits.apply(proposed.guardrails.entry(agent.clone()).or_default())?;
        let view = Review {
            id,
            agent,
            account,
            route,
            before,
            proposed,
            expected_revision,
            legacy: legacy.as_ref().map(|review| review.evidence().clone()),
        };
        Ok(Self {
            view,
            journal,
            legacy,
            at_ms,
        })
    }

    pub(super) fn persist(&self) -> Result<u64, SetupError> {
        let result = match &self.legacy {
            Some(review) => self.journal.initialize_for_route(
                review,
                self.view.proposed.clone(),
                self.at_ms,
                &self.view.route,
            ),
            None => self.journal.replace_for_route(
                self.view
                    .expected_revision
                    .ok_or_else(|| SetupError::conflict("missing reviewed revision"))?,
                self.view.proposed.clone(),
                self.at_ms,
                &self.view.route,
            ),
        };
        let version = result.map_err(|error| match error {
            PolicyError::StaleRevision { .. } | PolicyError::RouteChanged => {
                SetupError::conflict(error)
            }
            PolicyError::MigrationRequired | PolicyError::Unavailable { .. } => {
                SetupError::prerequisite(error)
            }
            // The core IO/SQLite error does not expose whether COMMIT happened.
            PolicyError::Ledger(_) => SetupError::new(ErrorKind::Uncertain, error),
        })?;
        if version.state != self.view.proposed || !version.state.is_globally_paused() {
            return Err(SetupError::new(
                ErrorKind::Uncertain,
                "persisted policy differs from the retained paused candidate",
            ));
        }
        Ok(version.revision)
    }
}
