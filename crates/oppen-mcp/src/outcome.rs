//! What a tool call answers with (`docs/spec.md` item 19).
//!
//! Two shapes, and the split is the point.
//!
//! [`Reply`] is the **synchronous result contract**: the order rested, the
//! order filled, the cancels landed, the engine wants an approval, or a
//! predicate refused. Every one of those is a correct, expected outcome of
//! asking, so every one of them is a *successful* tool call carrying a
//! `status` — a protocol error there would invite a blind retry, which is
//! exactly the wrong response to a limit.
//!
//! [`ToolError`] is the other half: oppen could not answer, the venue said no,
//! or the request left the process and its outcome is unknown. Those are
//! protocol errors, and they carry their taxonomy in `ErrorData::data` rather
//! than in a formatted sentence, because `AGENTS.md` invariant 8 says every
//! rejection is typed and a caller that has to parse English is not reading a
//! type.
//!
//! Both are deterministic JSON (invariant 6): `contract_version` first, then
//! the tag, then the fields that tag implies. `tests::` pins the exact bytes
//! of every variant, which is what actually holds the invariant — the derives
//! only make it likely.

use oppen_core::guardrail::{Refusal, VenueRule};
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use rust_decimal::Decimal;
use serde::Serialize;

/// Bumped when a field changes meaning, not when one is added — the rule
/// `oppen_core::state::AccountState` already follows, kept identical here so
/// an agent reads one version number across the whole surface.
const CONTRACT_VERSION: u32 = 0;

/// The envelope every execution tool answers with.
#[derive(Debug, Serialize)]
pub(crate) struct Reply {
    contract_version: u32,
    #[serde(flatten)]
    outcome: Outcome,
}

/// The statuses of item 19, plus `canceled`.
///
/// `canceled` is an addition: item 19's result shape is order-shaped (`oid`,
/// `filled_sz`, `avg_px`) and a cancel has none of those. Recorded as
/// `docs/decisions.md` C4.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// On the book, and it will stay there until it fills or is cancelled.
    Resting { oid: u64, cloid: Option<String> },

    /// Crossed on arrival. `avg_px` is the venue's, not a computed estimate.
    Filled {
        oid: u64,
        cloid: Option<String>,
        filled_sz: Decimal,
        avg_px: Decimal,
    },

    /// A cancel or cancel-all. Partial success is the normal case — an order
    /// that filled a moment ago cannot be cancelled — so the failures are
    /// itemised rather than collapsed into a count the agent cannot act on.
    Canceled {
        requested: usize,
        canceled: usize,
        failed: Vec<CancelFailure>,
    },

    /// Spec item 28. Nothing was signed; the engine is holding a proposal and
    /// the operator decides. `approval_id` is a receipt, not a credential.
    PendingApproval {
        approval_id: String,
        symbol: String,
        notional_usd: Decimal,
        expires_at_ms: u64,
    },

    /// A predicate refused before signing. Nothing reached the venue, no
    /// nonce was spent.
    Rejected {
        /// Part of the type, not a hint (item 19).
        retryable: bool,
        #[serde(flatten)]
        rejection: Rejection,
    },
}

/// One cancel the venue would not take.
///
/// The venue's own words, verbatim and display-only: program logic branches
/// on the fact that the cancel failed, never on this string (`AGENTS.md`
/// conventions).
#[derive(Debug, Serialize)]
pub(crate) struct CancelFailure {
    pub oid: Option<u64>,
    pub cloid: Option<String>,
    pub venue_message: String,
}

/// Why an order was refused before it was signed.
///
/// The mapping is the one `oppen_core::guardrail::Refusal`'s own
/// documentation already states, with one refinement: the two rate refusals
/// become `rate_limited` rather than `guardrail_reject`, because they are the
/// only refusals that clear on their own and the agent needs to know that
/// from the code rather than by reading the sentence. `docs/decisions.md` C2.
#[derive(Debug, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub(crate) enum Rejection {
    /// A guardrail predicate breached, or the engine could not establish that
    /// one was not. `refusal` names the predicate, the observed value and the
    /// limit, in the engine's own serialization.
    GuardrailReject { refusal: Refusal },

    /// A rule the *venue* would have enforced, caught here so the order never
    /// consumes a nonce or a rate token. Typed subtypes — `min_notional`,
    /// price decimals, size decimals — from oppen's own validation, never
    /// from parsing a venue sentence.
    VenueReject { subtype: VenueRule },

    /// oppen's own budget, which item 10 exists to spend before the venue's.
    /// The only rejection here that clears without the agent changing
    /// anything, which is why `retry_after_ms` is on it.
    RateLimited {
        retry_after_ms: u64,
        refusal: Refusal,
    },

    /// Spec item 26. The kill switch is engaged; new orders are paused.
    TradingPaused { refusal: Refusal },
}

impl Rejection {
    /// The `retryable` the envelope carries.
    ///
    /// Only a rate refusal clears on its own. A fail-closed refusal — a stale
    /// feed, an unreconciled account — is deliberately *not* retryable even
    /// though the condition may pass: telling an agent to retry into a
    /// degraded feed is how a quiet outage becomes a retry storm.
    fn retryable(&self) -> bool {
        matches!(self, Rejection::RateLimited { .. })
    }
}

impl Reply {
    fn new(outcome: Outcome) -> Self {
        Reply {
            contract_version: CONTRACT_VERSION,
            outcome,
        }
    }

    /// Render as the successful tool call it is.
    ///
    /// Serialization cannot fail: every field is a string, an integer, a
    /// `Decimal` written as a string, or a `#[derive(Serialize)]` type built
    /// from those. `expect` rather than a `Result` no caller could act on.
    pub(crate) fn into_result(self) -> CallToolResult {
        let json = serde_json::to_string(&self).expect("Reply serializes");
        CallToolResult::success(vec![ContentBlock::text(json)])
    }
}

/// Build the reply for a refusal from the engine.
///
/// `ApprovalRequired` is not a rejection and does not arrive here as one: it
/// is item 28's `pending_approval` status, and the engine has already spent
/// the order-rate token to mint the proposal.
pub(crate) fn refused(refusal: Refusal) -> Reply {
    let outcome = match refusal {
        Refusal::ApprovalRequired {
            symbol,
            notional_usd,
            approval_id,
            expires_at_ms,
        } => Outcome::PendingApproval {
            approval_id,
            symbol,
            notional_usd,
            expires_at_ms,
        },
        other => {
            // Read the wait off a borrow before the refusal is moved into the
            // rejection that carries it, so the two rate variants need no arm
            // that re-matches what the outer arm already proved.
            let rate_retry_after_ms = match &other {
                Refusal::OrderRate { retry_after_ms, .. }
                | Refusal::GlobalRateBudget { retry_after_ms, .. } => Some(*retry_after_ms),
                _ => None,
            };
            let rejection = match (rate_retry_after_ms, other) {
                (Some(retry_after_ms), refusal) => Rejection::RateLimited {
                    retry_after_ms,
                    refusal,
                },
                (None, Refusal::VenueRule(subtype)) => Rejection::VenueReject { subtype },
                (None, paused @ Refusal::TradingPaused { .. }) => {
                    Rejection::TradingPaused { refusal: paused }
                }
                (None, refusal) => Rejection::GuardrailReject { refusal },
            };
            Outcome::Rejected {
                retryable: rejection.retryable(),
                rejection,
            }
        }
    };
    Reply::new(outcome)
}

pub(crate) fn resting(oid: u64, cloid: Option<String>) -> Reply {
    Reply::new(Outcome::Resting { oid, cloid })
}

pub(crate) fn filled(
    oid: u64,
    cloid: Option<String>,
    filled_sz: Decimal,
    avg_px: Decimal,
) -> Reply {
    Reply::new(Outcome::Filled {
        oid,
        cloid,
        filled_sz,
        avg_px,
    })
}

pub(crate) fn canceled(requested: usize, failed: Vec<CancelFailure>) -> Reply {
    Reply::new(Outcome::Canceled {
        requested,
        canceled: requested - failed.len(),
        failed,
    })
}

/// The failures that are not outcomes of asking (`docs/spec.md` item 19).
///
/// These are protocol errors rather than statuses, and each carries its code
/// and retryability as structured data on the error rather than as prose.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ToolError {
    /// A durable predicate changed after evaluation but before signing.
    #[error("{refusal}")]
    GuardrailRefused { refusal: Refusal },
    /// The venue refused the signed request. The message is the venue's own,
    /// stored and rendered verbatim and **display-only** — nothing branches on
    /// it (`AGENTS.md` conventions). Split from `venue_reject`, which is
    /// oppen's own pre-sign catch, because the two mean different things to an
    /// agent and only one of them means a nonce was spent
    /// (`docs/decisions.md` C3).
    #[error("the venue refused the request (http {http_status}): {venue_message}")]
    VenueError {
        http_status: u16,
        venue_message: String,
    },

    /// The request left this process and no answer came back. The order may
    /// be live. Item 19: the only safe move is to query by cloid, never to
    /// resend — which is why `place` mints a cloid the agent can query with
    /// even when it did not supply one.
    #[error("the request was sent and its outcome is unknown; reconcile with get_order_status")]
    TimeoutUnknownOutcome {
        cloid: Option<String>,
        detail: String,
    },

    /// oppen could not read what it needed to answer. Nothing was signed.
    #[error("{what} is unavailable: {detail}")]
    Unavailable { what: &'static str, detail: String },

    /// A retained worker failed; its authority state requires controlled recovery.
    #[error("{what} failed: {detail}")]
    WorkerFailed { what: &'static str, detail: String },

    /// The agent's own input. Not retryable as sent.
    #[error("{field}: {detail}")]
    InvalidParams { field: &'static str, detail: String },
}

impl ToolError {
    fn code(&self) -> &'static str {
        match self {
            ToolError::GuardrailRefused { .. } => "guardrail_reject",
            ToolError::VenueError { .. } => "venue_error",
            ToolError::TimeoutUnknownOutcome { .. } => "timeout_unknown_outcome",
            ToolError::Unavailable { .. } => "unavailable",
            ToolError::WorkerFailed { .. } => "worker_failed",
            ToolError::InvalidParams { .. } => "invalid_params",
        }
    }

    /// A 429 or a 5xx is the venue asking for a moment; a 4xx is a request it
    /// will refuse identically forever. An unknown outcome is never
    /// retryable — that is the whole content of item 19's rule about it.
    fn retryable(&self) -> bool {
        match self {
            ToolError::VenueError { http_status, .. } => {
                *http_status == 429 || (500..600).contains(http_status)
            }
            ToolError::Unavailable { .. } => true,
            ToolError::TimeoutUnknownOutcome { .. }
            | ToolError::InvalidParams { .. }
            | ToolError::GuardrailRefused { .. } => false,
            ToolError::WorkerFailed { .. } => false,
        }
    }

    /// The venue was reached and answered. Kept separate from a transport
    /// failure because only one of them leaves the outcome unknown.
    pub(crate) fn venue(status: u16, message: String) -> Self {
        ToolError::VenueError {
            http_status: status,
            venue_message: message,
        }
    }

    pub(crate) fn unavailable(what: &'static str, e: impl std::fmt::Display) -> Self {
        ToolError::Unavailable {
            what,
            detail: e.to_string(),
        }
    }

    pub(crate) fn worker_failed(what: &'static str, error: tokio::task::JoinError) -> Self {
        Self::WorkerFailed {
            what,
            detail: error.to_string(),
        }
    }

    pub(crate) fn invalid(field: &'static str, detail: impl std::fmt::Display) -> Self {
        ToolError::InvalidParams {
            field,
            detail: detail.to_string(),
        }
    }
}

impl From<ToolError> for ErrorData {
    fn from(e: ToolError) -> Self {
        let mut data = serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "code": e.code(),
            "retryable": e.retryable(),
            "detail": match &e {
                ToolError::GuardrailRefused { refusal } => refusal.to_string(),
                ToolError::VenueError { venue_message, .. } => venue_message.clone(),
                ToolError::TimeoutUnknownOutcome { detail, .. }
                | ToolError::Unavailable { detail, .. }
                | ToolError::WorkerFailed { detail, .. }
                | ToolError::InvalidParams { detail, .. } => detail.clone(),
            },
            "cloid": match &e {
                ToolError::TimeoutUnknownOutcome { cloid, .. } => {
                    cloid.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null)
                }
                _ => serde_json::Value::Null,
            },
        });
        if let ToolError::GuardrailRefused { refusal } = &e {
            data["refusal"] = serde_json::to_value(refusal).expect("Refusal serializes");
        }
        // `invalid_params` is the caller's mistake and the JSON-RPC layer has
        // a code for it; everything else is oppen or the venue failing to
        // answer, which is what `internal_error` means. The taxonomy an agent
        // branches on is in `data`, not in the JSON-RPC code.
        match e {
            ToolError::InvalidParams { .. } => ErrorData::invalid_params(e.to_string(), Some(data)),
            _ => ErrorData::internal_error(e.to_string(), Some(data)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oppen_core::guardrail::{KillReason, KillScope};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    fn json(reply: Reply) -> String {
        serde_json::to_string(&reply).expect("Reply serializes")
    }

    #[tokio::test]
    async fn a_failed_worker_has_a_nonretryable_recovery_code() {
        let error = tokio::task::spawn_blocking(|| panic!("synthetic worker failure"))
            .await
            .unwrap_err();
        let data: ErrorData = ToolError::worker_failed("operator decision worker", error).into();
        let payload = data.data.unwrap();
        assert_eq!(payload["code"], "worker_failed");
        assert_eq!(payload["retryable"], false);
        assert!(
            payload["detail"]
                .as_str()
                .unwrap()
                .contains("synthetic worker failure")
        );
    }

    /// Invariant 6 is a claim about bytes, so it is asserted on bytes. A
    /// derive that starts emitting a different key order, a `Decimal` that
    /// starts serializing as a float, or a renamed tag all fail here.
    #[test]
    fn every_status_has_one_byte_sequence() {
        assert_eq!(
            json(resting(7, Some("0x01".to_owned()))),
            r#"{"contract_version":0,"status":"resting","oid":7,"cloid":"0x01"}"#
        );
        assert_eq!(
            json(filled(7, None, d("1.5"), d("64000.5"))),
            r#"{"contract_version":0,"status":"filled","oid":7,"cloid":null,"filled_sz":"1.5","avg_px":"64000.5"}"#
        );
        assert_eq!(
            json(canceled(2, vec![])),
            r#"{"contract_version":0,"status":"canceled","requested":2,"canceled":2,"failed":[]}"#
        );
    }

    /// A `Decimal` written as a float would round `64000.5` differently on a
    /// different platform and silently change a price. `serde-with-str` is
    /// what stops that, and it is a feature flag someone can drop.
    #[test]
    fn decimals_are_strings_not_floats() {
        let body = json(filled(1, None, d("0.000000001"), d("100000.000000001")));
        assert!(body.contains(r#""filled_sz":"0.000000001""#), "{body}");
        assert!(body.contains(r#""avg_px":"100000.000000001""#), "{body}");
    }

    #[test]
    fn account_open_exposure_refusal_is_typed_and_not_retryable() {
        assert_eq!(
            json(refused(Refusal::OpenExposure {
                observed_usd: d("26"),
                limit_usd: d("25"),
            })),
            r#"{"contract_version":0,"status":"rejected","retryable":false,"code":"guardrail_reject","refusal":{"refusal":"open_exposure","observed_usd":"26","limit_usd":"25"}}"#
        );
    }

    #[test]
    fn a_partial_cancel_reports_the_count_and_names_each_failure() {
        let body = json(canceled(
            3,
            vec![CancelFailure {
                oid: Some(9),
                cloid: None,
                venue_message: "Order was never placed, already canceled, or filled.".to_owned(),
            }],
        ));
        assert_eq!(
            body,
            r#"{"contract_version":0,"status":"canceled","requested":3,"canceled":2,"failed":[{"oid":9,"cloid":null,"venue_message":"Order was never placed, already canceled, or filled."}]}"#
        );
    }

    /// The mapping `Refusal`'s own documentation states. A new refusal variant
    /// that should not be a plain `guardrail_reject` has to be added here as
    /// well as there.
    #[test]
    fn a_venue_rule_is_a_venue_reject_carrying_its_subtype() {
        let body = json(refused(Refusal::VenueRule(VenueRule::MinNotional {
            notional_usd: d("4"),
            minimum_usd: d("10"),
        })));
        assert_eq!(
            body,
            r#"{"contract_version":0,"status":"rejected","retryable":false,"code":"venue_reject","subtype":{"venue_rule":"min_notional","notional_usd":"4","minimum_usd":"10"}}"#
        );
    }

    #[test]
    fn the_kill_switch_is_trading_paused_not_a_guardrail_reject() {
        let body = json(refused(Refusal::TradingPaused {
            scope: KillScope::Global,
            since_ms: 1_700_000_000_000,
            reason: KillReason::Operator,
        }));
        assert!(body.contains(r#""code":"trading_paused""#), "{body}");
        assert!(body.contains(r#""retryable":false"#), "{body}");
    }

    /// The one refinement this module makes to that mapping, and the reason
    /// it is worth making: a rate refusal is the only one an agent clears by
    /// waiting, and it says how long.
    #[test]
    fn a_rate_refusal_is_retryable_and_says_when() {
        let body = json(refused(Refusal::OrderRate {
            limit: 6,
            window_ms: 60_000,
            tokens_available: Decimal::ZERO,
            retry_after_ms: 10_000,
        }));
        assert!(body.contains(r#""code":"rate_limited""#), "{body}");
        assert!(body.contains(r#""retryable":true"#), "{body}");
        assert!(body.contains(r#""retry_after_ms":10000"#), "{body}");
    }

    /// The global budget is a different predicate with the same remedy, so it
    /// carries the same code — and the refusal it wraps still names which one
    /// tripped, because the operator's fix differs.
    #[test]
    fn the_global_budget_is_also_rate_limited_and_still_names_itself() {
        let body = json(refused(Refusal::GlobalRateBudget {
            tokens_available: d("2"),
            reserve: 5,
            retry_after_ms: 750,
        }));
        assert!(body.contains(r#""code":"rate_limited""#), "{body}");
        assert!(body.contains(r#""retry_after_ms":750"#), "{body}");
        assert!(body.contains(r#""refusal":"global_rate_budget""#), "{body}");
    }

    /// A breached limit is not retryable: the same order will be refused
    /// identically until the operator raises the cap or the agent sends less.
    #[test]
    fn a_breached_limit_is_a_guardrail_reject_and_is_not_retryable() {
        let body = json(refused(Refusal::SymbolNotAllowed {
            symbol: "DOGE".to_owned(),
            allowed: vec!["BTC".to_owned()],
        }));
        assert!(body.contains(r#""code":"guardrail_reject""#), "{body}");
        assert!(body.contains(r#""retryable":false"#), "{body}");
        assert!(body.contains(r#""symbol":"DOGE""#), "{body}");
    }

    /// Item 28's status, not a rejection. The distinction is load-bearing: an
    /// agent that reads `pending_approval` as a refusal gives up on an order a
    /// human is about to approve.
    #[test]
    fn approval_required_is_a_status_and_not_a_rejection() {
        let body = json(refused(Refusal::ApprovalRequired {
            symbol: "BTC".to_owned(),
            notional_usd: d("500"),
            approval_id: "prop-1".to_owned(),
            expires_at_ms: 1_700_000_060_000,
        }));
        assert_eq!(
            body,
            r#"{"contract_version":0,"status":"pending_approval","approval_id":"prop-1","symbol":"BTC","notional_usd":"500","expires_at_ms":1700000060000}"#
        );
        assert!(!body.contains("rejected"), "{body}");
    }

    /// Invariant 8 on the error half: the taxonomy is in `data`, where a
    /// caller can branch on it, not only in the sentence.
    #[test]
    fn a_tool_error_carries_its_code_and_retryability_as_data() {
        let data = ErrorData::from(ToolError::venue(429, "too many requests".to_owned()))
            .data
            .expect("typed data");
        assert_eq!(data["code"], "venue_error");
        assert_eq!(data["retryable"], true);
        assert_eq!(data["detail"], "too many requests");
    }

    /// The rule item 19 states outright. A retryable unknown outcome is how an
    /// agent doubles a position.
    #[test]
    fn an_unknown_outcome_is_never_retryable_and_carries_the_cloid_to_query() {
        let data = ErrorData::from(ToolError::TimeoutUnknownOutcome {
            cloid: Some("0xabc".to_owned()),
            detail: "connection reset".to_owned(),
        })
        .data
        .expect("typed data");
        assert_eq!(data["code"], "timeout_unknown_outcome");
        assert_eq!(data["retryable"], false);
        assert_eq!(data["cloid"], "0xabc");
    }

    /// A 4xx that is not a 429 is the venue saying the request itself is
    /// wrong; resending it unchanged asks the same question again.
    #[test]
    fn venue_retryability_follows_the_http_class() {
        let retryable = |status| {
            ErrorData::from(ToolError::venue(status, "x".to_owned()))
                .data
                .expect("data")["retryable"]
                == true
        };
        assert!(retryable(429));
        assert!(retryable(503));
        assert!(!retryable(422));
        assert!(!retryable(400));
    }

    #[test]
    fn an_unreadable_venue_is_retryable_and_a_bad_parameter_is_not() {
        let data = ErrorData::from(ToolError::unavailable("meta", "connection refused"))
            .data
            .expect("data");
        assert_eq!(data["code"], "unavailable");
        assert_eq!(data["retryable"], true);

        let data = ErrorData::from(ToolError::invalid("size", "not a decimal"))
            .data
            .expect("data");
        assert_eq!(data["code"], "invalid_params");
        assert_eq!(data["retryable"], false);
    }
}
