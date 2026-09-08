// Local visual fixtures only; no native command, signing key or venue access.
import type { ApprovalQueueStatus } from "../src/lib/bridge";

const scenario = new URLSearchParams(location.search).get("approvals") ?? "ready";
const account = "0x0000000000000000000000000000000000000001";
const observed = Date.now();
let status: ApprovalQueueStatus = {
  owner_id: "fixture-queue-1", agent: "fixture-agent", account, phase: "ready",
  observed_at_ms: observed, error: null, decision: null, review: null, confirmation: null,
  pending: [
    { id: "proposal-1", agent: "fixture-agent", account, symbol: "BTC", is_buy: true,
      px: "60250.123456", sz: "0.0002", reduce_only: false,
      reason: "Measured entry; <img src=x onerror=alert(1)> remains plain text.",
      expires_at_ms: observed + 120_000,
      original: { kind: { kind: "market", slippage_bps: "10" }, reference_px: "60190", reference_at_ms: observed - 100 } },
    { id: "proposal-2", agent: "fixture-agent", account, symbol: "LONG-MARKET-NAME", is_buy: false,
      px: "123456789012345.123456789", sz: "0.000000000012345678", reduce_only: true,
      reason: "Retained-position-review/".repeat(12), expires_at_ms: observed + 120_000,
      original: { kind: { kind: "close_position", position_size: "0.000000000012345678", slippage_bps: "5" }, reference_px: "123456789012345.234567890", reference_at_ms: observed - 200 } },
    { id: "proposal-3", agent: "fixture-agent", account, symbol: "ETH", is_buy: true,
      px: "3000", sz: "0.004", reduce_only: false, reason: "Historical normalized request.",
      expires_at_ms: observed + 120_000, original: null },
  ],
};
if (scenario === "empty") status.pending = [];
if (scenario === "unavailable" || scenario === "recovery_required") {
  status.phase = scenario;
  status.error = "Retained authority observation is unavailable. " + "approval-ledger-diagnostic/".repeat(12);
}
if (scenario === "busy") status.phase = "rejecting";
function scoped(agent: string, requestedAccount: string): void {
  if (agent !== status.agent || requestedAccount !== status.account) throw { detail: "Fixture owner differs from the requested binding." };
}
export async function fetchApprovalQueueStatus(agent: string, requestedAccount: string): Promise<ApprovalQueueStatus> {
  scoped(agent, requestedAccount);
  return structuredClone(status);
}
export async function refreshApprovalQueue(agent: string, requestedAccount: string): Promise<ApprovalQueueStatus> {
  scoped(agent, requestedAccount);
  if (scenario !== "recovery_required" && scenario !== "busy") {
    status.phase = "ready";
    status.error = null;
    status.observed_at_ms = Date.now();
  }
  return structuredClone(status);
}
export async function rejectApprovalProposal(agent: string, requestedAccount: string, ownerId: string, proposalId: string): Promise<ApprovalQueueStatus> {
  scoped(agent, requestedAccount);
  if (ownerId !== status.owner_id) throw { detail: "Fixture queue owner changed." };
  if (scenario === "uncertain") {
    status.phase = "unavailable";
    status.error = "Rejection publication is uncertain. " + "retained-decision-evidence/".repeat(12);
    status.decision = { proposal_id: proposalId, outcome: "uncertain", at_ms: Date.now(), error: status.error };
  } else {
    const exists = status.pending.some(proposal => proposal.id === proposalId);
    status.pending = status.pending.filter(proposal => proposal.id !== proposalId);
    status.decision = { proposal_id: proposalId, outcome: exists ? "rejected" : "not_pending", at_ms: Date.now(), error: null };
    status.phase = "ready";
    status.observed_at_ms = Date.now();
  }
  return structuredClone(status);
}

export async function prepareApprovalReview(agent: string, requestedAccount: string, ownerId: string, proposalId: string): Promise<ApprovalQueueStatus> {
  scoped(agent, requestedAccount);
  if (ownerId !== status.owner_id || status.review) throw new Error("Fixture review unavailable.");
  const proposal = status.pending.find(row => row.id === proposalId);
  if (!proposal) throw new Error("Fixture proposal absent.");
  status.review = {
    id: "fixture-review-1", owner_id: ownerId, pairing_id: { network: "testnet", issued_seq: 3 }, reason: proposal.reason,
    display: { ...proposal, proposal_id: proposal.id, original_px: proposal.px,
      reference_px: "60200", reference_at_ms: Date.now(), drift_bps: "1.661",
      asset_index: 0, notional_usd: "12.05", order_type: { limit: { tif: "Ioc" } },
      cloid: "0x00000000000000000000000000000001", grouping: "na", builder: null,
      route: { network: "testnet", binding_seq: 2, binding: { agent, container: account, vault_address: null,
        wallet: { generation: 1, address: "0x0000000000000000000000000000000000000002", approved_at_ms: observed, valid_until_ms: observed + 300_000 } } },
      policy_revision: 4, policy_hash: "a".repeat(64), reviewed_at_ms: Date.now(), expires_at_ms: observed + 120_000 },
  };
  status.phase = "review_ready";
  return structuredClone(status);
}
export async function confirmApprovalReview(agent: string, requestedAccount: string, ownerId: string, reviewId: string): Promise<ApprovalQueueStatus> {
  scoped(agent, requestedAccount);
  if (ownerId !== status.owner_id || status.review?.id !== reviewId || status.phase !== "review_ready") throw new Error("Fixture review unavailable.");
  status.confirmation = { review_id: reviewId, proposal_id: status.review.display.proposal_id, at_ms: Date.now(),
    result: { status: "rejected", cloid: status.review.display.cloid, detail: "Synthetic fixture refusal; no submission." }, error: null };
  status.phase = "idle";
  status.observed_at_ms = null;
  return structuredClone(status);
}
export async function discardApprovalReview(agent: string, requestedAccount: string, ownerId: string, reviewId: string): Promise<ApprovalQueueStatus> {
  scoped(agent, requestedAccount);
  if (ownerId !== status.owner_id || status.review?.id !== reviewId || status.phase !== "review_ready") throw new Error("Fixture review unavailable.");
  status.review = null;
  status.phase = "ready";
  return structuredClone(status);
}
