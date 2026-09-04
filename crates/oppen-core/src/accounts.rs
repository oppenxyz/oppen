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
//! **Nothing this module refuses is refused at the signer.**
//! `crate::guardrail`'s engine stamps every `Clearance` with a `vault_address`
//! taken from its own `AgentId -> Address` map, persisted in the
//! `guardrail_vault` table, written from an argument its caller supplies when
//! an agent is registered and never re-read from here; `sign_cleared` is the
//! signing path. This table is authoritative for the roster and is not
//! consulted there.
//!
//! **The gap is wider than a stale copy**, and the difference decides what
//! closes it. Under `docs/decisions.md` V2 every Hyperliquid v1 container is
//! top-level, so `register_agent` is called with `None`, that map stays
//! **empty**, and the container is implied by whichever agent key signs rather
//! than named anywhere. Deleting `guardrail_vault` would therefore end a
//! disagreement without restoring a revocation: a retired agent is still
//! cleared and still signed for, in both container shapes, on an engine
//! reopened from the same database file. Only the engine asking this module per
//! decision closes it.
//!
//! `docs/decisions.md` R7 settles which store owns the binding: "oppen's SQLite
//! registry is the only place the agent → container → agent-wallet binding
//! lives", because a top-level container is undiscoverable and nothing at the
//! venue can rebuild the list. The guardrail copy is therefore a cache, and the
//! obligation on it is exactly this: delete `guardrail_vault` and the engine's
//! `vaults` map, and have every decision resolve the binding by calling
//! [`Registry::route_for_agent`], carrying the whole [`Route`] rather than an
//! `Option<Address>` — the `Option` collapses a top-level container, which is
//! every Hyperliquid v1 container (`docs/decisions.md` V2), into the same
//! `None` as "no binding" — and treating this module's errors as fail-closed
//! refusals rather than as a missing cache entry. Stated here rather than
//! implied, because a doc comment asserting a safety property the code does not
//! have is worse than none.
//!
//! The product rules it makes structural are cited where they bite:
//! `docs/decisions.md` R3 on [`Standing`] and [`Registry::observe`], R2 on
//! [`Registry::provision`], `docs/specs/history.md` 3.4 on [`Scope`] and
//! [`Registry::retire`], and `docs/spec.md` item 33 on [`Classification`].
//!
//! **Operator-only.** `docs/spec.md` D3 and `AGENTS.md` invariant 3: no
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

/// What oppen is doing with an account right now.
///
/// `docs/decisions.md` R3 describes a lifecycle rather than a pair of booleans.
/// The stored row carries three flags — `recorded`, `provisioned_by_oppen`,
/// `active` — and this is the only place that reads meaning out of their
/// combination. `provisioned_by_oppen` without `recorded` is unreachable through
/// this module ([`Registry::opt_out`] refuses it); a hand-edited database that
/// holds it anyway reads as [`Standing::Discovered`], which is R3's default and
/// the safe direction to fail in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

/// Which bucket an **account** belongs to. Never a fill's bucket.
///
/// `docs/spec.md` item 33 and `docs/specs/history.md` §2. An account with an
/// owner is attributable to that owner; anything else — the funding account,
/// an account made in the Hyperliquid web app, a discovered one the operator
/// merely watches — is `manual · external`.
///
/// §2 is explicit that "the account a fill arrived on does not name the agent"
/// and that attribution runs on `cloid` and ledger intent. The operator's own
/// ticket trades inside an agent's container, so two fills on one address can
/// belong to the agent and to the operator, and a reconciler that bucketed a
/// fill by handing its address to [`Registry::classify`] would file every one
/// of the operator's manual fills under the agent. This answers a question
/// about the address. It is the bucket for a fill the join did not attribute,
/// never a substitute for the join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

/// Which rows a listing returns.
///
/// Named rather than left to a boolean at each call site, because
/// `docs/specs/history.md` 3.4 puts a retired account in three answers with
/// different defaults: out of every live view, into every all-time total, and
/// still inside the reconcile set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Active accounts. What a roster, a position table or a live PnL asks for.
    Live,
    /// Accounts oppen records history for (R3 opt-in), **retired ones
    /// included**. What the backfill and the fill poller iterate.
    ///
    /// Retirement does not leave this set. `docs/specs/history.md` 3.4 is
    /// explicit that a retired container is excluded from live views "**not**
    /// from reconciliation: it keeps its `backfill_state` row, and a fill that
    /// arrives on it is ingested". No venue deletes a container, so a retired
    /// one still holds whatever was left in it and can still be filled — a
    /// resting order that crosses, a liquidation, an operator flattening it by
    /// hand. Dropping it here would stop fill capture on an account that is
    /// still trading, and that loss is permanent rather than a gap: nothing
    /// records that oppen stopped looking, so no backfill goes back for it.
    Recorded,
    /// Every row ever registered, retired included. What an all-time total and
    /// an audit export ask for.
    All,
}

impl Scope {
    fn admits(self, account: &Account) -> bool {
        match self {
            Scope::Live => account.standing != Standing::Retired,
            // The stored bit, not the standing: `Standing::Retired` displaces
            // the recorded bit rather than reporting it, and reading the
            // standing here is exactly how a retired container falls out of the
            // poll set.
            Scope::Recorded => account.recorded,
            Scope::All => true,
        }
    }
}

/// Where an order for an agent's container is routed on the wire.
///
/// `docs/hl-signing.md` §"Sub-account routing": a sub-account action carries
/// `vaultAddress`, a top-level account carries none. Both facts are here
/// because a caller needs both and neither may be derived from the other: the
/// container is which account the position, the margin and the fills belong to,
/// and [`Route::vault_address`] is the wire field. Collapsing the two — passing
/// the container as `vaultAddress`, or reading "no `vaultAddress`" as "no
/// container" — is each of the two ways D1's routing fails, and the venue
/// answers both with the same opaque rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The container the agent trades in. Never sent as `vaultAddress` unless
    /// [`Route::vault_address`] says so.
    pub container: Address,
    /// The `vaultAddress` field of the exchange request, ready for
    /// `oppen_hl::ExchangeRequest`. `None` omits the field, which signs the
    /// action as the container's own API wallet.
    pub vault_address: Option<Address>,
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
    /// Whether oppen records this account's history (R3 opt-in). Kept beside
    /// [`Account::standing`] for the same reason as
    /// [`Account::provisioned_by_oppen`]: retirement hides it.
    /// [`Standing::Retired`] is one variant of a lifecycle, so it displaces the
    /// recorded bit instead of reporting it, and [`Scope::Recorded`] needs the
    /// bit itself to keep polling a container after it is retired.
    pub recorded: bool,
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
            recorded: row.recorded,
            provisioned_by_oppen: row.provisioned_by_oppen,
            created_ts_ms: row.created_ts_ms,
        })
    }

    /// Where this account's orders route on the wire (`docs/spec.md` D1).
    ///
    /// **The container kind is a decision, not an inference.**
    /// `docs/decisions.md` V2 settles it for every container v1 can hold: on
    /// Hyperliquid, the only venue v1 ships, sub-accounts are gated behind
    /// $100,000 of traded volume — observed refusing a real `createSubAccount`
    /// — so oppen provisions a **top-level** account per agent and no
    /// sub-account container exists to route to. `vaultAddress` is therefore
    /// omitted, and the action is signed by the container's own API wallet.
    ///
    /// This used to read the kind off `provisioned_by_oppen`, which is a
    /// provenance bit and not a kind bit. The two are independent, so the guess
    /// was wrong in both directions and both are the failure D1 exists to
    /// prevent: a genuine sub-account container that oppen created — V1's shape
    /// on Aster and Lighter, and V5's on Hyperliquid — lost its `vaultAddress`
    /// and routed into the **master**, and a top-level container re-added by
    /// address through R7's recovery path gained one, naming an account its own
    /// API wallet does not master.
    ///
    /// The stored row carries no container kind:
    /// `docs/specs/venue-containers.md` §3.5 puts `container_kind` in a later
    /// migration, and V5's upgrade path is what needs it. When that column
    /// lands this reads it, and this is the only place that has to change.
    fn route(&self) -> Route {
        Route {
            container: self.address,
            vault_address: None,
        }
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
    /// Two **live** rows claim the same agent, so D1's 1:1 roster does not hold
    /// and there is no single right answer for `vaultAddress` — which is why the
    /// route lookup checks rather than taking the first row.
    ///
    /// Three ways in, all reproduced. Editing the database file. Calling
    /// `crate::ledger::Ledger::upsert_sub_account` directly, which is `pub` and
    /// takes a bare `String`. And two concurrent [`Registry::provision`] calls
    /// for one owner — this module's own public API — because the bound-owner
    /// check and the insert are separate ledger statements with nothing making
    /// the pair atomic. The third is closed by a partial unique index over the
    /// live rows, which is the move the ledger already makes for
    /// `events.idem_key` and `feed_gaps`, and not by a lock here: a
    /// [`Registry`] is a borrow with no state of its own to guard.
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
    /// The owner is already bound to another **live** account. D1 maps the
    /// roster 1:1, so a second live account for the same agent is a fork of its
    /// PnL. A retired account does not raise this: `docs/specs/history.md` 3.4
    /// makes a migration two rows rather than a rewritten one.
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
    /// attributed, keeps its `recorded` bit so [`Scope::Recorded`] goes on
    /// polling it, drops out of [`Scope::Live`] and stays in [`Scope::All`].
    ///
    /// **What this closes, and what it does not.** It closes every route this
    /// registry hands out: [`Registry::route_for_agent`] answers
    /// [`AccountsError::Retired`], and it is the only accessor here that yields
    /// a [`Route`]. It does **not** close the signing path, because the signing
    /// path does not read this table — `crate::guardrail`'s engine stamps each
    /// `Clearance` with a `vault_address` from its own `guardrail_vault` copy of
    /// the binding, written when the agent was registered, and retiring here
    /// leaves that copy untouched. So an agent retired precisely because the
    /// operator stopped trusting it can still be cleared and signed for. What
    /// does stop it today is the kill switch, which the engine checks on every
    /// decision; what would make this method stop it is the obligation in the
    /// module doc — the engine resolving the binding through
    /// [`Registry::route_for_agent`] per decision rather than caching it.
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

    /// Every account in `scope`, ascending by address.
    ///
    /// Ordered rather than left to the storage engine, so anything built from
    /// this — a roster view, an export, a hash — is byte-identical across runs
    /// (`AGENTS.md` invariant 6).
    pub fn list(&self, scope: Scope) -> Result<Vec<Account>, AccountsError> {
        let mut accounts = Vec::new();
        for row in self.ledger.sub_accounts()? {
            let account = Account::from_row(&row)?;
            if scope.admits(&account) {
                accounts.push(account);
            }
        }
        Ok(accounts)
    }

    /// Which bucket an address belongs to (`docs/spec.md` item 33). Read
    /// [`Classification`] before using this on a fill.
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
    /// [`Route::vault_address`] goes straight into `oppen_hl::ExchangeRequest`,
    /// following the agent's container kind rather than a call-site guess.
    ///
    /// **This is the owning store for the binding.** `docs/decisions.md` R7:
    /// "oppen's SQLite registry is the only place the agent → container →
    /// agent-wallet binding lives", because a top-level container is
    /// undiscoverable and nothing at the venue can rebuild the list. Any other
    /// copy is a cache, and a cache of this must be resolved from here at the
    /// moment it is used rather than filled once from its own caller —
    /// otherwise every refusal below is a refusal the signer never sees.
    /// `crate::guardrail`'s `guardrail_vault` is such a copy today and does not
    /// yet do this; the module doc states the obligation in full.
    ///
    /// Refuses rather than guessing in all three ways it can be uncertain: an
    /// unknown agent, a retired one, and an agent bound to two accounts. Each of
    /// them, resolved to a plausible answer, signs an order into an account that
    /// is not the agent's.
    pub fn route_for_agent(&self, agent: &AgentId) -> Result<Route, AccountsError> {
        let mut live: Option<Account> = None;
        let mut retired: Option<Address> = None;
        for row in self.ledger.sub_accounts()? {
            if !owned_by(&row, OwnerType::Agent, agent.as_str()) {
                continue;
            }
            let account = Account::from_row(&row)?;
            if account.standing == Standing::Retired {
                // A retired row is history, not a binding — see `bound_to`.
                // Rows arrive in address order, so this is the lowest one and
                // the refusal names the same account on every run.
                retired.get_or_insert(account.address);
                continue;
            }
            if let Some(first) = &live {
                return Err(AccountsError::AmbiguousAgent {
                    agent: agent.clone(),
                    first: first.address,
                    second: account.address,
                });
            }
            live = Some(account);
        }
        match (live, retired) {
            (Some(account), _) => Ok(account.route()),
            (None, Some(address)) => Err(AccountsError::Retired(address)),
            (None, None) => Err(AccountsError::UnknownAgent(agent.clone())),
        }
    }

    /// The **live** account an owner is already bound to, if any.
    ///
    /// Retired rows are skipped, because D1's 1:1 is a statement about the
    /// containers an owner trades in and a retired container is one it does
    /// not. `docs/specs/history.md` 3.4 makes that concrete: "a migrated agent
    /// has two rows, not a rewritten one — the old container retired, the new
    /// one active", and V5's "re-point the registry row" is "realised as an
    /// insert beside a retirement rather than an update". Counting the retired
    /// row would refuse that insert with
    /// [`AccountsError::OwnerAlreadyBound`], so the migration history.md
    /// mandates would be impossible through this API and re-pairing a retired
    /// agent would be impossible at all.
    ///
    /// This does not weaken the retirement refusal. Nothing revives a retired
    /// row — [`Registry::provision`] still answers [`AccountsError::Retired`]
    /// for the retired address itself, and [`Registry::route_for_agent`] still
    /// refuses an agent whose only container is retired.
    ///
    /// Scans the table rather than indexing it: D1's 1:1 mapping over a
    /// human-kept roster means tens of rows, so an index here would be a schema
    /// change for no measurable gain.
    fn bound_to(&self, owner: &Owner) -> Result<Option<Address>, AccountsError> {
        for row in self.ledger.sub_accounts()? {
            if row.active && owned_by(&row, owner.owner_type, &owner.owner_id) {
                return Ok(Some(Account::from_row(&row)?.address));
            }
        }
        Ok(None)
    }

    /// The stored row at `address`, or [`AccountsError::UnknownAccount`].
    ///
    /// **Keyed on the stored text, unlike the rest of this module.** This and
    /// [`Registry::classify`] look the row up by `Address::to_string()`, which
    /// is lowercase; [`Registry::list`], [`Registry::route_for_agent`] and
    /// `bound_to` scan and compare the *parsed* address. Every write
    /// here normalizes, so the two agree — until a row arrives through
    /// `crate::ledger::Ledger::upsert_sub_account`, which is `pub` and takes a
    /// bare `String`, in the checksummed form every explorer renders. That row
    /// then routes and lists normally, classifies as `manual · external`
    /// whatever its owner, cannot be retired at all
    /// ([`AccountsError::UnknownAccount`]), and accepts a
    /// [`Registry::provision`] that writes a *second* row for the same account
    /// — after which the agent is [`AccountsError::AmbiguousAgent`] between two
    /// copies of one address. The fix is normalization at the ledger boundary,
    /// where the text is written; doing it here would only move the split.
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

    /// The stored row at `address`, retired ones included.
    fn stored(registry: &Registry, address: Address) -> Option<Account> {
        registry
            .list(Scope::All)
            .expect("list")
            .into_iter()
            .find(|account| account.address == address)
    }

    /// D1: an agent's account is recorded from birth.
    #[test]
    fn a_provisioned_account_is_recorded_from_birth() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);

        let account = registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1_700_000)
            .expect("provision");
        assert_eq!(account.standing, Standing::ProvisionedByOppen);
        assert!(account.recorded);
        assert_eq!(
            registry.classify(address(AGENT_A)).expect("classify"),
            Classification::Agent
        );

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

    /// `docs/decisions.md` V2: every container v1 can hold on Hyperliquid is
    /// top-level, so **no** container sends a `vaultAddress` — and the route
    /// says so whatever the row's provenance.
    ///
    /// This pins the deleted inference. Reading the kind off
    /// `provisioned_by_oppen` is wrong in both directions and both misroute: a
    /// sub-account container oppen created loses its `vaultAddress` and the
    /// order lands in the **master**, and a top-level container oppen did not
    /// create — R7's recovery path re-adds one by address — gains a
    /// `vaultAddress` naming an account its own API wallet does not master.
    /// Both rows are built here, and both must route identically.
    #[test]
    fn no_v1_container_sends_a_vault_address_whatever_its_provenance() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);

        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");
        assert_eq!(
            registry
                .route_for_agent(&AgentId::new("carry"))
                .expect("route"),
            Route {
                container: address(AGENT_A),
                vault_address: None,
            }
        );

        // The same container re-added by address after losing the database
        // (R7), so `provisioned_by_oppen` is false for an account oppen made.
        ledger
            .upsert_sub_account(&SubAccount {
                address: AGENT_B.to_owned(),
                name: "basis".to_owned(),
                owner: Some(agent_owner("basis")),
                recorded: true,
                provisioned_by_oppen: false,
                active: true,
                created_ts_ms: 2,
            })
            .expect("upsert");
        assert_eq!(
            registry
                .route_for_agent(&AgentId::new("basis"))
                .expect("route"),
            Route {
                container: address(AGENT_B),
                vault_address: None,
            }
        );
    }

    /// `docs/specs/history.md` 3.4: "a migrated agent has two rows, not a
    /// rewritten one — the old container retired, the new one active", and V5's
    /// "re-point the registry row" is "realised as an insert beside a
    /// retirement rather than an update".
    ///
    /// A retired row is history, not a binding. Counting it as one refuses the
    /// insert that migration is made of and makes re-pairing a retired agent
    /// impossible, and if the row is forced in behind the registry the agent
    /// routes to [`AccountsError::AmbiguousAgent`] for ever — bricked by the
    /// very act the spec prescribes. What must survive is the refusal itself:
    /// an agent whose only container is retired still cannot be routed, and the
    /// retired address itself still cannot be revived.
    #[test]
    fn retirement_frees_the_agent_for_a_new_container() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);
        let agent = AgentId::new("carry");

        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");
        registry.retire(address(AGENT_A)).expect("retire");

        // With only the retired row, the agent has no route.
        assert!(matches!(
            registry.route_for_agent(&agent),
            Err(AccountsError::Retired(_))
        ));
        // And the retired address is never revived.
        assert!(matches!(
            registry.provision(address(AGENT_A), "carry", agent_owner("carry"), 2),
            Err(AccountsError::Retired(_))
        ));

        // The migration: a second row beside the retirement.
        registry
            .provision(address(AGENT_B), "carry", agent_owner("carry"), 2)
            .expect("the new container is accepted");
        assert_eq!(
            registry.route_for_agent(&agent).expect("route"),
            Route {
                container: address(AGENT_B),
                vault_address: None,
            }
        );
        // Both rows survive, so per-agent totals select over both (3.4).
        assert_eq!(registry.list(Scope::All).expect("all").len(), 2);
        assert_eq!(registry.list(Scope::Live).expect("live").len(), 1);

        // Two *live* rows are still ambiguous, and still refused.
        ledger
            .upsert_sub_account(&SubAccount {
                address: STRANGER.to_owned(),
                name: "smuggled".to_owned(),
                owner: Some(agent_owner("carry")),
                recorded: true,
                provisioned_by_oppen: false,
                active: true,
                created_ts_ms: 3,
            })
            .expect("upsert behind the registry");
        assert!(matches!(
            registry.route_for_agent(&agent),
            Err(AccountsError::AmbiguousAgent { .. })
        ));
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
            assert!(!account.recorded);
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

        let still = stored(&registry, address(OPERATOR)).expect("row");
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
        assert_eq!(
            registry.classify(address(AGENT_A)).expect("classify"),
            Classification::Agent
        );

        assert!(registry.list(Scope::Live).expect("live").is_empty());
        assert_eq!(registry.list(Scope::All).expect("all").len(), 1);
        // Still reconciled: 3.4 excludes a retired container from live views
        // and not from reconciliation. Pinned in full by
        // `retiring_a_recorded_container_does_not_stop_fill_capture`.
        assert_eq!(registry.list(Scope::Recorded).expect("recorded").len(), 1);

        // The registry's own route closes. This is the whole of what retiring
        // closes: the signing path reads the guardrail engine's copy of the
        // binding, not this table, so this refusal does not reach the signer
        // until that copy is resolved from here. See `Registry::retire`.
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
            stored(&registry, address(AGENT_A)).expect("row").standing,
            Standing::Retired
        );
        // Retiring twice is a no-op, not an error.
        assert_eq!(
            registry.retire(address(AGENT_A)).expect("retire").standing,
            Standing::Retired
        );
    }

    /// `docs/specs/history.md` 3.4: a retired container is excluded from live
    /// views, **not** from reconciliation.
    ///
    /// No venue deletes a container, so a retired one still holds a balance and
    /// can still be filled — a resting order that crosses, a liquidation, an
    /// operator flattening it by hand. Dropping it from the poll set loses those
    /// fills permanently rather than as a gap, because nothing records that
    /// oppen stopped looking. Reading [`Standing`] instead of the stored
    /// `recorded` bit is exactly how that happens: `Standing::of` tests
    /// `!active` first, so retirement hides the bit.
    #[test]
    fn retiring_a_recorded_container_does_not_stop_fill_capture() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);

        // One container oppen provisioned, one the operator opted in, and one
        // it merely discovered and never recorded.
        registry
            .provision(address(AGENT_A), "carry", agent_owner("carry"), 1)
            .expect("provision");
        registry
            .observe(
                &[discovered(AGENT_B, "basis"), discovered(OPERATOR, "manual")],
                1,
            )
            .expect("observe");
        registry.opt_in(address(AGENT_B)).expect("opt in");
        for target in [AGENT_A, AGENT_B, OPERATOR] {
            registry.retire(address(target)).expect("retire");
        }

        // All three leave the live roster.
        assert!(registry.list(Scope::Live).expect("live").is_empty());
        // The two oppen was recording stay in the poll set. The one it never
        // recorded has no history to keep flowing and does not join it.
        assert_eq!(
            registry
                .list(Scope::Recorded)
                .expect("recorded")
                .iter()
                .map(|account| account.address)
                .collect::<Vec<_>>(),
            vec![address(AGENT_A), address(AGENT_B)]
        );

        // The bit is readable even though the standing displaces it.
        let retired = stored(&registry, address(AGENT_A)).expect("row");
        assert_eq!(retired.standing, Standing::Retired);
        assert!(retired.recorded);
    }

    /// `docs/spec.md` item 33 and `docs/specs/history.md` §6: the account the
    /// operator trades by hand — the funding account, which under
    /// `docs/decisions.md` V7 is not an agent container — is representable,
    /// classifies as `manual · external`, and is in the poll set.
    ///
    /// The acceptance gate closes oppen for 24 hours while trades run in the
    /// Hyperliquid web app and then demands every one of those fills. That needs
    /// the address to be a recorded row with no owner, which is what this pins.
    /// It is not reached through [`Registry::provision`] — that binds an owner,
    /// which would attribute the operator's fills to an agent — but through
    /// [`Registry::observe`] plus [`Registry::opt_in`], R3's "add by address"
    /// mechanism.
    #[test]
    fn the_manually_traded_account_is_representable_and_polled() {
        let dir = TempDir::new().expect("tempdir");
        let ledger = ledger(&dir);
        let registry = Registry::new(&ledger);

        registry
            .observe(&[discovered(OPERATOR, "funding")], 1)
            .expect("observe");
        let account = registry.opt_in(address(OPERATOR)).expect("opt in");
        assert_eq!(account.standing, Standing::OptedIn);
        assert_eq!(account.owner, None);
        assert!(account.recorded);
        assert_eq!(
            registry.classify(address(OPERATOR)).expect("classify"),
            Classification::ManualExternal
        );
        assert_eq!(
            registry
                .list(Scope::Recorded)
                .expect("recorded")
                .iter()
                .map(|entry| entry.address)
                .collect::<Vec<_>>(),
            vec![address(OPERATOR)]
        );
        // It is nobody's agent container, so no agent can be routed into it.
        assert!(matches!(
            registry.route_for_agent(&AgentId::new("funding")),
            Err(AccountsError::UnknownAgent(_))
        ));
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
        registry
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
        assert_eq!(
            registry.classify(address(AGENT_B)).expect("classify"),
            Classification::Workflow
        );
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
        let account = stored(&registry, address(AGENT_A)).expect("row");
        assert_eq!(account.standing, Standing::Discovered);
        assert!(!account.recorded);
        assert!(account.provisioned_by_oppen);
    }
}
