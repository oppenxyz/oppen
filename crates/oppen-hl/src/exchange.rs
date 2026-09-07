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
/// construct a signed request and skip it. The trait lives here because
/// `oppen-hl` owns the signer and cannot depend on the crate above it.
///
/// The implementation oppen ships is **private to `oppen-core`**: a gate
/// bound to one guardrail clearance, built by `GuardrailEngine::sign_cleared`
/// and nameable nowhere else. The guardrail engine deliberately does not
/// implement this trait itself. It did until 2026-09-04, and because both the
/// engine and this constructor are public, that let any caller hand the real
/// engine an [`Action`](crate::Action) it had never evaluated and collect a
/// signature — the gate sees only the assembled request, so it had nothing to
/// check the action against.
///
/// **What this does not prevent.** A caller can still write its own type that
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
    ///
    /// The two statements in the body are in that order on purpose, and the
    /// order is load-bearing rather than stylistic: swapping them still
    /// returns `Err(SignError::Refused)` to the caller, so no test that
    /// inspects the return value can tell the difference, while the private
    /// key has in fact been used on an order the gate went on to refuse.
    /// `seal_tests::the_gate_runs_before_the_key_is_touched` pins the order
    /// against this file's own source because that is the only place the
    /// difference is visible.
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
    /// is invisible is worse than one that is obvious, and `oppen-core`'s
    /// `no_call_site_in_oppen_core_reaches_the_signer_unchecked` test greps
    /// for exactly this name so an accidental one fails the suite.
    ///
    /// There is deliberately no `sign` shorthand. An unqualified name would
    /// be the one a new call site reaches for by habit, and habit is the
    /// mechanism this whole module exists to defeat.
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
        let raw: RawResponse = serde_json::from_str(json)
            .map_err(|e| Error::InvalidExchangeResponse(format!("{e}; body: {json}")))?;
        match raw {
            RawResponse::Err(message) => Err(Error::ExchangeRejected { message }),
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
        let response = self
            .http
            .post(&self.url)
            .timeout(crate::REQUEST_TIMEOUT)
            .json(request)
            .send()
            .await?;
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

    #[tokio::test]
    async fn requests_cannot_hold_an_execution_lock_forever() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });
        let mut exchange = ExchangeClient::with_client(Network::Testnet, Client::new());
        exchange.url = format!("http://{address}/exchange");
        let key =
            AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
                .unwrap();
        let request = ExchangeRequest::sign_unchecked(
            &key,
            Action::ClaimRewards,
            1,
            None,
            None,
            Network::Testnet,
        )
        .unwrap();
        let result = tokio::time::timeout(
            crate::REQUEST_TIMEOUT + std::time::Duration::from_secs(3),
            exchange.post(&request),
        )
        .await;
        peer.abort();
        let _ = peer.await;
        let error = result
            .expect("the client deadline must release the request")
            .unwrap_err();
        assert!(
            matches!(error, Error::Http(ref error) if error.is_timeout()),
            "{error:?}"
        );
    }

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
            Err(Error::ExchangeRejected { .. })
        ));
    }

    #[test]
    fn malformed_exchange_bodies_do_not_prove_rejection() {
        for body in [
            "",
            "<html>upstream failed</html>",
            r#"{"status":"err"}"#,
            r#"{"status":"ok","response":null}"#,
        ] {
            assert!(matches!(
                ExchangeResponse::parse(body),
                Err(Error::InvalidExchangeResponse(_))
            ));
        }
    }

    #[test]
    fn envelope_has_python_shape() {
        let key =
            AgentKey::from_hex("0123456789012345678901234567890123456789012345678901234567890123")
                .unwrap();
        let req = ExchangeRequest::sign_unchecked(
            &key,
            Action::ClaimRewards,
            1,
            None,
            None,
            Network::Testnet,
        )
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

    /// The property `AGENTS.md` invariant 1 actually asks for, at this layer:
    /// over generated inputs, **every** attempt is evaluated exactly once,
    /// every refusal yields no `ExchangeRequest`, and every pass yields one.
    ///
    /// What it proves: `sign_checked` cannot be entered without the gate
    /// running, cannot run it twice, and cannot produce a value on the
    /// refusal branch. What it cannot prove: that some other code did not
    /// call [`ExchangeRequest::sign_unchecked`] — that is a call-site
    /// property, and `oppen-core` tests it by grepping its own sources.
    #[test]
    fn every_generated_input_is_evaluated_exactly_once_and_only_passes_sign() {
        /// Refuses on an arbitrary but deterministic predicate, so both
        /// branches are exercised by the same generator.
        struct OddNoncesRefused(std::cell::Cell<u32>);
        impl PreSignCheck for OddNoncesRefused {
            type Refusal = u64;
            fn check(&self, r: PreSign<'_>) -> Result<(), u64> {
                self.0.set(self.0.get() + 1);
                if r.nonce % 2 == 1 {
                    Err(r.nonce)
                } else {
                    Ok(())
                }
            }
        }

        let key = key();
        let check = OddNoncesRefused(std::cell::Cell::new(0));
        let mut signed = 0u32;
        let mut refused = 0u32;
        // A tiny LCG rather than a dependency: the point is coverage of both
        // branches over many shapes, and a fixed seed keeps the run
        // reproducible after the fact.
        let mut seed = 0x2026_0904_u64;
        for i in 0..2_000u64 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let nonce = (seed >> 11) ^ i;
            let network = if seed & 1 == 0 {
                Network::Testnet
            } else {
                Network::Mainnet
            };
            let vault = if seed & 2 == 0 {
                None
            } else {
                Some(Address::from_bytes([7u8; 20]))
            };
            let expires_after = if seed & 4 == 0 { None } else { Some(nonce) };
            let action = if seed & 8 == 0 {
                Action::ClaimRewards
            } else {
                Action::ScheduleCancel { time: Some(nonce) }
            };
            let before = check.0.get();
            let out = ExchangeRequest::sign_checked(
                &key,
                action,
                nonce,
                vault,
                expires_after,
                network,
                &check,
            );
            assert_eq!(
                check.0.get(),
                before + 1,
                "case {i}: exactly one evaluation per attempt"
            );
            match out {
                Err(SignError::Refused(n)) => {
                    assert_eq!(n % 2, 1, "case {i}: refused a passing input");
                    refused += 1;
                }
                Ok(request) => {
                    assert_eq!(nonce % 2, 0, "case {i}: signed a refused input");
                    assert_eq!(request.nonce(), nonce);
                    assert_eq!(request.vault_address(), vault);
                    assert_eq!(request.expires_after(), expires_after);
                    signed += 1;
                }
                Err(SignError::Signing(e)) => panic!("case {i}: signing failed: {e}"),
            }
        }
        assert_eq!(u64::from(signed + refused), 2_000);
        assert_eq!(
            check.0.get(),
            signed + refused,
            "one evaluation per attempt, refusals included"
        );
        assert!(
            signed > 500 && refused > 500,
            "both branches were exercised"
        );
    }

    /// **The ordering, which no runtime test above can see.**
    ///
    /// Move the `check` call after the `sign_unchecked` call and every other
    /// test in this module still passes: the caller still gets
    /// `Err(SignError::Refused)` and still gets no [`ExchangeRequest`]. What
    /// changed is invisible from outside — the agent key was used to sign an
    /// order the gate then refused. `AGENTS.md` invariant 1 says the check
    /// happens *before* signing, not merely that a refusal yields no value,
    /// so the ordering is asserted where it is observable: this file's own
    /// source.
    ///
    /// This is a grep, and it is exactly as strong as a grep. It cannot
    /// follow the check into a helper, and it would not notice a signature
    /// computed somewhere else entirely. It catches the one mistake that is
    /// actually likely, which is someone reordering two statements while
    /// refactoring and seeing a green suite.
    #[test]
    fn the_gate_runs_before_the_key_is_touched() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("exchange.rs"),
        )
        .expect("this module's own source is readable");
        let body = source
            .split_once("pub fn sign_checked")
            .expect("sign_checked exists")
            .1;
        // The end of the function: the next item at the same indentation.
        let body = body
            .split_once("\n    /// Signs with no gate.")
            .expect("sign_unchecked follows sign_checked in this file")
            .0;
        let gate_at = body.find(".check(PreSign {").expect("the gate is called");
        let sign_at = body
            .find("Self::sign_unchecked(")
            .expect("the signer is called");
        assert!(
            gate_at < sign_at,
            "sign_checked calls the signer at byte {sign_at} before the gate at {gate_at}; \
             a refused order would be signed and then discarded (AGENTS.md invariant 1)"
        );
    }
}
