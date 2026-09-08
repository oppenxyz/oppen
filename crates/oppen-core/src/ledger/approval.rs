//! ES25: authenticated, one-shot approval evidence in the existing event chain.

use std::collections::BTreeMap;
use std::sync::Arc;

use oppen_hl::order::OrderKind;
use oppen_hl::wire::{BuilderInfo, Cloid, Grouping, Tif, Tpsl};
use oppen_hl::{Action, Address, Network};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    Appended, AuthorizedRoute, Event, EventKind, Ledger, LedgerError, NewEvent, PolicyJournal,
};
use crate::guardrail::{
    APPROVAL_TTL_MS, AgentId, Cleared, ClearedKind, MAX_REASON_BYTES, OrderIntent, OriginalRequest,
    Proposal, Refusal,
};

type Result<T> = std::result::Result<T, ApprovalError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApprovalError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("approval authority unavailable: {detail}")]
    Unavailable { detail: String },
    #[error("approval conflict: {detail}")]
    Conflict { detail: String },
}
impl From<rusqlite::Error> for ApprovalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Ledger(error.into())
    }
}
impl From<serde_json::Error> for ApprovalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Ledger(error.into())
    }
}
fn unavailable(detail: impl ToString) -> ApprovalError {
    ApprovalError::Unavailable {
        detail: detail.to_string(),
    }
}
fn conflict(detail: impl ToString) -> ApprovalError {
    ApprovalError::Conflict {
        detail: detail.to_string(),
    }
}
fn timestamp(at_ms: u64) -> Result<i64> {
    i64::try_from(at_ms).map_err(|_| unavailable("approval timestamp out of range"))
}

pub(crate) struct Candidate {
    pub agent: AgentId,
    pub intent: OrderIntent,
    pub route: AuthorizedRoute,
    pub policy_revision: u64,
    pub at_ms: u64,
}

pub(crate) struct ApprovalJournal(Arc<PolicyJournal>);

pub(crate) struct ReviewEvidence {
    pub proposal: Proposal,
    pub policy_revision: u64,
    pub policy_hash: String,
    root: Link,
    authority: Arc<PolicyJournal>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewCommitment {
    proposal_root: Link,
    route: AuthorizedRoute,
    candidate: NormalizedIntent,
    action: Action,
    policy: Link,
    reviewed_at_ms: u64,
    reference_at_ms: u64,
    expires_at_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    reference_px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    sz: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    notional_usd: Decimal,
}

impl ReviewCommitment {
    pub(crate) fn new(
        evidence: &ReviewEvidence,
        candidate: &OrderIntent,
        action: Action,
        clearance: &crate::guardrail::Clearance,
        reviewed_at_ms: u64,
        reference_at_ms: u64,
    ) -> Result<Self> {
        let ClearedKind::Order {
            px,
            sz,
            notional_usd,
            reference_px,
            ..
        } = &clearance.kind
        else {
            return Err(unavailable("approval review requires an order"));
        };
        if clearance.policy_revision != evidence.policy_revision
            || clearance.route != evidence.proposal.route
        {
            return Err(conflict("review authority changed during evaluation"));
        }
        Ok(Self {
            proposal_root: evidence.root.clone(),
            route: evidence.proposal.route.clone(),
            candidate: NormalizedIntent::from_intent(candidate)?,
            action,
            policy: Link {
                seq: evidence.policy_revision,
                hash: evidence.policy_hash.clone(),
            },
            reviewed_at_ms,
            reference_at_ms,
            expires_at_ms: evidence.proposal.expires_at_ms,
            reference_px: *reference_px,
            px: *px,
            sz: *sz,
            notional_usd: *notional_usd,
        })
    }

    fn receipt_digest(&self, receipt: &Link) -> Result<String> {
        let canonical = super::hash::canonical_json(&serde_json::json!({
            "domain": "oppen.approval-review-receipt.v1", "review": self, "receipt": receipt,
        }))?;
        Ok(super::hash::payload_hash(canonical.as_bytes()))
    }

    pub(crate) fn digest(&self) -> Result<String> {
        let canonical = super::hash::canonical_json(&serde_json::json!({
            "domain": "oppen.approval-review-commitment.v1", "review": self,
        }))?;
        Ok(super::hash::payload_hash(canonical.as_bytes()))
    }

    fn matches_payload(&self, payload: &serde_json::Value) -> Result<bool> {
        let expected = serde_json::json!({
            "px": self.px, "sz": self.sz, "notional_usd": self.notional_usd,
        });
        Ok(
            payload.get("policy_revision") == Some(&self.policy.seq.into())
                && payload.get("approval_review_digest") == Some(&self.digest()?.into())
                && expected
                    .as_object()
                    .ok_or_else(|| unavailable("review economics missing"))?
                    .iter()
                    .all(|(key, value)| {
                        payload.get("kind").and_then(|kind| kind.get(key)) == Some(value)
                    }),
        )
    }
}

pub(crate) struct ClaimPermit {
    authority: Arc<PolicyJournal>,
    proposal: Proposal,
    root: Link,
    claimed: Link,
}
impl ClaimPermit {
    pub(crate) fn proposal(&self) -> &Proposal {
        &self.proposal
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    seq: u64,
    hash: String,
}
impl Link {
    fn event(event: &Event) -> Self {
        Self {
            seq: event.seq,
            hash: event.hash.clone(),
        }
    }
    fn appended(row: &Appended) -> Self {
        Self {
            seq: row.seq,
            hash: row.hash.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizedIntent {
    symbol: String,
    is_buy: bool,
    #[serde(with = "rust_decimal::serde::str")]
    px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    sz: Decimal,
    kind: NormalizedKind,
    reduce_only: bool,
    cloid: Cloid,
    grouping: Grouping,
    builder: Option<BuilderInfo>,
    #[serde(
        serialize_with = "rust_decimal::serde::str_option::serialize",
        deserialize_with = "optional_decimal"
    )]
    max_slippage_bps: Option<Decimal>,
    reason: String,
    // Absence is historical unknown provenance, not an inferred request kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original: Option<OriginalRequest>,
}

fn optional_decimal<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Decimal>, D::Error> {
    // Internally tagged operations buffer JSON null as unit. The standard
    // Option visitor accepts that representation without weakening string-only money.
    Option::<String>::deserialize(deserializer)?
        .map(|value| Decimal::from_str_exact(&value).map_err(serde::de::Error::custom))
        .transpose()
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum NormalizedKind {
    Limit {
        tif: Tif,
    },
    Trigger {
        is_market: bool,
        #[serde(with = "rust_decimal::serde::str")]
        trigger_px: Decimal,
        tpsl: Tpsl,
    },
}
impl NormalizedIntent {
    fn same_request(&self, other: &Self) -> bool {
        let mut comparable = other.clone();
        if let (Some(original), Some(other_original)) = (&self.original, &other.original)
            && original.kind == other_original.kind
        {
            // Quote evidence may advance on retry; retain the first signed observation.
            comparable.original = self.original.clone();
        }
        self == &comparable
    }

    fn from_intent(intent: &OrderIntent) -> Result<Self> {
        let value = Self {
            symbol: intent.symbol.clone(),
            is_buy: intent.is_buy,
            px: intent.px,
            sz: intent.sz,
            kind: match &intent.kind {
                OrderKind::Limit { tif } => NormalizedKind::Limit { tif: *tif },
                OrderKind::Trigger {
                    is_market,
                    trigger_px,
                    tpsl,
                } => NormalizedKind::Trigger {
                    is_market: *is_market,
                    trigger_px: *trigger_px,
                    tpsl: *tpsl,
                },
            },
            reduce_only: intent.reduce_only,
            cloid: intent
                .cloid
                .clone()
                .ok_or_else(|| unavailable("durable approval requires a cloid"))?,
            grouping: intent.grouping,
            builder: intent.builder.clone(),
            max_slippage_bps: intent.max_slippage_bps,
            reason: intent.reason.clone(),
            original: intent.original.clone(),
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        if self.symbol.is_empty()
            || self.symbol.chars().any(char::is_control)
            || self.px <= Decimal::ZERO
            || self.sz <= Decimal::ZERO
            || self.max_slippage_bps.is_some_and(|v| v < Decimal::ZERO)
            || self.reason.trim().is_empty()
            || self.reason.len() > MAX_REASON_BYTES
            || self
                .reason
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
            || matches!(self.kind, NormalizedKind::Trigger { trigger_px, .. } if trigger_px <= Decimal::ZERO)
            || self
                .original
                .as_ref()
                .is_some_and(|original| !original.matches_intent(&self.intent()))
        {
            return Err(unavailable("invalid normalized approval intent"));
        }
        Ok(())
    }
    fn intent(&self) -> OrderIntent {
        OrderIntent {
            symbol: self.symbol.clone(),
            is_buy: self.is_buy,
            px: self.px,
            sz: self.sz,
            kind: match self.kind {
                NormalizedKind::Limit { tif } => OrderKind::Limit { tif },
                NormalizedKind::Trigger {
                    is_market,
                    trigger_px,
                    tpsl,
                } => OrderKind::Trigger {
                    is_market,
                    trigger_px,
                    tpsl,
                },
            },
            reduce_only: self.reduce_only,
            cloid: Some(self.cloid.clone()),
            grouping: self.grouping,
            builder: self.builder.clone(),
            max_slippage_bps: self.max_slippage_bps,
            reason: self.reason.clone(),
            original: self.original.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposed {
    agent: AgentId,
    intent: NormalizedIntent,
    route: AuthorizedRoute,
    route_hash: String,
    policy: Link,
    expires_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
enum Disposition {
    Approved {
        intent: Link,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review_receipt: Option<String>,
    },
    Refused {
        detail: String,
        audit: Link,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review_receipt: Option<String>,
    },
    Rejected,
    Expired,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Proposed {
        proposal: Box<Proposed>,
    },
    Claimed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        review: Option<Box<ReviewCommitment>>,
    },
    Disposed {
        outcome: Disposition,
    },
}
impl Operation {
    fn kind(&self) -> EventKind {
        match self {
            Self::Proposed { .. } => EventKind::ApprovalProposed,
            Self::Claimed { .. } => EventKind::ApprovalClaimed,
            Self::Disposed { .. } => EventKind::ApprovalDisposed,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u64,
    network: Network,
    seq: u64,
    prev_hash: String,
    at_ms: u64,
    root: Option<Link>,
    previous: Option<Link>,
    operation: Operation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    envelope: Envelope,
    mac: String,
}

enum State {
    Pending,
    Claimed,
    Disposed,
}
struct Entry {
    data: Proposed,
    root: Link,
    last: Link,
    minted_at_ms: u64,
    last_at_ms: u64,
    state: State,
    review: Option<Box<ReviewCommitment>>,
}
impl Entry {
    fn proposal(&self, network: Network) -> Proposal {
        Proposal {
            id: format!("approval-{}-{}", super::network_key(network), self.root.seq),
            agent: self.data.agent.clone(),
            intent: self.data.intent.intent(),
            route: self.data.route.clone(),
            expires_at_ms: self.data.expires_at_ms,
        }
    }
}

fn message(envelope: &Envelope) -> Result<String> {
    Ok(super::hash::canonical_json(
        &serde_json::json!({"domain":"oppen.approval-authority.v1", "envelope":envelope}),
    )?)
}
fn row(connection: &Connection, seq: u64) -> Result<Event> {
    let mut statement = connection.prepare(&format!(
        "SELECT {} FROM events WHERE seq = ?1",
        super::SELECT_EVENT_COLUMNS
    ))?;
    let mut rows = statement.query(params![timestamp(seq)?])?;
    super::event_from_row(
        rows.next()?
            .ok_or_else(|| unavailable("linked approval evidence missing"))?,
    )
    .map_err(Into::into)
}

impl ApprovalJournal {
    pub(crate) fn new(authority: Arc<PolicyJournal>) -> Self {
        Self(authority)
    }
    fn ledger(&self) -> &Ledger {
        self.0.registry().ledger()
    }

    pub(crate) fn prepare(&self, id: &str, at_ms: u64) -> Result<Option<ReviewEvidence>> {
        timestamp(at_ms)?;
        let mut guard = self.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let entries = self.replay(&tx)?;
        let current = self.0.current_in(&tx).map_err(unavailable)?;
        let evidence = entries
            .values()
            .find(|entry| entry.proposal(self.ledger().network).id() == id)
            .map(|entry| {
                if at_ms < entry.last_at_ms {
                    return Err(unavailable("approval clock moved backwards"));
                }
                if !matches!(entry.state, State::Pending) || at_ms >= entry.data.expires_at_ms {
                    return Ok(None);
                }
                Ok(Some(ReviewEvidence {
                    proposal: entry.proposal(self.ledger().network),
                    root: entry.root.clone(),
                    policy_revision: current.revision,
                    policy_hash: row(&tx, current.revision)?.hash,
                    authority: self.0.clone(),
                }))
            })
            .transpose()?
            .flatten();
        tx.commit()?;
        Ok(evidence)
    }

    fn validate_review(
        &self,
        connection: &Connection,
        entry: &Entry,
        review: &ReviewCommitment,
        at_ms: u64,
        claim_seq: u64,
    ) -> Result<()> {
        review.candidate.validate()?;
        if review.proposal_root != entry.root
            || review.route != entry.data.route
            || review.reviewed_at_ms < entry.minted_at_ms
            || review.reviewed_at_ms > at_ms
            || review.reference_at_ms > review.reviewed_at_ms
            || review.expires_at_ms != entry.data.expires_at_ms
            || at_ms >= review.expires_at_ms
            || review.reference_px <= Decimal::ZERO
            || review.px <= Decimal::ZERO
            || review.sz <= Decimal::ZERO
            || review.notional_usd <= Decimal::ZERO
            || review.policy.seq >= claim_seq
        {
            return Err(unavailable(
                "invalid reviewed lifetime, economics or policy ordering",
            ));
        }
        let policy = row(connection, review.policy.seq)?;
        if policy.hash != review.policy.hash
            || !matches!(
                policy.kind,
                EventKind::PolicyInitialized | EventKind::PolicyReplaced
            )
        {
            return Err(unavailable("review policy linkage mismatch"));
        }
        let mut expected = entry.data.intent.clone();
        if let Some(original) = &mut expected.original
            && matches!(
                original.kind,
                crate::guardrail::RequestedOrderKind::Market { .. }
                    | crate::guardrail::RequestedOrderKind::ClosePosition { .. }
            )
        {
            expected.px = review.candidate.px;
            original.reference_px = Some(review.reference_px);
            original.reference_at_ms = review.reference_at_ms;
        }
        if expected != review.candidate {
            return Err(conflict("review changed fixed proposal fields"));
        }
        let Action::Order {
            orders,
            grouping,
            builder,
        } = &review.action
        else {
            return Err(unavailable("review action is not an order"));
        };
        let [order] = orders.as_slice() else {
            return Err(unavailable("review requires one order"));
        };
        let same_kind = match (&order.t, &review.candidate.kind) {
            (oppen_hl::wire::OrderType::Limit { tif }, NormalizedKind::Limit { tif: expected }) => {
                tif == expected
            }
            (
                oppen_hl::wire::OrderType::Trigger {
                    is_market, tpsl, ..
                },
                NormalizedKind::Trigger {
                    is_market: expected_market,
                    tpsl: expected_tpsl,
                    ..
                },
            ) => is_market == expected_market && tpsl == expected_tpsl,
            _ => false,
        };
        if *grouping != review.candidate.grouping
            || !same_kind
            || review.px.checked_mul(review.sz) != Some(review.notional_usd)
            || *builder != review.candidate.builder
            || order.b != review.candidate.is_buy
            || order.r != review.candidate.reduce_only
            || order.c.as_ref() != Some(&review.candidate.cloid)
            || order.p.as_str().parse::<Decimal>().map_err(unavailable)? != review.px
            || order.s.as_str().parse::<Decimal>().map_err(unavailable)? != review.sz
        {
            return Err(unavailable("review action does not match candidate"));
        }
        Ok(())
    }

    fn validate_review_receipt(
        entry: &Entry,
        receipt: &Link,
        digest: &Option<String>,
    ) -> Result<()> {
        let expected = entry
            .review
            .as_ref()
            .map(|review| review.receipt_digest(receipt))
            .transpose()?;
        if &expected != digest {
            return Err(unavailable("review commitment/receipt digest mismatch"));
        }
        Ok(())
    }

    fn replay(&self, connection: &Connection) -> Result<BTreeMap<u64, Entry>> {
        // This verifies the anchored chain and all policy MACs even when the queue is empty.
        self.0.current_in(connection).map_err(unavailable)?;
        let mut entries: BTreeMap<u64, Entry> = BTreeMap::new();
        let mut statement = connection.prepare(&format!("SELECT {}, idem_key FROM events WHERE kind IN ('approval_proposed','approval_claimed','approval_disposed') ORDER BY seq", super::SELECT_EVENT_COLUMNS))?;
        let mut rows = statement.query([])?;
        while let Some(record) = rows.next()? {
            let event = super::event_from_row(record)?;
            let raw: Option<String> = record.get(4)?;
            let raw = raw.ok_or_else(|| unavailable("required approval evidence redacted"))?;
            let signed = self.decode(&event, &raw)?;
            let e = signed.envelope;
            let key: Option<String> = record.get(12)?;
            match e.operation {
                Operation::Proposed { proposal } => {
                    if e.root.is_some() || e.previous.is_some() {
                        return Err(unavailable("proposal cannot have a predecessor"));
                    }
                    self.validate_proposed(connection, &proposal, e.at_ms, event.seq)?;
                    if key.as_deref() != Some(&mint_key(&proposal))
                        || entries.values().any(|entry| {
                            entry.data.route.binding.container == proposal.route.binding.container
                                && entry.data.intent.cloid == proposal.intent.cloid
                        })
                    {
                        return Err(unavailable("approval cloid reused or mint key mismatched"));
                    }
                    let link = Link::event(&event);
                    entries.insert(
                        event.seq,
                        Entry {
                            data: *proposal,
                            root: link.clone(),
                            last: link,
                            minted_at_ms: e.at_ms,
                            last_at_ms: e.at_ms,
                            state: State::Pending,
                            review: None,
                        },
                    );
                }
                operation => {
                    let root = e
                        .root
                        .ok_or_else(|| unavailable("approval root link missing"))?;
                    let entry = entries
                        .get_mut(&root.seq)
                        .ok_or_else(|| unavailable("approval root missing or out of order"))?;
                    if root != entry.root
                        || e.previous.as_ref() != Some(&entry.last)
                        || e.at_ms < entry.last_at_ms
                    {
                        return Err(unavailable("approval predecessor, root or time mismatch"));
                    }
                    let expected_key = step_key(operation.kind(), root.seq);
                    if key.as_deref() != Some(&expected_key) {
                        return Err(unavailable("approval transition key mismatch"));
                    }
                    match operation {
                        Operation::Claimed { review }
                            if matches!(entry.state, State::Pending)
                                && e.at_ms < entry.data.expires_at_ms =>
                        {
                            if let Some(review) = &review {
                                self.validate_review(
                                    connection, entry, review, e.at_ms, event.seq,
                                )?;
                            }
                            entry.review = review;
                            entry.state = State::Claimed
                        }
                        Operation::Disposed { outcome } => {
                            match (&entry.state, &outcome) {
                                (State::Pending, Disposition::Rejected)
                                    if e.at_ms < entry.data.expires_at_ms => {}
                                (State::Pending, Disposition::Expired)
                                    if e.at_ms >= entry.data.expires_at_ms => {}
                                (
                                    State::Claimed,
                                    Disposition::Refused {
                                        detail,
                                        audit,
                                        review_receipt,
                                    },
                                ) if !detail.is_empty() => {
                                    self.validate_refusal_row(
                                        connection, entry, audit, detail, event.seq,
                                    )?;
                                    Self::validate_review_receipt(entry, audit, review_receipt)?;
                                }
                                (
                                    State::Claimed,
                                    Disposition::Approved {
                                        intent,
                                        review_receipt,
                                    },
                                ) => {
                                    self.validate_intent_row(connection, entry, intent, event.seq)?;
                                    Self::validate_review_receipt(entry, intent, review_receipt)?;
                                }
                                _ => {
                                    return Err(unavailable(
                                        "invalid approval disposition transition",
                                    ));
                                }
                            }
                            entry.state = State::Disposed;
                        }
                        _ => {
                            return Err(unavailable(
                                "approval claimed twice, expired, or already disposed",
                            ));
                        }
                    }
                    entry.last = Link::event(&event);
                    entry.last_at_ms = e.at_ms;
                }
            }
        }
        Ok(entries)
    }

    fn decode(&self, event: &Event, raw: &str) -> Result<Signed> {
        let payload = event
            .payload
            .as_ref()
            .ok_or_else(|| unavailable("approval payload missing"))?;
        if super::hash::canonical_json(payload)? != raw {
            return Err(unavailable("noncanonical or duplicate approval fields"));
        }
        let signed: Signed = serde_json::from_value(payload.clone())
            .map_err(|error| unavailable(format!("malformed approval record: {error}")))?;
        if serde_json::to_value(&signed)? != *payload {
            return Err(unavailable(
                "missing, unknown or noncanonical approval fields",
            ));
        }
        if signed.mac.len() != 64
            || !signed
                .mac
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(unavailable("invalid approval MAC encoding"));
        }
        let mac = hex::decode(&signed.mac).map_err(unavailable)?;
        if !self
            .0
            .registry()
            .authority_key()
            .verify(message(&signed.envelope)?.as_bytes(), &mac)
        {
            return Err(unavailable("approval MAC mismatch"));
        }
        let e = &signed.envelope;
        if e.version != 1
            || e.network != self.ledger().network
            || e.seq != event.seq
            || e.prev_hash != event.prev_hash
            || timestamp(e.at_ms)? != event.ts_ms
            || e.operation.kind() != event.kind
            || event.agent_id.is_some()
            || event.snapshot_id.is_some()
            || event.snapshot_hash.is_some()
        {
            return Err(unavailable("approval envelope does not match its row"));
        }
        Ok(signed)
    }

    fn validate_proposed(
        &self,
        connection: &Connection,
        data: &Proposed,
        at_ms: u64,
        seq: u64,
    ) -> Result<()> {
        crate::keys::checked_agent_id(&data.agent).map_err(unavailable)?;
        data.intent.validate()?;
        if data.agent != data.route.binding.agent
            || data.route.network != self.ledger().network
            || data.route.binding.container == Address::ZERO
            || data.expires_at_ms
                != at_ms
                    .checked_add(APPROVAL_TTL_MS)
                    .ok_or_else(|| unavailable("approval TTL overflow"))?
            || data.route.binding_seq >= seq
            || data.policy.seq >= seq
        {
            return Err(unavailable(
                "invalid proposal identity, TTL or authority ordering",
            ));
        }
        timestamp(data.expires_at_ms)?;
        // Replay registry authority, including retired grants, without retargeting the proposal.
        self.0
            .registry()
            .optional_route_in(connection, &data.agent)
            .map_err(unavailable)?;
        let grant = row(connection, data.route.binding_seq)?;
        if grant.kind != EventKind::RegistryGranted
            || grant.hash != data.route_hash
            || grant
                .payload
                .as_ref()
                .and_then(|v| v.pointer("/envelope/authority/binding"))
                != Some(&serde_json::to_value(&data.route.binding)?)
        {
            return Err(unavailable("proposal registry linkage mismatch"));
        }
        let policy = row(connection, data.policy.seq)?;
        if !matches!(
            policy.kind,
            EventKind::PolicyInitialized | EventKind::PolicyReplaced
        ) || policy.hash != data.policy.hash
            || policy.payload.is_none()
        {
            return Err(unavailable("proposal policy linkage mismatch"));
        }
        Ok(())
    }

    fn validate_intent_row(
        &self,
        connection: &Connection,
        entry: &Entry,
        link: &Link,
        disposition_seq: u64,
    ) -> Result<Event> {
        let event = row(connection, link.seq)?;
        if event.hash != link.hash
            || event.kind != EventKind::OrderIntent
            || link.seq <= entry.last.seq
            || link.seq >= disposition_seq
            || event.agent_id.as_deref() != Some(entry.data.agent.as_str())
        {
            return Err(unavailable("approved intent linkage mismatch"));
        }
        let payload = event
            .payload
            .as_ref()
            .ok_or_else(|| unavailable("approved intent redacted"))?;
        let kind = payload
            .get("kind")
            .ok_or_else(|| unavailable("approved intent kind missing"))?;
        if kind.get("cleared").and_then(|v| v.as_str()) != Some("order")
            || payload.get("agent") != Some(&serde_json::to_value(&entry.data.agent)?)
            || payload.get("route") != Some(&serde_json::to_value(&entry.data.route)?)
            || payload.get("network") != Some(&serde_json::to_value(self.ledger().network)?)
            || payload.get("reason").and_then(|v| v.as_str())
                != Some(entry.data.intent.reason.as_str())
            || kind.get("cloid") != Some(&serde_json::to_value(&entry.data.intent.cloid)?)
            || kind.get("symbol").and_then(|v| v.as_str())
                != Some(entry.data.intent.symbol.as_str())
            || kind.get("is_buy").and_then(|v| v.as_bool()) != Some(entry.data.intent.is_buy)
            || kind.get("reduce_only").and_then(|v| v.as_bool())
                != Some(entry.data.intent.reduce_only)
        {
            return Err(unavailable("approved intent does not match proposal"));
        }
        if let Some(review) = &entry.review
            && !review.matches_payload(payload)?
        {
            return Err(unavailable(
                "approved receipt differs from reviewed commitment, economics or policy",
            ));
        }
        Ok(event)
    }

    fn publish(&self, tx: Transaction<'_>) -> Result<()> {
        let (seq, hash) = super::head(&tx)?;
        tx.commit()?;
        self.ledger().note_head(&Appended { seq, hash })?;
        Ok(())
    }

    fn validate_refusal_row(
        &self,
        connection: &Connection,
        entry: &Entry,
        link: &Link,
        detail: &str,
        disposition_seq: u64,
    ) -> Result<Event> {
        let event = row(connection, link.seq)?;
        if event.hash != link.hash
            || event.kind != EventKind::Refusal
            || link.seq <= entry.last.seq
            || link.seq >= disposition_seq
            || event.agent_id.as_deref() != Some(entry.data.agent.as_str())
        {
            return Err(unavailable("refused approval audit linkage mismatch"));
        }
        let payload = event
            .payload
            .as_ref()
            .ok_or_else(|| unavailable("refused approval audit redacted"))?;
        if payload.get("refusal").and_then(|v| v.as_str()) != Some(detail)
            || payload.get("reason").and_then(|v| v.as_str())
                != Some(entry.data.intent.reason.as_str())
            || payload.get("refusal_detail").is_none()
        {
            return Err(unavailable(
                "refused approval audit does not match proposal",
            ));
        }
        Ok(event)
    }
    fn append(
        &self,
        tx: Transaction<'_>,
        entry: Option<&Entry>,
        operation: Operation,
        at_ms: u64,
    ) -> Result<Appended> {
        let (head, prev_hash) = super::head(&tx)?;
        // Replay validated this head. Repair its publication before adding a row,
        // so a second publication failure cannot exceed the one-row crash window.
        self.ledger().note_head(&Appended {
            seq: head,
            hash: prev_hash.clone(),
        })?;
        let seq = head.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let kind = operation.kind();
        let key = match &operation {
            Operation::Proposed { proposal } => mint_key(proposal),
            _ => step_key(
                kind,
                entry
                    .ok_or_else(|| unavailable("approval transition without root"))?
                    .root
                    .seq,
            ),
        };
        let envelope = Envelope {
            version: 1,
            network: self.ledger().network,
            seq,
            prev_hash,
            at_ms,
            root: entry.map(|e| e.root.clone()),
            previous: entry.map(|e| e.last.clone()),
            operation,
        };
        let mac = self
            .0
            .registry()
            .authority_key()
            .sign(message(&envelope)?.as_bytes())
            .map_err(|_| unavailable("approval signing failed"))?;
        let payload = serde_json::to_value(Signed {
            envelope,
            mac: hex::encode(mac),
        })?;
        let appended = super::append_keyed_in_tx(
            &tx,
            &NewEvent {
                kind,
                ts_ms: timestamp(at_ms)?,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &key,
        )?
        .ok_or_else(|| unavailable("approval key without verified transition"))?;
        tx.commit()?;
        self.ledger().note_head(&appended)?;
        Ok(appended)
    }

    pub(crate) fn mint(&self, candidate: Candidate) -> Result<Proposal> {
        timestamp(candidate.at_ms)?;
        let intent = NormalizedIntent::from_intent(&candidate.intent)?;
        let mut guard = self.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let entries = self.replay(&tx)?;
        let policy = self.0.current_in(&tx).map_err(unavailable)?;
        let route = self
            .0
            .registry()
            .route_in(&tx, &candidate.agent)
            .map_err(unavailable)?;
        if route != candidate.route
            || policy.revision != candidate.policy_revision
            || candidate.agent != route.binding.agent
        {
            return Err(conflict("proposal route or policy changed"));
        }
        if let Some(existing) = entries.values().find(|e| {
            e.data.route.binding.container == route.binding.container
                && e.data.intent.cloid == intent.cloid
        }) {
            if !existing.data.intent.same_request(&intent)
                || existing.data.agent != candidate.agent
                || existing.data.route != route
                || existing.data.policy.seq != candidate.policy_revision
            {
                return Err(conflict("cloid already names another proposal"));
            }
            if candidate.at_ms < existing.minted_at_ms {
                return Err(unavailable("approval clock moved backwards"));
            }
            let pending = matches!(existing.state, State::Pending)
                && candidate.at_ms < existing.data.expires_at_ms;
            let proposal = existing.proposal(self.ledger().network);
            self.publish(tx)?;
            return if pending {
                Ok(proposal)
            } else {
                Err(conflict("proposal already consumed or expired"))
            };
        }
        let expires_at_ms = candidate
            .at_ms
            .checked_add(APPROVAL_TTL_MS)
            .ok_or_else(|| unavailable("approval TTL overflow"))?;
        let data = Proposed {
            agent: candidate.agent,
            intent,
            route_hash: row(&tx, route.binding_seq)?.hash,
            route,
            policy: Link {
                seq: policy.revision,
                hash: row(&tx, policy.revision)?.hash,
            },
            expires_at_ms,
        };
        let (head, _) = super::head(&tx)?;
        self.validate_proposed(
            &tx,
            &data,
            candidate.at_ms,
            head.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?,
        )?;
        let stored = data.clone();
        let appended = self.append(
            tx,
            None,
            Operation::Proposed {
                proposal: Box::new(data),
            },
            candidate.at_ms,
        )?;
        Ok(Entry {
            data: stored,
            root: Link::appended(&appended),
            last: Link::appended(&appended),
            minted_at_ms: candidate.at_ms,
            last_at_ms: candidate.at_ms,
            state: State::Pending,
            review: None,
        }
        .proposal(self.ledger().network))
    }

    pub(crate) fn pending(&self, now_ms: u64) -> Result<Vec<Proposal>> {
        timestamp(now_ms)?;
        loop {
            let mut guard = self.ledger().lock()?;
            let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let entries = self.replay(&tx)?;
            if entries.values().any(|entry| now_ms < entry.minted_at_ms) {
                return Err(unavailable("approval clock moved backwards"));
            }
            if let Some(entry) = entries.values().find(|entry| {
                matches!(entry.state, State::Pending) && now_ms >= entry.data.expires_at_ms
            }) {
                self.append(
                    tx,
                    Some(entry),
                    Operation::Disposed {
                        outcome: Disposition::Expired,
                    },
                    now_ms,
                )?;
                continue;
            }
            let pending = entries
                .values()
                .filter(|e| matches!(e.state, State::Pending))
                .map(|e| e.proposal(self.ledger().network))
                .collect();
            self.publish(tx)?;
            return Ok(pending);
        }
    }

    pub(crate) fn claim(&self, id: &str, at_ms: u64) -> Result<Option<ClaimPermit>> {
        self.claim_inner(id, at_ms, None)
    }

    pub(crate) fn claim_review(
        &self,
        evidence: ReviewEvidence,
        review: ReviewCommitment,
        at_ms: u64,
    ) -> Result<Option<ClaimPermit>> {
        if !Arc::ptr_eq(&evidence.authority, &self.0) {
            return Err(conflict("review belongs to another authority owner"));
        }
        self.claim_inner(evidence.proposal.id(), at_ms, Some((&evidence, review)))
    }

    fn claim_inner(
        &self,
        id: &str,
        at_ms: u64,
        reviewed: Option<(&ReviewEvidence, ReviewCommitment)>,
    ) -> Result<Option<ClaimPermit>> {
        timestamp(at_ms)?;
        let mut guard = self.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let entries = self.replay(&tx)?;
        let entry = entries
            .values()
            .find(|e| e.proposal(self.ledger().network).id() == id);
        let Some(entry) = entry else {
            self.publish(tx)?;
            return Ok(None);
        };
        if at_ms < entry.last_at_ms {
            return Err(unavailable("approval clock moved backwards"));
        }
        if !matches!(entry.state, State::Pending) {
            self.publish(tx)?;
            return Ok(None);
        }
        if at_ms >= entry.data.expires_at_ms {
            self.append(
                tx,
                Some(entry),
                Operation::Disposed {
                    outcome: Disposition::Expired,
                },
                at_ms,
            )?;
            return Ok(None);
        }
        let review = if let Some((evidence, review)) = reviewed {
            let current = self.0.current_in(&tx).map_err(unavailable)?;
            if entry.root != evidence.root
                || entry.proposal(self.ledger().network) != evidence.proposal
                || current.revision != evidence.policy_revision
                || row(&tx, current.revision)?.hash != evidence.policy_hash
            {
                return Err(conflict("review proposal or policy changed"));
            }
            let (head, _) = super::head(&tx)?;
            self.validate_review(
                &tx,
                entry,
                &review,
                at_ms,
                head.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?,
            )?;
            Some(Box::new(review))
        } else {
            None
        };
        let appended = self.append(tx, Some(entry), Operation::Claimed { review }, at_ms)?;
        Ok(Some(ClaimPermit {
            authority: self.0.clone(),
            proposal: entry.proposal(self.ledger().network),
            root: entry.root.clone(),
            claimed: Link::appended(&appended),
        }))
    }

    pub(crate) fn reject(&self, id: &str, at_ms: u64) -> Result<bool> {
        timestamp(at_ms)?;
        let mut guard = self.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let entries = self.replay(&tx)?;
        let entry = entries
            .values()
            .find(|e| e.proposal(self.ledger().network).id() == id);
        let Some(entry) = entry else {
            self.publish(tx)?;
            return Ok(false);
        };
        if at_ms < entry.last_at_ms {
            return Err(unavailable("approval clock moved backwards"));
        }
        if !matches!(entry.state, State::Pending) {
            self.publish(tx)?;
            return Ok(false);
        }
        let expired = at_ms >= entry.data.expires_at_ms;
        self.append(
            tx,
            Some(entry),
            Operation::Disposed {
                outcome: if expired {
                    Disposition::Expired
                } else {
                    Disposition::Rejected
                },
            },
            at_ms,
        )?;
        Ok(!expired)
    }

    pub(crate) fn finish(
        &self,
        claim: ClaimPermit,
        outcome: &std::result::Result<Cleared, Refusal>,
        receipt: Option<&Appended>,
        at_ms: u64,
    ) -> Result<()> {
        timestamp(at_ms)?;
        if !Arc::ptr_eq(&claim.authority, &self.0) {
            return Err(conflict("claim belongs to another authority owner"));
        }
        let mut guard = self.ledger().lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let entries = self.replay(&tx)?;
        let entry = entries
            .get(&claim.root.seq)
            .ok_or_else(|| unavailable("claim root missing"))?;
        if entry.root != claim.root
            || entry.last != claim.claimed
            || !matches!(entry.state, State::Claimed)
            || at_ms < entry.last_at_ms
        {
            return Err(conflict("claim is not the current one-shot transition"));
        }
        let disposition = match outcome {
            Ok(cleared) => {
                let clearance = cleared.clearance();
                if !matches!(clearance.kind, ClearedKind::Order { .. })
                    || clearance.route != entry.data.route
                    || clearance.agent != entry.data.agent
                {
                    return Err(conflict("approval clearance route or action differs"));
                }
                if let Some(review) = &entry.review
                    && !cleared.matches_reviewed_action(&review.action)
                {
                    return Err(conflict("clearance action differs from reviewed action"));
                }
                let receipt = receipt.ok_or_else(|| {
                    unavailable("approved outcome requires durable intent receipt")
                })?;
                let link = Link::appended(receipt);
                let (head, _) = super::head(&tx)?;
                let event = self.validate_intent_row(
                    &tx,
                    entry,
                    &link,
                    head.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?,
                )?;
                let mut expected = serde_json::to_value(clearance)?;
                expected
                    .as_object_mut()
                    .ok_or_else(|| unavailable("clearance is not an object"))?
                    .insert("reason".into(), entry.data.intent.reason.clone().into());
                if event.payload.as_ref() != Some(&expected) {
                    return Err(conflict(
                        "intent receipt payload differs from approved clearance",
                    ));
                }
                let review_receipt = entry
                    .review
                    .as_ref()
                    .map(|review| review.receipt_digest(&link))
                    .transpose()?;
                Disposition::Approved {
                    intent: link,
                    review_receipt,
                }
            }
            Err(refusal) => {
                let receipt = receipt
                    .ok_or_else(|| unavailable("refused outcome requires durable audit receipt"))?;
                let audit = Link::appended(receipt);
                let detail = refusal.to_string();
                let (head, _) = super::head(&tx)?;
                let event = self.validate_refusal_row(
                    &tx,
                    entry,
                    &audit,
                    &detail,
                    head.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?,
                )?;
                let expected = serde_json::json!({"refusal": detail, "refusal_detail": refusal, "reason": entry.data.intent.reason});
                if event.payload.as_ref() != Some(&expected) {
                    return Err(conflict("refusal receipt differs from actual outcome"));
                }
                let review_receipt = entry
                    .review
                    .as_ref()
                    .map(|review| review.receipt_digest(&audit))
                    .transpose()?;
                Disposition::Refused {
                    detail,
                    audit,
                    review_receipt,
                }
            }
        };
        self.append(
            tx,
            Some(entry),
            Operation::Disposed {
                outcome: disposition,
            },
            at_ms,
        )?;
        Ok(())
    }
}

fn mint_key(data: &Proposed) -> String {
    format!(
        "approval:{}:{}",
        data.route.binding.container,
        data.intent.cloid.as_str()
    )
}
fn step_key(kind: EventKind, seq: u64) -> String {
    format!("{}:{seq}", kind.as_str())
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;
