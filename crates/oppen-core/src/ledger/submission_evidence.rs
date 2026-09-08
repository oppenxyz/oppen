//! Authenticated provenance in the existing submission journal. A signed row
//! alone is never evidence of dispatch, acceptance, or current resting state.

use super::*;
use crate::guardrail::{GuardedSignature, SubmissionPostError};
use crate::ledger::{Appended, AuthorizedRoute, RegistryJournal};
use oppen_hl::exchange::{ExchangeResponse, ExchangeResponseKind, Status};
use oppen_hl::{Action, ExchangeRequest, Network};
use rust_decimal::Decimal;
use sha3::{Digest, Keccak256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    seq: u64,
    hash: String,
}
impl From<&Appended> for Link {
    fn from(row: &Appended) -> Self {
        Self {
            seq: row.seq,
            hash: row.hash.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedRequest {
    start: Link,
    route: AuthorizedRoute,
    cloid: Cloid,
    action: Action,
    clearance: Value,
    signer: Address,
    signed_at_ms: u64,
    request_digest: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum AcceptedStatus {
    Resting,
    Filled {
        #[serde(with = "rust_decimal::serde::str")]
        total_sz: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        avg_px: Decimal,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Signed {
        request: Box<SignedRequest>,
    },
    Accepted {
        signed: Link,
        order_index: u32,
        oid: u64,
        status: AcceptedStatus,
    },
}
impl Operation {
    fn kind(&self) -> EventKind {
        match self {
            Self::Signed { .. } => EventKind::SubmissionSigned,
            Self::Accepted { .. } => EventKind::SubmissionAccepted,
        }
    }
    fn key(&self) -> String {
        match self {
            Self::Signed { request } => format!("submission_signed:{}", request.start.seq),
            Self::Accepted { signed, .. } => format!("submission_accepted:{}", signed.seq),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u64,
    network: Network,
    genesis: String,
    seq: u64,
    prev_hash: String,
    at_ms: u64,
    operation: Operation,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authenticated {
    envelope: Envelope,
    mac: String,
}

impl Authenticated {
    fn verify(&self, raw: &str, key: &crate::keys::HmacKey) -> Result<()> {
        if crate::ledger::hash::canonical_json(&serde_json::to_value(self)?)? != raw
            || self.mac.len() != 64
            || !self
                .mac
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !key.verify(
                message(&self.envelope)?.as_bytes(),
                &hex::decode(&self.mac).map_err(|_| invalid("invalid evidence MAC"))?,
            )
        {
            return Err(invalid("invalid authenticated submission evidence"));
        }
        Ok(())
    }
}

struct Evidence {
    signed: BTreeMap<u64, (Link, SignedRequest)>,
    accepted: BTreeMap<u64, (Link, u64, AcceptedStatus)>,
}

fn message(envelope: &Envelope) -> Result<String> {
    Ok(crate::ledger::hash::canonical_json(
        &serde_json::json!({"domain":"oppen.submission-evidence.v1", "envelope":envelope}),
    )?)
}

fn digest(request: &ExchangeRequest, network: Network, genesis: &str) -> Result<String> {
    // v1 explicitly fixes nulls and signature widths without changing venue wire
    // serialization. Wire price/size strings retain their exact signed spelling.
    let signature = request.signature();
    digest_preimage(&serde_json::json!({
        "network": network, "ledger_genesis": genesis, "action":request.action(),
        "nonce":request.nonce(), "vault_address":request.vault_address(), "expires_after":request.expires_after(),
        "signature":{"r":format!("0x{}", hex::encode(signature.r)), "s":format!("0x{}", hex::encode(signature.s)), "v":signature.v}
    }))
}

fn digest_preimage(preimage: &Value) -> Result<String> {
    let canonical = crate::ledger::hash::canonical_json(preimage)?;
    let mut hash = Keccak256::new();
    hash.update(b"oppen.signed-submission.v1\0");
    hash.update(canonical.as_bytes());
    Ok(hex::encode(hash.finalize()))
}

impl SubmissionJournal {
    pub(super) fn verify_evidence_in(&self, connection: &Connection) -> Result<()> {
        self.evidence_in(connection).map(|_| ())
    }

    pub(crate) fn same_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            && matches!((&self.1, &other.1), (Some((a, ar)), Some((b, br))) if Arc::ptr_eq(a,b) && ar == br)
    }

    pub(crate) fn verify_owner(&self, registry: &RegistryJournal) -> Result<()> {
        if !std::ptr::eq(self.0.as_ref(), registry.ledger())
            || !self
                .1
                .as_ref()
                .is_some_and(|(own, _)| std::ptr::eq(own.as_ref(), registry))
        {
            return Err(invalid(
                "submission journal is not the signing authority owner",
            ));
        }
        Ok(())
    }

    pub(crate) fn verify_reserved(
        &self,
        receipt: &SubmissionReceipt,
        clearance: &Clearance,
    ) -> Result<()> {
        let mut guard = self.0.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Deferred)?;
        self.verify_reserved_in(&tx, receipt, clearance)?;
        let evidence = self.evidence_in(&tx)?;
        if evidence
            .signed
            .values()
            .any(|(_, signed)| signed.start.seq == receipt.seq)
        {
            return Err(invalid(
                "reservation already has signing evidence; never sign again",
            ));
        }
        Ok(())
    }

    fn verify_reserved_in(
        &self,
        connection: &Connection,
        receipt: &SubmissionReceipt,
        clearance: &Clearance,
    ) -> Result<()> {
        let replay = self.replay(connection)?;
        if receipt.chain != self.0.genesis
            || replay.starts.get(&receipt.seq) != Some(receipt)
            || replay.resolved.contains(&receipt.seq)
            || replay
                .accounts
                .get(&receipt.start.account)
                .and_then(|state| state.pending.as_ref())
                != Some(receipt)
        {
            return Err(invalid("submission reservation no longer current"));
        }
        let mut payload = validate_intent(&self.0, connection, &receipt.start, &receipt.agent)?;
        payload
            .as_object_mut()
            .ok_or_else(|| invalid("intent is not an object"))?
            .remove("reason");
        if payload != serde_json::to_value(clearance)?
            || clearance.network != self.0.network
            || clearance.route.binding.container != receipt.start.account
            || clearance.agent.as_str() != receipt.agent
        {
            return Err(invalid("signing clearance differs from reserved intent"));
        }
        Ok(())
    }

    pub(crate) fn publish_signed_in(
        &self,
        tx: &rusqlite::Transaction<'_>,
        proof: &GuardedSignature<'_>,
    ) -> Result<Appended> {
        self.verify_reserved_in(tx, proof.receipt(), proof.clearance())?;
        let evidence = self.evidence_in(tx)?;
        if evidence
            .signed
            .values()
            .any(|(_, signed)| signed.start.seq == proof.receipt().seq)
        {
            return Err(invalid("reservation already signed"));
        }
        let data = SignedRequest {
            start: Link {
                seq: proof.receipt().seq,
                hash: proof.receipt().hash.clone(),
            },
            route: proof.clearance().route.clone(),
            cloid: proof.receipt().cloid().clone(),
            action: proof.request().action().clone(),
            clearance: serde_json::to_value(proof.clearance())?,
            signer: proof.signer(),
            signed_at_ms: proof.signed_at_ms(),
            request_digest: digest(proof.request(), proof.clearance().network, &self.0.genesis)?,
        };
        let replay = self.replay(tx)?;
        self.validate_signed_data(tx, &replay, &data, proof.signed_at_ms())?;
        self.append_evidence(
            tx,
            Operation::Signed {
                request: Box::new(data),
            },
            proof.signed_at_ms(),
        )
    }

    pub(crate) fn verify_signed_in(
        &self,
        connection: &Connection,
        receipt: &SubmissionReceipt,
        signed: &Appended,
        request: &ExchangeRequest,
        clearance: &Clearance,
    ) -> Result<()> {
        self.verify_reserved_in(connection, receipt, clearance)?;
        let evidence = self.evidence_in(connection)?;
        let (link, data) = evidence
            .signed
            .get(&signed.seq)
            .ok_or_else(|| invalid("signed evidence missing"))?;
        if link != &Link::from(signed)
            || data.start.seq != receipt.seq
            || data.start.hash != receipt.hash
            || data.clearance != serde_json::to_value(clearance)?
            || &data.action != request.action()
            || data.request_digest != digest(request, clearance.network, &self.0.genesis)?
            || evidence.accepted.contains_key(&signed.seq)
        {
            return Err(invalid(
                "signed request or receipt mismatch, or already accepted",
            ));
        }
        Ok(())
    }

    pub(crate) fn record_post_result(
        &self,
        receipt: &SubmissionReceipt,
        signed: &Appended,
        request: &ExchangeRequest,
        clearance: &Clearance,
        response: &std::result::Result<ExchangeResponse, SubmissionPostError>,
        at_ms: u64,
    ) -> Result<()> {
        let Ok(response) = response else {
            return Ok(());
        };
        if response.kind != ExchangeResponseKind::Order {
            return Ok(());
        }
        let (oid, status) = match response.statuses.as_slice() {
            [Status::Resting { oid }] => (*oid, AcceptedStatus::Resting),
            [
                Status::Filled {
                    oid,
                    total_sz,
                    avg_px,
                },
            ] => (
                *oid,
                AcceptedStatus::Filled {
                    total_sz: *total_sz,
                    avg_px: *avg_px,
                },
            ),
            _ => return Ok(()),
        };
        let mut guard = self.0.lock()?;
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.verify_signed_in(&tx, receipt, signed, request, clearance)?;
        let evidence = self.evidence_in(&tx)?;
        let (_, data) = evidence
            .signed
            .get(&signed.seq)
            .ok_or_else(|| invalid("missing signed receipt"))?;
        validate_acceptance(data, oid, &status, at_ms)?;
        if evidence.accepted.iter().any(|(seq, (_, existing_oid, _))| {
            *existing_oid == oid
                && evidence.signed.get(seq).is_some_and(|(_, existing)| {
                    existing.route.binding.container == data.route.binding.container
                })
        }) {
            return Err(invalid(
                "OID already associated with another signed submission",
            ));
        }
        let appended = self.append_evidence(
            &tx,
            Operation::Accepted {
                signed: Link::from(signed),
                order_index: 0,
                oid,
                status,
            },
            at_ms,
        )?;
        tx.commit()?;
        self.0.note_head(&appended)?;
        Ok(())
    }

    fn validate_signed_data(
        &self,
        connection: &Connection,
        replay: &Replay,
        data: &SignedRequest,
        at_ms: u64,
    ) -> Result<()> {
        let receipt = replay
            .starts
            .get(&data.start.seq)
            .ok_or_else(|| invalid("signed evidence lacks reservation"))?;
        let mut payload = validate_intent(&self.0, connection, &receipt.start, &receipt.agent)?;
        payload
            .as_object_mut()
            .ok_or_else(|| invalid("intent is not an object"))?
            .remove("reason");
        let Action::Order { orders, .. } = &data.action else {
            return Err(invalid("submission evidence is not an order"));
        };
        let [order] = orders.as_slice() else {
            return Err(invalid("submission evidence requires exactly one order"));
        };
        if payload != data.clearance
            || data.start.hash != receipt.hash
            || data.route.network != self.0.network
            || data.route.binding.container != receipt.start.account
            || data.route.binding.agent.as_str() != receipt.agent
            || data.signer != data.route.binding.wallet.address
            || data.clearance.get("route") != Some(&serde_json::to_value(&data.route)?)
            || data.cloid != receipt.start.cloid
            || order.c.as_ref() != Some(&data.cloid)
            || data.signed_at_ms != at_ms
            || data.request_digest.len() != 64
            || !data
                .request_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("signed submission identity mismatch"));
        }
        let kind = &data.clearance["kind"];
        if kind["cleared"] != "order"
            || kind["px"] != serde_json::to_value(order.p.as_str())?
            || kind["sz"] != serde_json::to_value(order.s.as_str())?
            || kind["is_buy"] != order.b
            || kind["reduce_only"] != order.r
        {
            // Decimal formatting in audit is not necessarily normalized wire formatting.
            let decimal = |value: &Value| -> Result<Decimal> {
                value
                    .as_str()
                    .ok_or_else(|| invalid("missing order decimal"))?
                    .parse()
                    .map_err(|_| invalid("invalid order decimal"))
            };
            if kind["cleared"] != "order"
                || decimal(&kind["px"])?
                    != order
                        .p
                        .as_str()
                        .parse::<Decimal>()
                        .map_err(|_| invalid("invalid wire price"))?
                || decimal(&kind["sz"])?
                    != order
                        .s
                        .as_str()
                        .parse::<Decimal>()
                        .map_err(|_| invalid("invalid wire size"))?
                || kind["is_buy"] != order.b
                || kind["reduce_only"] != order.r
            {
                return Err(invalid("signed action differs from clearance"));
            }
        }
        Ok(())
    }

    fn evidence_in(&self, connection: &Connection) -> Result<Evidence> {
        let (registry, _) = self
            .1
            .as_ref()
            .ok_or_else(|| invalid("authenticated submission authority required"))?;
        let replay = self.replay(connection)?;
        let mut evidence = Evidence {
            signed: BTreeMap::new(),
            accepted: BTreeMap::new(),
        };
        let mut signed_starts = HashSet::new();
        let mut accepted_oids = HashSet::new();
        let mut statement = connection.prepare(&format!("SELECT {SELECT_EVENT_COLUMNS}, idem_key FROM events WHERE kind IN ('submission_signed','submission_accepted') ORDER BY seq"))?;
        let mut rows = statement.query([])?;
        while let Some(record) = rows.next()? {
            let event = crate::ledger::event_from_row(record)?;
            let raw: String = record
                .get::<_, Option<String>>(4)?
                .ok_or_else(|| invalid("submission evidence redacted"))?;
            let auth: Authenticated = serde_json::from_str(&raw)?;
            auth.verify(&raw, registry.authority_key())?;
            let e = auth.envelope;
            if e.version != 1
                || e.network != self.0.network
                || e.genesis != self.0.genesis
                || e.seq != event.seq
                || e.prev_hash != event.prev_hash
                || timestamp(e.at_ms)? != event.ts_ms
                || e.operation.kind() != event.kind
                || event.agent_id.is_some()
                || event.snapshot_id.is_some()
                || event.snapshot_hash.is_some()
                || record.get::<_, Option<String>>(12)?.as_deref()
                    != Some(e.operation.key().as_str())
            {
                return Err(invalid("submission evidence envelope mismatch"));
            }
            let link = Link {
                seq: event.seq,
                hash: event.hash,
            };
            match e.operation {
                Operation::Signed { request } => {
                    self.validate_signed_data(connection, &replay, &request, e.at_ms)?;
                    if request.start.seq >= e.seq || !signed_starts.insert(request.start.seq) {
                        return Err(invalid("duplicate or unordered signing evidence"));
                    }
                    evidence.signed.insert(e.seq, (link, *request));
                }
                Operation::Accepted {
                    signed,
                    order_index,
                    oid,
                    status,
                } => {
                    let (expected, request) = evidence
                        .signed
                        .get(&signed.seq)
                        .ok_or_else(|| invalid("acceptance precedes signing"))?;
                    validate_acceptance(request, oid, &status, e.at_ms)?;
                    if expected != &signed
                        || order_index != 0
                        || evidence.accepted.contains_key(&signed.seq)
                        || !accepted_oids.insert((request.route.binding.container, oid))
                    {
                        return Err(invalid("conflicting OID acceptance"));
                    }
                    evidence.accepted.insert(signed.seq, (link, oid, status));
                }
            }
        }
        Ok(evidence)
    }

    fn append_evidence(
        &self,
        tx: &rusqlite::Transaction<'_>,
        operation: Operation,
        at_ms: u64,
    ) -> Result<Appended> {
        let (registry, _) = self
            .1
            .as_ref()
            .ok_or_else(|| invalid("authenticated submission authority required"))?;
        let (head, prev_hash) = crate::ledger::head(tx)?;
        self.0.note_head(&Appended {
            seq: head,
            hash: prev_hash.clone(),
        })?;
        let seq = head.checked_add(1).ok_or(LedgerError::SeqOutOfRange)?;
        let key = operation.key();
        let kind = operation.kind();
        let envelope = Envelope {
            version: 1,
            network: self.0.network,
            genesis: self.0.genesis.clone(),
            seq,
            prev_hash,
            at_ms,
            operation,
        };
        let mac = registry
            .authority_key()
            .sign(message(&envelope)?.as_bytes())
            .map_err(|_| invalid("submission MAC signing failed"))?;
        let payload = serde_json::to_value(Authenticated {
            envelope,
            mac: hex::encode(mac),
        })?;
        crate::ledger::append_keyed_in_tx(
            tx,
            &NewEvent {
                kind,
                ts_ms: timestamp(at_ms)?,
                agent_id: None,
                payload: &payload,
                snapshot: None,
            },
            &key,
        )?
        .ok_or_else(|| invalid("submission evidence already recorded"))
    }
}

fn validate_acceptance(
    data: &SignedRequest,
    oid: u64,
    status: &AcceptedStatus,
    at_ms: u64,
) -> Result<()> {
    if oid == 0 || at_ms < data.signed_at_ms {
        return Err(invalid("invalid acceptance identity or time"));
    }
    if let AcceptedStatus::Filled { total_sz, avg_px } = status {
        let Action::Order { orders, .. } = &data.action else {
            return Err(invalid("acceptance action missing"));
        };
        let size = orders
            .first()
            .ok_or_else(|| invalid("acceptance order missing"))?
            .s
            .as_str()
            .parse::<Decimal>()
            .map_err(|_| invalid("acceptance size invalid"))?;
        if *total_sz <= Decimal::ZERO || *total_sz > size || *avg_px <= Decimal::ZERO {
            return Err(invalid("invalid filled acceptance"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn submission_evidence_v1_digest_pins_canonical_encoding_and_every_signed_field() {
        // Encoding-only fixture, not a signable request or a journal append.
        // The guarded transport test independently matches this scheme against
        // the complete request actually serialized by ExchangeClient.
        let preimage = json!({
            "network": "testnet", "ledger_genesis": "fixture-genesis",
            "action": {"type":"order","orders":[],"grouping":"na"},
            "nonce": 1, "vault_address": null, "expires_after": null,
            "signature": {"r":format!("0x{:0>64}", "1"), "s":"0x02", "v":27}
        });
        let canonical = crate::ledger::hash::canonical_json(&preimage).unwrap();
        // Pin the domain and canonical bytes independently of the production
        // digest helper, so dropping a field or changing a version fails.
        let expected_canonical = format!(
            r#"{{"action":{{"grouping":"na","orders":[],"type":"order"}},"expires_after":null,"ledger_genesis":"fixture-genesis","network":"testnet","nonce":1,"signature":{{"r":"{}","s":"0x02","v":27}},"vault_address":null}}"#,
            preimage["signature"]["r"].as_str().unwrap()
        );
        assert_eq!(canonical, expected_canonical);
        let mut expected = Keccak256::new();
        expected.update(b"oppen.signed-submission.v1\0");
        expected.update(expected_canonical.as_bytes());
        let baseline = digest_preimage(&preimage).unwrap();
        assert_eq!(baseline, hex::encode(expected.finalize()));
        for (path, value) in [
            ("/nonce", json!(2)),
            ("/network", json!("mainnet")),
            ("/expires_after", json!(2)),
            (
                "/vault_address",
                json!("0x1111111111111111111111111111111111111111"),
            ),
            ("/ledger_genesis", json!("different-genesis")),
            ("/signature/r", json!("0x03")),
            ("/signature/s", json!("0x04")),
            ("/signature/v", json!(28)),
            ("/action/grouping", json!("normalTpsl")),
        ] {
            let mut changed = preimage.clone();
            *changed.pointer_mut(path).unwrap() = value;
            assert_ne!(digest_preimage(&changed).unwrap(), baseline, "{path}");
        }
        let mut absent = preimage;
        absent.as_object_mut().unwrap().remove("expires_after");
        assert_ne!(
            digest_preimage(&absent).unwrap(),
            baseline,
            "explicit null is committed"
        );
    }

    #[test]
    fn submission_evidence_invalid_mac_and_changed_authenticated_fields_are_refused() {
        let key = crate::keys::HmacKey::from_bytes([31; 32]);
        let envelope = Envelope {
            version: 1,
            network: Network::Testnet,
            genesis: "fixture-genesis".into(),
            seq: 2,
            prev_hash: "fixture-head".into(),
            at_ms: 3,
            operation: Operation::Accepted {
                signed: Link {
                    seq: 1,
                    hash: "fixture-signed".into(),
                },
                order_index: 0,
                oid: 4,
                status: AcceptedStatus::Resting,
            },
        };
        let mac = hex::encode(key.sign(message(&envelope).unwrap().as_bytes()).unwrap());
        let value = serde_json::to_value(Authenticated { envelope, mac }).unwrap();
        let verify = |value: &Value| {
            let raw = crate::ledger::hash::canonical_json(value).unwrap();
            let auth: Authenticated = serde_json::from_str(&raw).unwrap();
            auth.verify(&raw, &key)
        };
        assert!(verify(&value).is_ok());
        // Verify envelopes in isolation; do not synthesize or re-anchor a tail.
        for (path, replacement) in [
            ("/mac", json!("00".repeat(32))),
            ("/mac", json!("bad")),
            ("/envelope/network", json!("mainnet")),
            ("/envelope/at_ms", json!(4)),
            ("/envelope/operation/oid", json!(5)),
            (
                "/envelope/operation/signed/hash",
                json!("different-receipt"),
            ),
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(path).unwrap() = replacement;
            assert!(
                matches!(verify(&changed), Err(SubmissionError::InvalidRecord { .. })),
                "{path}"
            );
        }
    }
}
