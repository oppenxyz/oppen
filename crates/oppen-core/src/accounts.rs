//! The sub-account registry: which Hyperliquid accounts oppen knows about,
//! whether it records each one, who owns it, and where an order for it routes.
//!
//! `docs/spec.md` D1 maps the agent roster 1:1 onto accounts, so this is also
//! the answer to "which address does this agent trade in". `AGENTS.md`
//! invariant 7 forbids a second event store and the same reasoning applies to
//! the roster: the ledger's `sub_accounts` table is the only store, and this
//! module is a typed surface over it rather than a cache beside it. Every
//! method here reads or writes that table through [`crate::ledger::Ledger`].
//!
//! The product rules it makes structural are cited where they bite:
//! `docs/decisions.md` R3 on [`Standing`] and [`Registry::observe`], R2 on
//! [`Registry::provision`], `docs/specs/history.md` 3.4 on [`Scope`] and
//! [`Registry::retire`], and `docs/spec.md` item 33 on [`Classification`].
//!
//! **Operator-only.** `docs/spec.md` item 22 and `AGENTS.md` invariant 3: no
//! agent-reachable path modifies the agent registry. That is already true by
//! construction, because a [`Registry`] borrows a `&Ledger` and the ledger
//! hands agents an `AgentView` instead. Do not expose one of these through an
//! MCP tool.
//!
//! **What this module does not do.** It writes no chained event. Opting an
//! account in and retiring one are operator actions and belong in the record,
//! but the operator command that calls these already owns that write through
//! the guardrail engine's audit sink, and a second row appended here would put
//! the same act in the chain twice.

use std::collections::BTreeMap;

use oppen_hl::Address;
use oppen_hl::types::SubAccount as VenueSubAccount;
use serde::Serialize;

use crate::guardrail::AgentId;
use crate::ledger::{Ledger, LedgerError, Owner, OwnerType, SubAccount};

/// The operator-facing name of the bucket every unattributed fill lands in
/// (`docs/spec.md` item 33, `docs/specs/history.md` §2).
///
/// One definition, so the middle dot cannot drift into a hyphen in one view and
/// a bullet in another. It is a display string; the wire form is
/// [`Classification`]'s `manual_external`.
pub const MANUAL_EXTERNAL_BUCKET: &str = "manual · external";

/// What oppen is doing with an account right now.
///
/// `docs/decisions.md` R3 describes a lifecycle rather than a pair of booleans.
/// The stored row carries three flags — `recorded`, `provisioned_by_oppen`,
/// `active` — and this is the only place that reads meaning out of their
/// combination. `provisioned_by_oppen` without `recorded` is unreachable through
/// this module ([`Registry::opt_out`] refuses it); a hand-edited database that
/// holds it anyway reads as [`Standing::Discovered`], which is R3's default and
/// the safe direction to fail in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// Seen under the master and not opted in. R3's default for anything oppen
    /// did not provision.
    Discovered,
    /// The operator ticked it. oppen records its history but did not create it.
    OptedIn,
    /// oppen created it, so it is recorded from birth (R3: it watches what it
    /// made).
    ProvisionedByOppen,
    /// Retired. Not deleted: `docs/specs/history.md` 3.4 keeps the row and its
    /// past, excludes it from live views, and counts it in all-time totals.
    Retired,
}

impl Standing {
    fn of(row: &SubAccount) -> Standing {
        if !row.active {
            Standing::Retired
        } else if !row.recorded {
            Standing::Discovered
        } else if row.provisioned_by_oppen {
            Standing::ProvisionedByOppen
        } else {
            Standing::OptedIn
        }
    }

    /// Whether oppen collects new history for this account. [`Standing::Retired`]
    /// is `false`, so use [`Scope::All`] to count a retired account in a total.
    pub fn is_recording(self) -> bool {
        matches!(self, Standing::OptedIn | Standing::ProvisionedByOppen)
    }
}

/// Which bucket an account's activity is attributed to.
///
/// `docs/spec.md` item 33 and `docs/specs/history.md` §2. An account with an
/// owner is attributable to that owner; anything else — the master account
/// itself, an account made in the Hyperliquid web app, a discovered one the
/// operator merely watches — is `manual · external`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// A paired agent's account (D1, one per agent).
    Agent,
    /// A workflow's account (`docs/decisions.md` R2; the rule that decides
    /// whether workflows get their own is still open).
    Workflow,
    /// Everything with no owner, including addresses the registry has never
    /// seen.
    ManualExternal,
}

impl Classification {
    fn of(row: &SubAccount) -> Classification {
        match row.owner.as_ref().map(|owner| owner.owner_type) {
            Some(OwnerType::Agent) => Classification::Agent,
            Some(OwnerType::Workflow) => Classification::Workflow,
            None => Classification::ManualExternal,
        }
    }

    /// The operator-facing bucket name, for a roster column or a fill row.
    pub fn label(self) -> &'static str {
        match self {
            Classification::Agent => "agent",
            Classification::Workflow => "workflow",
            Classification::ManualExternal => MANUAL_EXTERNAL_BUCKET,
        }
    }
}

/// Which rows a listing returns.
///
/// Named rather than left to a boolean at each call site, because
/// `docs/specs/history.md` 3.4 puts a retired account in two answers with
/// opposite defaults: out of every live view, into every all-time total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Active accounts. What a roster, a position table or a live PnL asks for.
    Live,
    /// Active accounts oppen records history for (R3 opt-in). What the
    /// backfill and the fill poller iterate.
    Recorded,
    /// Every row ever registered, retired included. What an all-time total and
    /// an audit export ask for.
    All,
}

impl Scope {
    fn admits(self, standing: Standing) -> bool {
        match self {
            Scope::Live => standing != Standing::Retired,
            Scope::Recorded => standing.is_recording(),
            Scope::All => true,
        }
    }
}

/// Where an order for an account is routed on the wire.
///
/// `docs/hl-signing.md` §"Sub-account routing": a sub-account action carries
/// `vaultAddress`, a top-level account carries none. D1 requires the route to be
/// a property of the container rather than hardcoded either way, and it keeps
/// `vaultAddress` off the call site — a hand-built one can be the wrong case,
/// the wrong account, or empty, and the venue answers all three with the same
/// opaque rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Sign for this sub-account: `vaultAddress` is set to it.
    SubAccount(Address),
    /// Sign for the master account itself: `vaultAddress` is omitted. Only the
    /// manual escape hatch (`docs/spec.md` item 33) routes here.
    Master,
}

impl Route {
    /// The `vaultAddress` field of the exchange request, ready for
    /// `oppen_hl::ExchangeRequest`.
    pub fn vault_address(self) -> Option<Address> {
        match self {
            Route::SubAccount(address) => Some(address),
            Route::Master => None,
        }
    }
}

/// A registered account, with its stored flags already read as a lifecycle.
///
/// The address is parsed once here so that no caller downstream handles the
/// stored text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    /// The account address, lowercase and `0x`-prefixed.
    pub address: Address,
    /// Operator-facing name. Untrusted display text — it comes from the venue
    /// or from an operator, so render it as plain text like any other
    /// (`AGENTS.md` invariant 9 is about agent `reason` strings, but nothing
    /// here is safer than one).
    pub name: String,
    /// `None` for anything oppen merely discovered. `docs/decisions.md` R2
    /// keeps the discriminator while the product rule stays open.
    pub owner: Option<Owner>,
    /// Where this account sits in R3's lifecycle.
    pub standing: Standing,
    /// Which bucket its activity is attributed to (`docs/spec.md` item 33).
    pub classification: Classification,
    /// Whether oppen created it. Kept beside [`Account::standing`] because
    /// retirement hides it: a retired account that oppen provisioned is still
    /// distinguishable from one it merely found.
    pub provisioned_by_oppen: bool,
    /// When the row was first written, in unix milliseconds. Rediscovering an
    /// account does not make it new.
    pub created_ts_ms: i64,
}

impl Account {
    /// Read a stored row, failing rather than panicking on an address it cannot
    /// hold.
    fn from_row(row: &SubAccount) -> Result<Account, AccountsError> {
        let address =
            Address::parse(&row.address).map_err(|_| AccountsError::MalformedAddress {
                stored: row.address.clone(),
            })?;
        Ok(Account {
            address,
            name: row.name.clone(),
            owner: row.owner.clone(),
            standing: Standing::of(row),
            classification: Classification::of(row),
            provisioned_by_oppen: row.provisioned_by_oppen,
            created_ts_ms: row.created_ts_ms,
        })
    }
}

/// One sub-account as the venue reports it, reduced to what the registry
/// stores.
///
/// The boundary between discovery and the registry. `InfoClient::sub_accounts`
/// is async and lives in `oppen-hl`; everything here is a blocking SQLite call,
/// so the caller does the request and hands the result over as a slice. A test
/// supplies the slice directly, or a JSON fixture through
/// [`Discovered::from_venue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub address: Address,
    /// The name the venue reports for it.
    pub name: String,
}

impl Discovered {
    /// Convert one `subAccounts` entry, checking it really is the master's.
    ///
    /// **The response shape is unverified against a live account.**
    /// `oppen_hl::types::SubAccount` was written from the documentation and the
    /// probe address had no sub-accounts, so the first real round trip is what
    /// confirms it. That is exactly why the master is checked here rather than
    /// assumed: if the field means something other than what the docs say, or
    /// the caller passes the wrong master, the failure is a typed refusal
    /// instead of somebody else's account silently entering the roster and
    /// being offered as a route to sign into.
    pub fn from_venue(entry: &VenueSubAccount, master: Address) -> Result<Self, AccountsError> {
        if entry.master != master {
            return Err(AccountsError::ForeignMaster {
                address: entry.sub_account_user,
                expected: master,
                found: entry.master,
            });
        }
        Ok(Discovered {
            address: entry.sub_account_user,
            name: entry.name.clone(),
        })
    }
}

/// What one discovery sweep changed.
///
/// Returned rather than logged so the console can say "3 new sub-accounts
/// found, none recorded" — R3 makes discovery a prompt to the operator, and a
/// prompt needs to know what is new.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiscoveryReport {
    /// Accounts the registry had never seen, now stored as
    /// [`Standing::Discovered`]. Ascending by address.
    pub added: Vec<Address>,
    /// Known accounts whose venue name changed. Ascending by address.
    pub renamed: Vec<Address>,
    /// Known accounts the sweep left untouched.
    pub unchanged: usize,
}

/// What can go wrong in the registry.
///
/// `AGENTS.md` conventions: `thiserror`, and no panic on an input path. The
/// database file and the venue response are both untrusted inputs.
#[derive(Debug, thiserror::Error)]
pub enum AccountsError {
    /// The underlying store refused.
    #[error("registry ledger error: {0}")]
    Ledger(#[from] LedgerError),
    /// No row at that address. Distinct from `manual · external`: a caller that
    /// wants to *classify* an unknown address gets the bucket, a caller that
    /// wants to *change* one gets this.
    #[error("no sub-account is registered at {0}")]
    UnknownAccount(Address),
    /// No account is bound to that agent. Never resolved to the master account:
    /// silently routing an agent's order to the master would put its position in
    /// the operator's own account, which is the failure D1 exists to prevent.
    #[error("no sub-account is bound to agent {0}")]
    UnknownAgent(AgentId),
    /// Two rows claim the same agent, so D1's 1:1 roster does not hold and there
    /// is no single right answer for `vaultAddress`. Reachable by editing the
    /// database or by calling `crate::ledger::Ledger::upsert_sub_account`
    /// directly, which is why the route lookup checks rather than taking the
    /// first row.
    #[error("agent {agent} maps to more than one sub-account: {first} and {second}")]
    AmbiguousAgent {
        agent: AgentId,
        /// Lowest address claiming it.
        first: Address,
        second: Address,
    },
    /// The account is retired. `docs/specs/history.md` 3.4 keeps its history,
    /// but it takes no new orders and no new opt-in.
    #[error("sub-account {0} is retired")]
    Retired(Address),
    /// The address is already bound to a different owner. Rebinding it would
    /// re-attribute every fill it has ever produced, because attribution is by
    /// account (D1).
    #[error("sub-account {address} is owned by {current}, not {offered}")]
    OwnerConflict {
        address: Address,
        /// Owners as `type:id`.
        current: String,
        offered: String,
    },
    /// The owner is already bound to another account. D1 maps the roster 1:1, so
    /// a second account for the same agent is a fork of its PnL.
    #[error("{owner} is already bound to sub-account {address}")]
    OwnerAlreadyBound {
        /// The owner, as `type:id`.
        owner: String,
        address: Address,
    },
    /// Recording cannot be turned off for an account oppen created. R3's
    /// default-off is about accounts oppen did not provision; turning it off for
    /// one oppen made would stop recording the history of an account its own
    /// agent is trading, and nothing downstream would report the hole. Retire it
    /// instead.
    #[error("sub-account {0} was provisioned by oppen; retire it rather than stop recording it")]
    ProvisionedCannotOptOut(Address),
    /// A stored row holds something that is not an address. Only reachable by
    /// editing the database file.
    #[error("registry holds {stored:?}, which is not an address")]
    MalformedAddress {
        /// The stored text, verbatim.
        stored: String,
    },
    /// A discovered entry names a different master. Refused rather than adopted;
    /// see [`Discovered::from_venue`].
    #[error("sub-account {address} reports master {found}, not {expected}")]
    ForeignMaster {
        address: Address,
        /// The master the sweep was run for.
        expected: Address,
        /// The master the entry claims.
        found: Address,
    },
}

/// The sub-account registry.
///
/// A typed surface over the ledger's `sub_accounts` table, holding no state of
/// its own. Operator-only; see the module doc.
#[derive(Debug, Clone, Copy)]
pub struct Registry<'l> {
    ledger: &'l Ledger,
}

impl<'l> Registry<'l> {
    /// Open the registry over a ledger.
    ///
    /// Takes `&Ledger` rather than the agent-facing `AgentView`, which is what
    /// makes `AGENTS.md` invariant 3 hold by construction.
    pub fn new(ledger: &'l Ledger) -> Self {
        Registry { ledger }
    }

    /// Record an account oppen created, bound to its owner.
    ///
    /// Recorded from birth: R3's default-off covers accounts oppen did not
    /// provision, and this is one it did. `owner` carries R2's discriminator and
    /// both [`OwnerType`] values are accepted, because whether a workflow gets
    /// its own account or binds to an agent's is a product rule
    /// `docs/decisions.md` R2 deliberately leaves open.
    ///
    /// Idempotent for the same owner, so a re-run after a crash between the
    /// on-chain `createSubAccount` and this write converges. It refuses to
    /// re-point an account at a different owner, to bind an owner that already
    /// has an account (D1 is 1:1), or to revive a retired one — reusing a
    /// retired address for a new agent would attach the old agent's entire
    /// history to the new one.
    pub fn provision(
        &self,
        address: Address,
        name: &str,
        owner: Owner,
        now_ms: i64,
    ) -> Result<Account, AccountsError> {
        let key = address.to_string();
        let existing = self.ledger.sub_account(&key)?;
        if let Some(row) = &existing {
            if !row.active {
                return Err(AccountsError::Retired(address));
            }
            if let Some(current) = &row.owner
                && *current != owner
            {
                return Err(AccountsError::OwnerConflict {
                    address,
                    current: owner_label(current),
                    offered: owner_label(&owner),
                });
            }
        }
        if let Some(bound) = self.bound_to(&owner)?
            && bound != address
        {
            return Err(AccountsError::OwnerAlreadyBound {
                owner: owner_label(&owner),
                address: bound,
            });
        }
        let row = SubAccount {
            address: key,
            name: name.to_owned(),
            owner: Some(owner),
            recorded: true,
            provisioned_by_oppen: true,
            active: true,
            created_ts_ms: existing.map_or(now_ms, |row| row.created_ts_ms),
        };
        self.ledger.upsert_sub_account(&row)?;
        Account::from_row(&row)
    }

    /// Fold one discovery sweep into the registry (R3, "discover all").
    ///
    /// New accounts land as [`Standing::Discovered`] — present in the roster,
    /// recording nothing, waiting for the operator's tick. Known accounts have
    /// their name refreshed and **nothing else touched**: the opt-in bit,
    /// retirement and the creation timestamp all survive, because a sweep is an
    /// observation and not an instruction. A sweep that reset the opt-in bit
    /// would silently stop recording an account the operator ticked weeks ago,
    /// and nothing downstream would report a gap — a never-recorded account has
    /// no gap.
    ///
    /// An account missing from `seen` is never retired. Hyperliquid has no
    /// delete for a sub-account, so absence means a truncated or failed response
    /// far more often than it means anything about the account, and retiring the
    /// roster on one bad response would take every recorded account out of the
    /// live views at once.
    ///
    /// Duplicate addresses in `seen` collapse, last name winning. Iteration is
    /// by address so the report and the write order are the same on every run
    /// (`AGENTS.md` invariant 6).
    pub fn observe(
        &self,
        seen: &[Discovered],
        now_ms: i64,
    ) -> Result<DiscoveryReport, AccountsError> {
        let mut wanted: BTreeMap<String, (Address, &str)> = BTreeMap::new();
        for entry in seen {
            wanted.insert(
                entry.address.to_string(),
                (entry.address, entry.name.as_str()),
            );
        }
        let mut report = DiscoveryReport::default();
        for (key, (address, name)) in wanted {
            match self.ledger.sub_account(&key)? {
                Some(row) if row.name == name => report.unchanged += 1,
                Some(row) => {
                    self.ledger.upsert_sub_account(&SubAccount {
                        name: name.to_owned(),
                        ..row
                    })?;
                    report.renamed.push(address);
                }
                None => {
                    self.ledger.upsert_sub_account(&SubAccount {
                        address: key,
                        name: name.to_owned(),
                        owner: None,
                        recorded: false,
                        provisioned_by_oppen: false,
                        active: true,
                        created_ts_ms: now_ms,
                    })?;
                    report.added.push(address);
                }
            }
        }
        Ok(report)
    }

    /// Start recording an account the operator ticked (R3, "opt in per
    /// account").
    ///
    /// Idempotent. Refuses a retired account: a retired row's history is closed
    /// (`docs/specs/history.md` 3.4), and reopening recording on it without
    /// reviving it would produce fills against an account no live view shows.
    pub fn opt_in(&self, address: Address) -> Result<Account, AccountsError> {
        self.set_recorded(address, true)
    }

    /// Stop recording an account the operator had ticked.
    ///
    /// Only for an account oppen did not provision; see
    /// [`AccountsError::ProvisionedCannotOptOut`]. Idempotent otherwise.
    pub fn opt_out(&self, address: Address) -> Result<Account, AccountsError> {
        self.set_recorded(address, false)
    }

    /// The one write behind [`Registry::opt_in`] and [`Registry::opt_out`], so
    /// the retired refusal and the idempotent return cannot drift apart.
    fn set_recorded(&self, address: Address, recorded: bool) -> Result<Account, AccountsError> {
        let row = self.row(address)?;
        if !row.active {
            return Err(AccountsError::Retired(address));
        }
        if !recorded && row.provisioned_by_oppen {
            return Err(AccountsError::ProvisionedCannotOptOut(address));
        }
        if row.recorded == recorded {
            return Account::from_row(&row);
        }
        let updated = SubAccount { recorded, ..row };
        self.ledger.upsert_sub_account(&updated)?;
        Account::from_row(&updated)
    }

    /// Retire an account: mark it inactive, delete nothing.
    ///
    /// `docs/specs/history.md` 3.4 — deleting an agent in the UI deletes the
    /// pairing, not the past. The row stays, keeps its owner so its fills stay
    /// attributed, drops out of [`Scope::Live`] and stays in [`Scope::All`].
    /// After this, [`Registry::route_for_agent`] refuses, so a retired agent
    /// cannot be signed for.
    ///
    /// Idempotent. There is no un-retire: an operator who retires the wrong
    /// account has lost nothing, and a revival would silently re-arm a route.
    pub fn retire(&self, address: Address) -> Result<Account, AccountsError> {
        let row = self.row(address)?;
        if !row.active {
            return Account::from_row(&row);
        }
        let updated = SubAccount {
            active: false,
            ..row
        };
        self.ledger.upsert_sub_account(&updated)?;
        Account::from_row(&updated)
    }

    /// One account by address, or `None` if the registry has never seen it.
    pub fn get(&self, address: Address) -> Result<Option<Account>, AccountsError> {
        match self.ledger.sub_account(&address.to_string())? {
            Some(row) => Ok(Some(Account::from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Every account in `scope`, ascending by address.
    ///
    /// Ordered rather than left to the storage engine, so anything built from
    /// this — a roster view, an export, a hash — is byte-identical across runs
    /// (`AGENTS.md` invariant 6).
    pub fn list(&self, scope: Scope) -> Result<Vec<Account>, AccountsError> {
        let mut accounts = Vec::new();
        for row in self.ledger.sub_accounts()? {
            let account = Account::from_row(&row)?;
            if scope.admits(account.standing) {
                accounts.push(account);
            }
        }
        Ok(accounts)
    }

    /// Which bucket an address's activity belongs to (`docs/spec.md` item 33).
    ///
    /// An address the registry has never seen is
    /// [`Classification::ManualExternal`] rather than an error:
    /// `docs/specs/history.md` §2 says a fill is never dropped for failing to
    /// match, and this is the bucket it lands in. A retired account keeps the
    /// classification it had, so its old fills stay attributed to its agent.
    pub fn classify(&self, address: Address) -> Result<Classification, AccountsError> {
        Ok(self
            .ledger
            .sub_account(&address.to_string())?
            .as_ref()
            .map_or(Classification::ManualExternal, Classification::of))
    }

    /// The route for one agent's orders (D1).
    ///
    /// The caller hands over an [`AgentId`] and gets a [`Route`] whose
    /// [`Route::vault_address`] goes straight into `oppen_hl::ExchangeRequest`.
    /// The guardrail engine keeps its own copy of the binding — it stamps the
    /// `vaultAddress` onto every clearance so a later caller cannot supply a
    /// different one — and this registry is where that copy comes from at
    /// registration time.
    ///
    /// Refuses rather than guessing in all three ways it can be uncertain: an
    /// unknown agent, a retired one, and an agent bound to two accounts. Each of
    /// them, resolved to a plausible answer, signs an order into an account that
    /// is not the agent's.
    pub fn route_for_agent(&self, agent: &AgentId) -> Result<Route, AccountsError> {
        let mut found: Option<Account> = None;
        for row in self.ledger.sub_accounts()? {
            if !owned_by(&row, OwnerType::Agent, agent.as_str()) {
                continue;
            }
            let account = Account::from_row(&row)?;
            if let Some(first) = &found {
                return Err(AccountsError::AmbiguousAgent {
                    agent: agent.clone(),
                    first: first.address,
                    second: account.address,
                });
            }
            found = Some(account);
        }
        let account = found.ok_or_else(|| AccountsError::UnknownAgent(agent.clone()))?;
        if account.standing == Standing::Retired {
            return Err(AccountsError::Retired(account.address));
        }
        Ok(Route::SubAccount(account.address))
    }

    /// The address an owner is already bound to, if any.
    ///
    /// Scans the table rather than indexing it: D1's 1:1 mapping over a
    /// human-kept roster means tens of rows, so an index here would be a schema
    /// change for no measurable gain.
    fn bound_to(&self, owner: &Owner) -> Result<Option<Address>, AccountsError> {
        for row in self.ledger.sub_accounts()? {
            if owned_by(&row, owner.owner_type, &owner.owner_id) {
                return Ok(Some(Account::from_row(&row)?.address));
            }
        }
        Ok(None)
    }

    /// The stored row at `address`, or [`AccountsError::UnknownAccount`].
    fn row(&self, address: Address) -> Result<SubAccount, AccountsError> {
        self.ledger
            .sub_account(&address.to_string())?
            .ok_or(AccountsError::UnknownAccount(address))
    }
}

/// Whether a row is owned by exactly this owner.
fn owned_by(row: &SubAccount, owner_type: OwnerType, owner_id: &str) -> bool {
    row.owner
        .as_ref()
        .is_some_and(|owner| owner.owner_type == owner_type && owner.owner_id == owner_id)
}

/// An owner rendered for an error message, as `type:id`.
fn owner_label(owner: &Owner) -> String {
    format!("{}:{}", owner.owner_type.as_str(), owner.owner_id)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::Network;

    const AGENT_A: &str = "0x00000000000000000000000000000000000000a1";
    const AGENT_B: &str = "0x00000000000000000000000000000000000000b2";
    const OPERATOR: &str = "0x00000000000000000000000000000000000000c3";
    const MASTER: &str = "0x00000000000000000000000000000000000000ff";
    const STRANGER: &str = "0x00000000000000000000000000000000000000ee";

    fn address(text: &str) -> Address {
        Address::parse(text).expect("test literal parses")
    }

    fn ledger(dir: &TempDir) -> Ledger {
        Ledger::open(dir.path(), Network::Testnet).expect("open ledger")
    }

    fn agent_owner(id: &str) -> Owner {
        Owner {
            owner_type: OwnerType::Agent,
            owner_id: id.to_owned(),
        }
    }

    fn discovered(text: &str, name: &str) -> Discovered {
        Discovered {
            address: address(text),
            name: name.to_owned(),
        }
    }

    /// D1: an agent's account is recorded from birth and routes as a
    /// `vaultAddress` without the caller ever touching a string.
    #[test]
    fn a_provisioned_account_is_recorded_and_routes() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);

        let account = registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1_700_000)
            .expect("provision");
        assert_eq!(account.standing, Standing::ProvisionedByOppen);
        assert_eq!(account.classification, Classification::Agent);
        assert!(account.standing.is_recording());

        let route = registry
            .route_for_agent(&AgentId::new("carry"))
            .expect("route");
        assert_eq!(route, Route::SubAccount(address(AGENT_A)));
        assert_eq!(route.vault_address(), Some(address(AGENT_A)));
        assert_eq!(Route::Master.vault_address(), None);

        // Idempotent: the second write keeps the first creation timestamp.
        let again = registry
            .provision(
                address(AGENT_A),
                "carry v2",
                agent_owner("carry"),
                9_999_999,
            )
            .expect("re-provision");
        assert_eq!(again.created_ts_ms, 1_700_000);
        assert_eq!(again.name, "carry v2");
    }

    /// R3: default off for anything oppen did not provision, and a later sweep
    /// must not undo the operator's tick. A naive upsert of the discovery shape
    /// would silently reset `recorded` here and stop recording an account with
    /// no gap to show for it.
    #[test]
    fn discovery_defaults_to_off_and_a_resweep_preserves_the_opt_in() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);

        let report = registry
            .observe(
                &[discovered(OPERATOR, "manual"), discovered(AGENT_B, "b")],
                10,
            )
            .expect("observe");
        assert_eq!(report.added, vec![address(AGENT_B), address(OPERATOR)]);
        assert_eq!(report.unchanged, 0);
        for account in registry.list(Scope::Live).expect("list") {
            assert_eq!(account.standing, Standing::Discovered);
            assert!(!account.standing.is_recording());
        }
        assert!(registry.list(Scope::Recorded).expect("list").is_empty());

        let opted = registry.opt_in(address(OPERATOR)).expect("opt in");
        assert_eq!(opted.standing, Standing::OptedIn);

        let report = registry
            .observe(
                &[
                    discovered(OPERATOR, "manual renamed"),
                    discovered(AGENT_B, "b"),
                ],
                20,
            )
            .expect("re-observe");
        assert_eq!(report.added, Vec::new());
        assert_eq!(report.renamed, vec![address(OPERATOR)]);
        assert_eq!(report.unchanged, 1);

        let still = registry.get(address(OPERATOR)).expect("get").expect("row");
        assert_eq!(still.standing, Standing::OptedIn);
        assert_eq!(still.name, "manual renamed");
        assert_eq!(still.created_ts_ms, 10);
        assert_eq!(
            registry
                .list(Scope::Recorded)
                .expect("list")
                .iter()
                .map(|account| account.address)
                .collect::<Vec<_>>(),
            vec![address(OPERATOR)]
        );
    }

    /// `docs/specs/history.md` 3.4: retiring deletes the pairing, not the past.
    #[test]
    fn retiring_keeps_the_row_and_closes_the_route() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");

        let retired = registry.retire(address(AGENT_A)).expect("retire");
        assert_eq!(retired.standing, Standing::Retired);
        assert!(retired.provisioned_by_oppen);
        // The owner survives, so its old fills stay attributed to the agent.
        assert_eq!(retired.classification, Classification::Agent);

        assert!(registry.list(Scope::Live).expect("live").is_empty());
        assert!(registry.list(Scope::Recorded).expect("recorded").is_empty());
        assert_eq!(registry.list(Scope::All).expect("all").len(), 1);
        assert!(registry.get(address(AGENT_A)).expect("get").is_some());

        assert!(matches!(
            registry.route_for_agent(&AgentId::new("carry")),
            Err(AccountsError::Retired(_))
        ));
        assert!(matches!(
            registry.opt_in(address(AGENT_A)),
            Err(AccountsError::Retired(_))
        ));
        // A sweep that still sees it does not revive it.
        registry
            .observe(&[discovered(AGENT_A, "carry")], 30)
            .expect("observe");
        assert_eq!(
            registry
                .get(address(AGENT_A))
                .expect("get")
                .expect("row")
                .standing,
            Standing::Retired
        );
        // Retiring twice is a no-op, not an error.
        assert_eq!(
            registry.retire(address(AGENT_A)).expect("retire").standing,
            Standing::Retired
        );
    }

    /// Spec item 33: the bucket is an account property, including for an
    /// address the registry has never seen.
    #[test]
    fn anything_unowned_is_the_manual_external_bucket() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        registry
            .observe(&[discovered(OPERATOR, "manual")], 1)
            .expect("observe");
        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");

        assert_eq!(
            registry.classify(address(OPERATOR)).expect("classify"),
            Classification::ManualExternal
        );
        assert_eq!(
            registry.classify(address(STRANGER)).expect("classify"),
            Classification::ManualExternal
        );
        assert_eq!(
            registry.classify(address(AGENT_A)).expect("classify"),
            Classification::Agent
        );
        assert_eq!(
            Classification::ManualExternal.label(),
            "manual \u{b7} external"
        );
        assert_eq!(MANUAL_EXTERNAL_BUCKET, "manual \u{b7} external");
        assert_eq!(
            serde_json::to_string(&Classification::ManualExternal).expect("json"),
            "\"manual_external\""
        );
    }

    /// R2: the discriminator carries a workflow owner today even though the
    /// product rule that decides whether workflows get their own sub-account is
    /// still open.
    #[test]
    fn a_workflow_owner_is_representable_and_classified() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        let account = registry
            .provision(
                address(AGENT_B),
                "guardian",
                Owner {
                    owner_type: OwnerType::Workflow,
                    owner_id: "position-guardian".to_owned(),
                },
                1,
            )
            .expect("provision");
        assert_eq!(account.classification, Classification::Workflow);
        // A workflow-owned account is not an agent route.
        assert!(matches!(
            registry.route_for_agent(&AgentId::new("position-guardian")),
            Err(AccountsError::UnknownAgent(_))
        ));
    }

    /// D1 is 1:1 in both directions, and the route lookup does not trust the
    /// table to hold it — `Ledger::upsert_sub_account` is public, so a second
    /// binding can arrive from outside this module.
    #[test]
    fn one_agent_maps_to_one_account_in_both_directions() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");

        assert!(matches!(
            registry.provision(address(AGENT_B), "carry again", agent_owner("carry"), 2),
            Err(AccountsError::OwnerAlreadyBound { .. })
        ));
        assert!(matches!(
            registry.provision(address(AGENT_A), "stolen", agent_owner("basis"), 2),
            Err(AccountsError::OwnerConflict { .. })
        ));

        ledger
            .upsert_sub_account(&SubAccount {
                address: AGENT_B.to_owned(),
                name: "smuggled".to_owned(),
                owner: Some(agent_owner("carry")),
                recorded: true,
                provisioned_by_oppen: false,
                active: true,
                created_ts_ms: 3,
            })
            .expect("upsert behind the registry");
        assert!(matches!(
            registry.route_for_agent(&AgentId::new("carry")),
            Err(AccountsError::AmbiguousAgent { .. })
        ));
    }

    /// R3's default-off is about accounts oppen did not provision. Turning
    /// recording off for one it made would leave its agent trading unrecorded.
    #[test]
    fn recording_cannot_be_turned_off_for_an_account_oppen_made() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");
        assert!(matches!(
            registry.opt_out(address(AGENT_A)),
            Err(AccountsError::ProvisionedCannotOptOut(_))
        ));

        registry
            .observe(&[discovered(OPERATOR, "manual")], 1)
            .expect("observe");
        registry.opt_in(address(OPERATOR)).expect("opt in");
        let out = registry.opt_out(address(OPERATOR)).expect("opt out");
        assert_eq!(out.standing, Standing::Discovered);
        // Idempotent in both directions.
        assert_eq!(
            registry.opt_out(address(OPERATOR)).expect("again").standing,
            Standing::Discovered
        );
        assert_eq!(
            registry.opt_in(address(OPERATOR)).expect("in").standing,
            Standing::OptedIn
        );
        assert!(matches!(
            registry.opt_in(address(STRANGER)),
            Err(AccountsError::UnknownAccount(_))
        ));
    }

    /// The discovery boundary, against the documented `subAccounts` shape.
    ///
    /// **Unverified live.** The probe address had no sub-accounts, so this
    /// fixture is the documentation's shape and not a captured response. It
    /// pins what the registry expects; the first real round trip is what
    /// confirms it.
    #[test]
    fn the_venue_shape_converts_and_a_foreign_master_is_refused() {
        let body = format!(
            r#"[{{"name":"trader","subAccountUser":"{AGENT_A}","master":"{MASTER}",
                 "clearinghouseState":{{
                   "marginSummary":{{"accountValue":"100.0","totalNtlPos":"0.0",
                     "totalRawUsd":"100.0","totalMarginUsed":"0.0"}},
                   "crossMarginSummary":{{"accountValue":"100.0","totalNtlPos":"0.0",
                     "totalRawUsd":"100.0","totalMarginUsed":"0.0"}},
                   "crossMaintenanceMarginUsed":"0.0","withdrawable":"100.0",
                   "assetPositions":[],"time":1756800000000}}}}]"#
        );
        let entries: Vec<VenueSubAccount> =
            serde_json::from_str(&body).expect("documented subAccounts shape");
        let converted = Discovered::from_venue(&entries[0], address(MASTER)).expect("convert");
        assert_eq!(converted, discovered(AGENT_A, "trader"));

        // Somebody else's sub-account never enters the roster, so it is never
        // offered as a route to sign into.
        assert!(matches!(
            Discovered::from_venue(&entries[0], address(STRANGER)),
            Err(AccountsError::ForeignMaster { .. })
        ));
    }

    /// The database file is user-writable, so every read of it is an input
    /// path: a row that cannot hold an address is an error, never a panic.
    #[test]
    fn a_row_that_is_not_an_address_is_an_error() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        ledger
            .upsert_sub_account(&SubAccount {
                address: "not-an-address".to_owned(),
                name: "hand edited".to_owned(),
                owner: None,
                recorded: false,
                provisioned_by_oppen: false,
                active: true,
                created_ts_ms: 1,
            })
            .expect("upsert");
        assert!(matches!(
            registry.list(Scope::All),
            Err(AccountsError::MalformedAddress { .. })
        ));
    }

    /// A hand-edited row that claims to be provisioned without being recorded
    /// reads as not recorded, which is R3's default and the safe direction.
    #[test]
    fn an_impossible_flag_combination_fails_towards_not_recording() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        ledger
            .upsert_sub_account(&SubAccount {
                address: AGENT_A.to_owned(),
                name: "hand edited".to_owned(),
                owner: Some(agent_owner("carry")),
                recorded: false,
                provisioned_by_oppen: true,
                active: true,
                created_ts_ms: 1,
            })
            .expect("upsert");
        let account = registry.get(address(AGENT_A)).expect("get").expect("row");
        assert_eq!(account.standing, Standing::Discovered);
        assert!(!account.standing.is_recording());
        assert!(account.provisioned_by_oppen);
    }
}
