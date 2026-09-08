//! Operator-only reuse of the gateway's account, context and submission path.

use oppen_core::guardrail::{
    ApprovalReview, ApprovalReviewDisplay, CancelContext, GuardrailEngine,
};

use crate::server::OperatorWork;

use super::{
    Gateway, Reply, ToolError, cancel_outcome, cancellation_context, now_ms, order_outcome, outcome,
};

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
        if let Some(intent) = proposal.cancel_intent() {
            let context = self.operator_cancel_context(work, &intent.targets).await?;
            return self
                .operator_decision(work, move |engine| {
                    engine.operator_prepare_cancel_proposal(proposal.id(), &context, now_ms())
                })
                .await?
                .map_err(|refusal| ToolError::GuardrailRefused { refusal });
        }
        let symbol = proposal
            .order_intent()
            .ok_or_else(|| ToolError::invalid("proposal_id", "proposal is not an order"))?
            .symbol
            .clone();
        let context = self
            .evaluation_context_tracked(binding, &symbol, now_ms(), Some(work.tracker.clone()))
            .await?;
        let asset = context
            .universe
            .get(&symbol)
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
        if matches!(review.display(), ApprovalReviewDisplay::Cancel(_)) {
            return self.operator_confirm_cancel(work, review).await;
        }
        let symbol = review
            .symbol()
            .ok_or_else(|| ToolError::invalid("review", "order review has no symbol"))?
            .to_owned();
        work.check_live()?;
        let permit = self
            .reserve_submission_tracked(binding, Some(work.tracker.clone()))
            .await?;
        let context = self
            .evaluation_context_tracked(binding, &symbol, now_ms(), Some(work.tracker.clone()))
            .await?;
        let asset = context
            .universe
            .get(&symbol)
            .map_err(|error| ToolError::invalid("symbol", error))?
            .clone();
        let cloid = match review.display() {
            ApprovalReviewDisplay::Order(display) => display.cloid.clone(),
            ApprovalReviewDisplay::Cancel(_) => {
                return Err(ToolError::invalid("review", "expected order review"));
            }
        };
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

    async fn operator_cancel_context(
        &self,
        work: &OperatorWork,
        targets: &[super::CancelTarget],
    ) -> Result<CancelContext, ToolError> {
        work.check_live()?;
        self.require_route_tracked(work.binding(), Some(work.tracker.clone()))
            .await?;
        let universe = self.universe().await?;
        let orders = self
            .inner
            .info
            .frontend_open_orders(work.binding().account)
            .await
            .map_err(|error| ToolError::unavailable("cancellation targets", error))?;
        let orders: Vec<_> = orders
            .into_iter()
            .filter(|order| targets.iter().any(|target| target.oid == order.oid))
            .collect();
        cancellation_context(work.binding(), &orders, &universe, now_ms())
    }

    async fn operator_confirm_cancel(
        &self,
        work: &OperatorWork,
        review: ApprovalReview,
    ) -> Result<Reply, ToolError> {
        let targets = match review.display() {
            ApprovalReviewDisplay::Cancel(display) => display.targets.clone(),
            ApprovalReviewDisplay::Order(_) => {
                return Err(ToolError::invalid("review", "expected cancellation review"));
            }
        };
        let binding = work.binding();
        work.check_live()?;
        let permit = self.execution_queue(binding.account).lock_owned().await;
        let context = self.operator_cancel_context(work, &targets).await?;
        // Retain the account queue in the actual decision worker, not its observer.
        let (decision, _permit) = self
            .operator_decision(work, move |engine| {
                let decision = engine.operator_confirm_cancel_review(review, &context, now_ms());
                (decision, permit)
            })
            .await?;
        let cleared = match decision {
            Ok(cleared) => cleared,
            Err(refusal) => return Ok(outcome::refused(refusal)),
        };
        let response = match self
            .submit_authorized(cleared, None, binding, None, Some(work))
            .await
        {
            Ok(response) => response,
            Err(ToolError::GuardrailRefused { refusal }) => return Ok(outcome::refused(refusal)),
            Err(error) => return Err(error),
        };
        cancel_outcome(
            response,
            targets
                .into_iter()
                .map(|target| {
                    (
                        Some(target.oid),
                        target.cloid.map(|cloid| cloid.as_str().to_owned()),
                    )
                })
                .collect(),
        )
    }
}
