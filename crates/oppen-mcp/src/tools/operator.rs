//! Operator-only reuse of the gateway's account, context and submission path.

use oppen_core::guardrail::{ApprovalReview, GuardrailEngine};

use crate::server::OperatorWork;

use super::{Gateway, Reply, ToolError, now_ms, order_outcome, outcome};

impl Gateway {
    // The native controller owns this entire future. Blocking work retains the
    // server tracker and semaphore even if an observing IPC waiter disappears.
    async fn operator_decision<T: Send + 'static>(
        &self,
        work: &OperatorWork,
        decision: impl FnOnce(std::sync::Arc<GuardrailEngine>) -> T + Send + 'static,
    ) -> Result<T, ToolError> {
        work.check_live()?;
        let permit = self
            .inner
            .decision_worker
            .clone()
            .try_acquire_owned()
            .map_err(|error| ToolError::unavailable("decision worker busy", error))?;
        let tracker = work.tracker.clone();
        let engine = self.inner.engine.clone();
        tokio::task::spawn_blocking(move || {
            let _tracker = tracker;
            let _permit = permit;
            decision(engine)
        })
        .await
        .map_err(|error| ToolError::worker_failed("operator decision worker", error))
    }

    pub(crate) async fn operator_prepare(
        &self,
        work: &OperatorWork,
        proposal_id: &str,
    ) -> Result<ApprovalReview, ToolError> {
        let binding = work.binding();
        self.require_route_tracked(binding, Some(work.tracker.clone()))
            .await?;
        let id = proposal_id.to_owned();
        let binding_copy = binding.clone();
        let proposal = self
            .operator_decision(work, move |engine| {
                engine
                    .pending_proposals(now_ms())
                    .map_err(|error| ToolError::unavailable("approval queue", error))?
                    .into_iter()
                    .find(|proposal| {
                        proposal.id() == id
                            && proposal.agent() == &binding_copy.agent
                            && proposal.account() == binding_copy.account
                    })
                    .ok_or_else(|| {
                        ToolError::invalid("proposal_id", "no pending proposal for this binding")
                    })
            })
            .await??;
        let context = self
            .evaluation_context_tracked(
                binding,
                &proposal.intent().symbol,
                now_ms(),
                Some(work.tracker.clone()),
            )
            .await?;
        let asset = context
            .universe
            .get(&proposal.intent().symbol)
            .map_err(|error| ToolError::invalid("symbol", error))?
            .clone();
        self.operator_decision(work, move |engine| {
            engine.operator_prepare_proposal(
                proposal.id(),
                &asset,
                &context.market,
                &context.exposure,
                now_ms(),
            )
        })
        .await?
        .map_err(|refusal| ToolError::GuardrailRefused { refusal })
    }

    pub(crate) async fn operator_confirm(
        &self,
        work: &OperatorWork,
        review: ApprovalReview,
    ) -> Result<Reply, ToolError> {
        let binding = work.binding();
        if review.agent() != &binding.agent || review.account() != binding.account {
            return Err(ToolError::invalid(
                "review",
                "review and pinned pairing differ",
            ));
        }
        work.check_live()?;
        let permit = self
            .reserve_submission_tracked(binding, Some(work.tracker.clone()))
            .await?;
        let context = self
            .evaluation_context_tracked(
                binding,
                review.symbol(),
                now_ms(),
                Some(work.tracker.clone()),
            )
            .await?;
        let asset = context
            .universe
            .get(review.symbol())
            .map_err(|error| ToolError::invalid("symbol", error))?
            .clone();
        let cloid = review.display().cloid.clone();
        // Move the reservation into the real blocking decision, not its waiter.
        let (decision, permit) = self
            .operator_decision(work, move |engine| {
                let decision = engine.operator_confirm_review(
                    review,
                    &asset,
                    &context.market,
                    &context.exposure,
                    now_ms(),
                );
                (decision, permit)
            })
            .await?;
        let cleared = match decision {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal)),
        };
        let response = match self
            .submit_authorized(cleared, Some(&cloid), binding, Some(&permit), Some(work))
            .await
        {
            Ok(response) => response,
            Err(ToolError::GuardrailRefused { refusal }) => return Ok(outcome::refused(refusal)),
            Err(error) => return Err(error),
        };
        order_outcome(response, Some(cloid.as_str().to_owned()))
    }
}
