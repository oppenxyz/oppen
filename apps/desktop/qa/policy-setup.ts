// Synthetic visual fixtures only. No native invocation or authority persistence.
import type { PolicySetupEdits, PolicySetupStatus, ReviewedAgentPolicy, ReviewedPolicy } from "../src/lib/bridge";

const at = 1_788_998_400_000;
const account = "0x0000000000000000000000000000000000000001";
const config: ReviewedAgentPolicy = {
  symbols: ["BTC"], max_order_usd: "10", max_position_usd: "20", max_slippage_bps: "50",
  order_rate: { count: 5, per_ms: 60000 }, reduce_only: false, approval_required: true,
  risk: { max_leverage: 1, margin_mode: "cross", max_open_exposure_usd: "20", max_risk_usd: null },
  loss: { max_daily_loss_usd: "5", max_drawdown_usd: null },
  freshness: { max_market_age_ms: 5000, max_account_age_ms: 10000 },
  max_mark_divergence_bps: "50", mark_divergence_window_ms: 30000,
};
const before: ReviewedPolicy = {
  guardrails: { "fixture-agent": config, "retained-agent": { ...config, symbols: ["ETH"], reduce_only: true } },
  account_limits: { max_daily_loss_usd: "5", max_drawdown_usd: "10" },
  kill: { global: { engaged_at_ms: at, reason: { reason: "operator" } },
    agents: { "retained-agent": { engaged_at_ms: at - 1000, reason: { reason: "operator" } } } },
};
let status: PolicySetupStatus = { phase: "idle", review: null, receipt_revision: null, error: null };
let serial = 0;

export async function fetchPolicySetupStatus(): Promise<PolicySetupStatus> { return structuredClone(status); }
export async function reviewPolicySetup(agent: string, requestedAccount: string, edits: PolicySetupEdits, _empty: boolean, writers: boolean): Promise<PolicySetupStatus> {
  if (!writers) throw new Error("UI fixture: stopped-writer assertion required.");
  const proposed = structuredClone(before);
  proposed.guardrails[agent] = { ...structuredClone(config), symbols: [...edits.symbols],
    max_order_usd: edits.max_order_usd, max_position_usd: edits.max_position_usd, approval_required: edits.approval_required,
    risk: { ...config.risk, max_open_exposure_usd: edits.max_open_exposure_usd, max_leverage: edits.max_leverage } };
  status = { phase: "review_ready", receipt_revision: null, error: null, review: {
    id: ++serial, agent, account: requestedAccount,
    route: { network: "testnet", binding_seq: 2, binding: { agent, container: requestedAccount, vault_address: null,
      wallet: { generation: 0, address: "0x0000000000000000000000000000000000000002", approved_at_ms: at, valid_until_ms: at + 86400000 } } },
    before: structuredClone(before), proposed, expected_revision: 42, legacy: null,
  } };
  return fetchPolicySetupStatus();
}
export async function persistPolicySetup(id: number): Promise<PolicySetupStatus> {
  if (status.review?.id !== id || !["review_ready", "uncertain"].includes(status.phase)) throw new Error("UI fixture: review is not retryable.");
  status = { ...status, phase: "saved", receipt_revision: 43, error: null };
  return fetchPolicySetupStatus();
}
export async function discardPolicySetup(id: number): Promise<PolicySetupStatus> {
  if (status.review?.id !== id || !["review_ready", "failed", "saved"].includes(status.phase)) throw new Error("UI fixture: review cannot be discarded.");
  status = { phase: "idle", review: null, receipt_revision: null, error: null };
  return fetchPolicySetupStatus();
}

const scenario = new URLSearchParams(location.search).get("policy");
if (scenario && scenario !== "idle") {
  await reviewPolicySetup("fixture-agent", account, { symbols: ["BTC", "ETH"], max_order_usd: "15", max_position_usd: "25", max_open_exposure_usd: "25", max_leverage: 1, approval_required: true }, false, true);
  if (scenario === "uncertain") status = { ...status, phase: "uncertain", error: { kind: "uncertain", detail: "UI fixture: publication outcome is uncertain. " + "retained-policy-diagnostic/".repeat(20) } };
  if (scenario === "recovery_required") status = { ...status, phase: "recovery_required", error: { kind: "uncertain", detail: "UI fixture: persistence worker panicked after a possible commit. Publication remains unverified." } };
  if (scenario === "persisting" || scenario === "reviewing") status.phase = scenario;
  if (scenario === "saved") status = { ...status, phase: "saved", receipt_revision: 43 };
}
