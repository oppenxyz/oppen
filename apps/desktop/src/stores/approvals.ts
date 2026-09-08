import { reactive, readonly } from "vue";
import {
  fetchApprovalQueueStatus, refreshApprovalQueue, rejectApprovalProposal, inTauri,
  prepareApprovalReview, confirmApprovalReview, discardApprovalReview,
  type ApprovalConfirmation,
  type ApprovalDecision, type ApprovalQueueStatus, type McpStatus, type PendingApprovalView,
  type RuntimeStatus,
} from "../lib/bridge";

export interface ApprovalBinding { agent: string; account: string }

/** The roster is deliberately not an input to live queue authority. */
export function approvalBinding(input: {
  network: "testnet" | "mainnet"; supervision: Readonly<McpStatus> | null;
  runtime: Readonly<RuntimeStatus> | null; supervisionError: string | null;
  runtimeError: string | null; stopping: boolean;
}): ApprovalBinding | null {
  const { supervision, runtime } = input;
  if (input.network !== "testnet" || input.stopping || input.supervisionError !== null
    || input.runtimeError !== null || runtime?.phase !== "running"
    || (runtime.binding !== null && runtime.binding.network !== input.network)
    || supervision?.phase !== "listening" || supervision.network !== input.network
    || !supervision.agent || !supervision.account) return null;
  return { agent: supervision.agent, account: supervision.account };
}

function after(callback: () => void, ms: number): () => void {
  const timer = setTimeout(callback, ms);
  return () => clearTimeout(timer);
}
function detail(error: unknown): string {
  if (typeof error === "object" && error !== null && "detail" in error && typeof error.detail === "string") return error.detail;
  return error instanceof Error ? error.message : String(error);
}
function sameBinding(a: ApprovalBinding | null, b: ApprovalBinding | null): boolean {
  return a?.agent === b?.agent && a?.account === b?.account;
}
function terminal(status: ApprovalQueueStatus | null): boolean {
  return status?.phase === "closed" || status?.phase === "recovery_required";
}

export function createApprovals(
  transport: { status: typeof fetchApprovalQueueStatus; refresh: typeof refreshApprovalQueue; reject: typeof rejectApprovalProposal;
    prepare?: typeof prepareApprovalReview; confirm?: typeof confirmApprovalReview; discard?: typeof discardApprovalReview },
  available: () => boolean = () => true,
  schedule: typeof after = after,
  now: () => number = Date.now,
) {
  const state = reactive<{
    binding: ApprovalBinding | null; status: ApprovalQueueStatus | null; error: string | null;
    pending: "status" | "refresh" | "reject" | "prepare" | "confirm" | "discard" | null; decision: ApprovalDecision | null;
    execution: ApprovalConfirmation | null;
    confirmation: { owner_id: string; proposal: PendingApprovalView } | null;
  }>({ binding: null, status: null, error: null, pending: null, decision: null, confirmation: null, execution: null });
  const attempted = reactive(new Set<string>());
  const confirmed = reactive(new Set<string>());
  let active = false;
  let epoch = 0;
  let busy = false;
  let cancelPoll: (() => void) | null = null;
  let refreshProof: { owner: string; observed: number | null } | null = null;
  let decisionOwner: string | null = null;
  let executionOwner: string | null = null;

  function setBinding(binding: ApprovalBinding | null): void {
    if (sameBinding(binding, state.binding)) return;
    epoch += 1;
    state.binding = binding ? { ...binding } : null;
    state.status = null;
    state.decision = null;
    decisionOwner = null;
    state.execution = null;
    executionOwner = null;
    state.confirmation = null;
    refreshProof = null;
    state.error = binding && busy ? "Waiting for the earlier approval request to finish." : null;
    if (active) void readStatus();
  }

  function canRefresh(): boolean {
    return active && available() && state.binding !== null && !busy && !terminal(state.status)
      && (state.status === null || ["idle", "ready", "unavailable"].includes(state.status.phase));
  }

  function canReject(id: string): boolean {
    const status = state.status;
    return active && available() && !busy && state.error === null && status?.phase === "ready"
      && status.error === null && status.observed_at_ms !== null
      && !attempted.has(`${status.owner_id}:${id}`)
      && status.pending.some(proposal => proposal.id === id && proposal.expires_at_ms > now());
  }

  function prepareReject(id: string): void {
    if (!canReject(id)) return;
    const status = state.status!;
    state.confirmation = { owner_id: status.owner_id, proposal: { ...status.pending.find(p => p.id === id)! } };
  }
  function cancelReject(): void { state.confirmation = null; }

  function canPrepare(id: string): boolean {
    return active && available() && typeof transport.prepare === "function" && !busy && state.error === null
      && state.status?.phase === "ready" && state.status.error === null
      && state.status.observed_at_ms !== null
      && state.status.pending.some(proposal => proposal.id === id && proposal.expires_at_ms > now());
  }
  function canConfirm(): boolean {
    const review = state.status?.review;
    return active && available() && typeof transport.confirm === "function" && !busy && state.error === null
      && state.status?.phase === "review_ready" && state.status.error === null && review != null
      && review.owner_id === state.status.owner_id && sameBinding(review.display, state.binding)
      && review.pairing_id.network === "testnet" && review.display.expires_at_ms > now()
      && !confirmed.has(`${review.owner_id}:${review.id}`);
  }
  function canDiscard(): boolean {
    return active && available() && typeof transport.discard === "function" && !busy
      && state.status?.phase === "review_ready" && state.status.review != null
      && state.status.review.owner_id === state.status.owner_id && sameBinding(state.status.review.display, state.binding);
  }

  async function request(kind: "status" | "refresh" | "reject" | "prepare" | "confirm" | "discard", proposalId?: string): Promise<void> {
    if (!active || !available() || busy || state.binding === null) return;
    const binding = { ...state.binding };
    const generation = epoch;
    const owner = state.status?.owner_id ?? null;
    const reviewedProposal = state.status?.review?.display.proposal_id;
    if (kind === "refresh" && owner !== null) refreshProof = { owner, observed: state.status?.observed_at_ms ?? null };
    let expired = false;
    busy = true;
    state.pending = kind;
    const current = () => active && generation === epoch && sameBinding(binding, state.binding);
    const uncertain = (message: string) => {
      state.error = message;
      state.confirmation = null;
      if (kind === "reject" && proposalId) {
        state.decision = { proposal_id: proposalId, outcome: "uncertain", at_ms: now(), error: message };
        decisionOwner = owner;
      }
      if (kind === "confirm" && proposalId && reviewedProposal) {
        state.execution = { review_id: proposalId, proposal_id: reviewedProposal, at_ms: now(), result: null, error: { message, code: "confirmation_unconfirmed" } };
        executionOwner = owner;
      }
    };
    const cancelDeadline = schedule(() => {
      expired = true;
      if (current() && kind === "refresh") refreshProof = null;
      if (current()) uncertain(kind === "reject" || kind === "confirm"
        ? "Decision has not responded within 5 seconds. Outcome unconfirmed."
        : "Approval queue has not responded within 5 seconds.");
    }, 5_000);
    try {
      const status = await (kind === "status" ? transport.status(binding.agent, binding.account)
        : kind === "refresh" ? transport.refresh(binding.agent, binding.account)
        : kind === "prepare" ? transport.prepare!(binding.agent, binding.account, owner!, proposalId!)
        : kind === "confirm" ? transport.confirm!(binding.agent, binding.account, owner!, proposalId!)
        : kind === "discard" ? transport.discard!(binding.agent, binding.account, owner!, proposalId!)
        : transport.reject(binding.agent, binding.account, owner!, proposalId!));
      if (!current() || expired) return;
      if (!sameBinding(status, binding) || !status.owner_id
        || status.pending.some(p => !sameBinding(p, binding))
        || (status.review != null && (status.review.owner_id !== status.owner_id
          || !sameBinding(status.review.display, binding) || status.review.pairing_id.network !== "testnet"))) {
        state.status = null;
        refreshProof = null;
        uncertain("Approval queue identity did not match the current runtime binding.");
        return;
      }
      const previousOwner = owner ?? decisionOwner ?? executionOwner;
      if (previousOwner !== null && status.owner_id !== previousOwner) {
        state.confirmation = null;
        state.decision = null;
        decisionOwner = null;
        state.execution = null;
        executionOwner = null;
        state.status = null;
        refreshProof = null;
        epoch += 1;
        if (kind !== "status") {
          state.error = "Approval queue owner changed. Awaiting current status.";
          return;
        }
      }
      state.status = status;
      state.error = status.error;
      // A cached decision for another proposal cannot resolve a lost rejection reply.
      if (status.decision !== null && (state.decision?.outcome !== "uncertain"
        || (decisionOwner === status.owner_id && state.decision.proposal_id === status.decision.proposal_id))) {
        state.decision = status.decision;
        decisionOwner = status.owner_id;
      }
      if (status.confirmation != null && (state.execution === null
        || (executionOwner === status.owner_id && state.execution.review_id === status.confirmation.review_id
          && state.execution.proposal_id === status.confirmation.proposal_id))) {
        // A merely admitted confirmation is not evidence resolving an IPC failure.
        if (state.execution?.error == null || status.confirmation.result != null || status.confirmation.error != null) {
          state.execution = status.confirmation;
          executionOwner = status.owner_id;
        }
      }
      if (refreshProof?.owner === status.owner_id && status.phase === "ready"
        && status.error === null && status.observed_at_ms !== null
        && (refreshProof.observed === null || status.observed_at_ms > refreshProof.observed)) {
        for (const proposal of status.pending) attempted.delete(`${status.owner_id}:${proposal.id}`);
        refreshProof = null;
      } else if (status.phase === "unavailable" || terminal(status)) refreshProof = null;
      const confirmation = state.confirmation;
      if (confirmation && (status.phase !== "ready" || status.error !== null
        || status.owner_id !== confirmation.owner_id
        || !status.pending.some(p => JSON.stringify(p) === JSON.stringify(confirmation.proposal)))) {
        state.confirmation = null;
      }
    } catch (error) {
      if (current() && !expired) {
        if (kind === "refresh") refreshProof = null;
        uncertain(detail(error));
      }
    } finally {
      cancelDeadline();
      busy = false;
      state.pending = null;
      // Never retry a mutation. Context changes and timed-out replies need only a cached read.
      if (active && (generation !== epoch || expired)) void readStatus();
    }
  }

  async function readStatus(): Promise<void> { await request("status"); }
  async function refresh(): Promise<void> {
    if (canRefresh()) { state.confirmation = null; await request("refresh"); }
  }
  async function reject(): Promise<void> {
    const confirmation = state.confirmation;
    if (!confirmation || !canReject(confirmation.proposal.id)
      || confirmation.owner_id !== state.status?.owner_id
      || !sameBinding(confirmation.proposal, state.binding)) return;
    attempted.add(`${confirmation.owner_id}:${confirmation.proposal.id}`);
    refreshProof = null;
    state.confirmation = null;
    state.decision = null;
    decisionOwner = null;
    await request("reject", confirmation.proposal.id);
  }
  async function prepare(id: string): Promise<void> {
    if (!canPrepare(id)) return;
    state.confirmation = null;
    await request("prepare", id);
  }
  async function confirm(): Promise<void> {
    if (!canConfirm()) return;
    const review = state.status!.review!;
    confirmed.add(`${review.owner_id}:${review.id}`);
    state.execution = { review_id: review.id, proposal_id: review.display.proposal_id, at_ms: now(), result: null, error: null };
    executionOwner = review.owner_id;
    await request("confirm", review.id);
  }
  async function discard(): Promise<void> {
    if (canDiscard()) await request("discard", state.status!.review!.id);
  }
  function poll(): void {
    cancelPoll = schedule(() => {
      if (!active) return;
      void readStatus();
      poll();
    }, 1_000);
  }
  function start(): void {
    if (active || !available()) return;
    active = true;
    epoch += 1;
    void readStatus();
    poll();
  }
  function stop(): void {
    active = false;
    epoch += 1;
    cancelPoll?.();
    cancelPoll = null;
    state.binding = null;
    state.status = null;
    state.confirmation = null;
    state.decision = null;
    decisionOwner = null;
    state.execution = null;
    executionOwner = null;
    state.error = null;
    refreshProof = null;
    // The real IPC and its deadline remain owned until it settles.
  }
  return { state: readonly(state), setBinding, start, stop, readStatus, refresh, prepareReject, cancelReject, reject, canReject, canRefresh,
    prepare, confirm, discard, canPrepare, canConfirm, canDiscard };
}

export const approvals = createApprovals({ status: fetchApprovalQueueStatus, refresh: refreshApprovalQueue, reject: rejectApprovalProposal,
  prepare: prepareApprovalReview, confirm: confirmApprovalReview, discard: discardApprovalReview }, inTauri);

export const APPROVAL_PHASES: Record<ApprovalQueueStatus["phase"], string> = {
  idle: "Not refreshed", refreshing: "Refreshing", rejecting: "Rejecting", ready: "Observed",
  unavailable: "Unavailable", recovery_required: "Recovery required", closed: "Closed",
  reviewing: "Preparing review", review_ready: "Review retained", confirming: "Confirming",
};
export const APPROVAL_DECISIONS: Record<ApprovalDecision["outcome"], string> = {
  rejected: "Rejection recorded", not_pending: "Not pending; no rejection recorded", uncertain: "Rejection outcome unconfirmed",
};
