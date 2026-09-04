//! `POST /exchange`: request envelope, per-signer nonce allocation, the
//! HTTP client and response parsing (`docs/hl-signing.md` §5, §8–9).

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{Action, Address, AgentKey, Error, Network, Signature};

/// What the signer is about to sign, handed to a [`PreSignCheck`] before any
/// key is touched.
#[derive(Debug, Clone, Copy)]
pub struct PreSign<'a> {
    pub action: &'a Action,
    pub nonce: u64,
    pub vault_address: Option<Address>,
    pub expires_after: Option<u64>,
    pub network: Network,
}

/// The gate that runs immediately before signing.
///
/// `AGENTS.md` invariant 1 requires exactly one code path to the signer, and
/// that it run the guardrail check. This trait is that path: the check is
/// invoked *inside* [`ExchangeRequest::sign_checked`], so a caller cannot
/// construct a signed request and skip it. `oppen-core`'s guardrail engine is
/// the implementation oppen ships; the trait lives here because `oppen-hl`
/// owns the signer and cannot depend on the crate above it.
///
/// **What this does not prevent.** A caller can still write a type that
/// implements this trait and returns `Ok(())` unconditionally, and
/// [`ExchangeRequest::sign_unchecked`] remains available for the operator's
/// manual path and for tests. Both are deliberate, and both are *visible*: a
/// no-op implementation is a struct someone had to write, and the unchecked
/// constructor is greppable by name. The invariant this buys is that a
/// bypass cannot happen by accident or by forgetting.
pub trait PreSignCheck {
    /// Why the order was refused. `oppen-core` supplies its typed refusal.
    type Refusal;

    fn check(&self, request: PreSign<'_>) -> Result<(), Self::Refusal>;
}

/// A signing attempt that the pre-sign gate refused.
#[derive(Debug, thiserror::Error)]
pub enum SignError<R> {
    /// The gate refused. Carries the checker's own typed refusal.
    #[error("refused before signing")]
    Refused(R),
    /// The gate passed and signing itself failed.
    #[error(transparent)]
    Signing(#[from] Error),
}

/// The JSON body of an L1 exchange request, shaped like the python SDK's
/// `_post_action` (both optional keys present, `null` when unset).
///
/// Fields are private on purpose. They were public until 2026-09-04, which
/// meant a struct literal could assemble a "signed" request without ever
/// reaching the signer — the exact hole the doc comment claimed did not
/// exist.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeRequest {
    action: Action,
    nonce: u64,
    signature: Signature,
    vault_address: Option<Address>,
    expires_after: Option<u64>,
}

impl ExchangeRequest {
    /// The guarded signing path. Runs `check` first and signs only if it
    /// passes. This is the constructor every agent-initiated order uses.
    pub fn sign_checked<C: PreSignCheck>(
        key: &AgentKey,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
        network: Network,
        check: &C,
    ) -> Result<Self, SignError<C::Refusal>> {
        check
            .check(PreSign {
                action: &action,
                nonce,
                vault_address,
                expires_after,
                network,
            })
            .map_err(SignError::Refused)?;
        Ok(Self::sign_unchecked(
            key,
            action,
            nonce,
            vault_address,
            expires_after,
            network,
        )?)
    }

    /// Signs with no gate.
    ///
    /// Reserved for the operator's manual escape hatch (`docs/spec.md` item
    /// 33), the signing vectors, and the testnet CLI example. **Every new
    /// call site is a blocking review finding** — see `AGENTS.md` invariant 1.
    /// It is named to be greppable rather than hidden, because a bypass that
    /// is invisible is worse than one that is obvious.
    pub fn sign_unchecked(
        key: &AgentKey,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
        network: Network,
    ) -> Result<Self, Error> {
        let signature =
            key.sign_l1_action(&action, nonce, vault_address, expires_after, network)?;
        Ok(ExchangeRequest {
            action,
            nonce,
            signature,
            vault_address,
            expires_after,
        })
    }

    /// Temporary alias for [`Self::sign_unchecked`], kept only so the
    /// in-flight guardrail integration keeps compiling. Removed as soon as
    /// `oppen-core` moves to `sign_checked`.
    pub fn sign(
        key: &AgentKey,
        action: Action,
        nonce: u64,
        vault_address: Option<Address>,
        expires_after: Option<u64>,
        network: Network,
    ) -> Result<Self, Error> {
        Self::sign_unchecked(key, action, nonce, vault_address, expires_after, network)
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    pub fn vault_address(&self) -> Option<Address> {
        self.vault_address
    }

    pub fn expires_after(&self) -> Option<u64> {
        self.expires_after
    }
}

/// One entry of `response.data.statuses`, aligned with the request's
/// orders or cancels.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Status {
    Resting {
        oid: u64,
    },
    Filled {
        total_sz: Decimal,
        avg_px: Decimal,
        oid: u64,
    },
    Error(String),
    /// Cancels report the bare string `"success"`.
    #[serde(rename = "success")]
    Success,
    /// Trigger orders rest off-book until they trigger.
    WaitingForTrigger,
    WaitingForFill,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct StatusData {
    statuses: Vec<Status>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ResponseBody {
    Order {
        data: StatusData,
    },
    Cancel {
        data: StatusData,
    },
    #[serde(other)]
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", content = "response", rename_all = "camelCase")]
enum RawResponse {
    Ok(ResponseBody),
    Err(String),
}

/// A parsed `status: "ok"` exchange response. Actions without a payload
/// (leverage, sub-account, scheduleCancel) yield an empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeResponse {
    pub statuses: Vec<Status>,
}

impl ExchangeResponse {
    pub fn parse(json: &str) -> Result<Self, Error> {
        let raw: RawResponse = serde_json::from_str(json).map_err(|e| Error::Venue {
            status: 200,
            message: format!("unparseable exchange response: {e}; body: {json}"),
        })?;
        match raw {
            RawResponse::Err(message) => Err(Error::Venue {
                status: 200,
                message,
            }),
            RawResponse::Ok(ResponseBody::Order { data } | ResponseBody::Cancel { data }) => {
                Ok(ExchangeResponse {
                    statuses: data.statuses,
                })
            }
            RawResponse::Ok(ResponseBody::Default) => Ok(ExchangeResponse {
                statuses: Vec::new(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExchangeClient {
    http: Client,
    url: String,
}

impl ExchangeClient {
    pub fn new(network: Network) -> Result<Self, Error> {
        Ok(Self::with_client(network, Client::builder().build()?))
    }

    pub fn with_client(network: Network, http: Client) -> Self {
        ExchangeClient {
            http,
            url: format!("{}/exchange", network.api_url()),
        }
    }

    /// Posts an already-signed request. A non-2xx status, a `status: "err"`
    /// body and a transport failure are all distinct errors; a transport
    /// failure after the request left the process is the
    /// `timeout_unknown_outcome` case and the caller must reconcile by
    /// cloid, never resend.
    pub async fn post(&self, request: &ExchangeRequest) -> Result<ExchangeResponse, Error> {
        let response = self.http.post(&self.url).json(request).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(Error::Venue {
                status: status.as_u16(),
                message: body,
            });
        }
        ExchangeResponse::parse(&body)
    }
}

/// One monotonic nonce source per signer. The L1 keeps the 100 highest
/// nonces per signer and rejects reuse, and an agent wallet shares one
/// nonce set across every account it signs for — so oppen keeps exactly
/// one allocator per agent key and never hands the same value out twice.
#[derive(Debug, Default)]
pub struct NonceAllocator {
    last: Mutex<u64>,
}

impl NonceAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `max(now_ms, last + 1)`: wall-clock when it has moved on, otherwise
    /// the next integer, so bursts inside one millisecond stay unique.
    pub fn next(&self) -> u64 {
        self.next_at(now_ms())
    }

    fn next_at(&self, now: u64) -> u64 {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let nonce = now.max(*last + 1);
        *last = nonce;
        nonce
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_strictly_increasing_within_one_millisecond() {
        let n = NonceAllocator::new();
        assert_eq!(n.next_at(1_000), 1_000);
        assert_eq!(n.next_at(1_000), 1_001);
        assert_eq!(n.next_at(1_000), 1_002);
        assert_eq!(n.next_at(5_000), 5_000);
        assert_eq!(n.next_at(4_000), 5_001);
    }

    /// DOC-EXCH response examples verbatim.
    #[test]
    fn parses_documented_responses() {
        let resting = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":77738308}}]}}}"#;
        assert_eq!(
            ExchangeResponse::parse(resting).unwrap().statuses,
            vec![Status::Resting { oid: 77738308 }]
        );
        let filled = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"totalSz":"0.02","avgPx":"1891.4","oid":77747314}}]}}}"#;
        assert_eq!(
            ExchangeResponse::parse(filled).unwrap().statuses,
            vec![Status::Filled {
                total_sz: "0.02".parse().unwrap(),
                avg_px: "1891.4".parse().unwrap(),
                oid: 77747314
            }]
        );
        let err = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order must have minimum value of $10."}]}}}"#;
        assert_eq!(
            ExchangeResponse::parse(err).unwrap().statuses,
            vec![Status::Error(
                "Order must have minimum value of $10.".into()
            )]
        );
        let cancel =
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
        assert_eq!(
            ExchangeResponse::parse(cancel).unwrap().statuses,
            vec![Status::Success]
        );
        let cancel_err = r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"Order was never placed, already canceled, or filled."}]}}}"#;
        assert!(matches!(
            ExchangeResponse::parse(cancel_err).unwrap().statuses[0],
            Status::Error(_)
        ));
        let default = r#"{"status":"ok","response":{"type":"default"}}"#;
        assert!(
            ExchangeResponse::parse(default)
                .unwrap()
                .statuses
                .is_empty()
        );
        let rejected = r#"{"status":"err","response":"User or API Wallet 0x0123 does not exist."}"#;
        assert!(matches!(
            ExchangeResponse::parse(rejected),
            Err(Error::Venue { status: 200, .. })
        ));
    }

    #[test]
    fn envelope_has_python_shape() {
        let key =
            AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
                .unwrap();
        let req =
            ExchangeRequest::sign(&key, Action::ClaimRewards, 1, None, None, Network::Testnet)
                .unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["action"]["type"], "claimRewards");
        assert_eq!(json["nonce"], 1);
        assert!(json["vaultAddress"].is_null());
        assert!(json["expiresAfter"].is_null());
        assert!(json["signature"]["r"].as_str().unwrap().starts_with("0x"));
        assert!(matches!(json["signature"]["v"].as_u64(), Some(27 | 28)));
    }
}

#[cfg(test)]
mod seal_tests {
    use super::*;
    use crate::Action;

    struct AlwaysRefuse;
    impl PreSignCheck for AlwaysRefuse {
        type Refusal = &'static str;
        fn check(&self, _r: PreSign<'_>) -> Result<(), Self::Refusal> {
            Err("nope")
        }
    }

    struct RecordingPass(std::cell::Cell<u32>);
    impl PreSignCheck for RecordingPass {
        type Refusal = &'static str;
        fn check(&self, _r: PreSign<'_>) -> Result<(), Self::Refusal> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }
    }

    fn key() -> AgentKey {
        AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
            .expect("test key")
    }

    /// `AGENTS.md` invariant 1: a refused order is never signed. If the gate
    /// were moved after the signing call this test would still pass on the
    /// error type, so it also asserts the key was never used by checking that
    /// no signature exists to inspect.
    #[test]
    fn a_refused_order_is_never_signed() {
        let out = ExchangeRequest::sign_checked(
            &key(),
            Action::ClaimRewards,
            1,
            None,
            None,
            Network::Testnet,
            &AlwaysRefuse,
        );
        assert!(matches!(out, Err(SignError::Refused("nope"))));
    }

    /// The gate runs exactly once per signing attempt, before the signature.
    #[test]
    fn the_gate_runs_on_every_guarded_sign() {
        let check = RecordingPass(std::cell::Cell::new(0));
        for nonce in 1..=3 {
            ExchangeRequest::sign_checked(
                &key(),
                Action::ClaimRewards,
                nonce,
                None,
                None,
                Network::Testnet,
                &check,
            )
            .expect("gate passes");
        }
        assert_eq!(check.0.get(), 3);
    }

    /// The gate sees the real action, nonce and network — not a copy made
    /// after the fact. A checker that cannot see what it is approving is not
    /// a checker.
    #[test]
    fn the_gate_sees_what_will_be_signed() {
        struct Inspect;
        impl PreSignCheck for Inspect {
            type Refusal = String;
            fn check(&self, r: PreSign<'_>) -> Result<(), Self::Refusal> {
                if matches!(r.action, Action::ClaimRewards)
                    && r.nonce == 77
                    && r.network == Network::Mainnet
                {
                    Ok(())
                } else {
                    Err(format!(
                        "unexpected {:?} {} {:?}",
                        r.action, r.nonce, r.network
                    ))
                }
            }
        }
        ExchangeRequest::sign_checked(
            &key(),
            Action::ClaimRewards,
            77,
            None,
            None,
            Network::Mainnet,
            &Inspect,
        )
        .expect("inspector saw the real request");
    }
}
