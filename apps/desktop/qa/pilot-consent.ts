// Synthetic UI transport only. No native, venue, key or ledger operations.
import type { PilotConsentAttestations, PilotConsentStatus } from "../src/lib/bridge";
import { activationAgent as agent, activationAccount as account, createActivationFixture } from "./activation";

export async function createPilotConsentFixture(scenario = "idle", clock = Date.now) {
  let status: PilotConsentStatus | null = null;
  let owner = 0;
  async function reviewPilotConsent(a: string, b: string): Promise<PilotConsentStatus> {
    if (a !== agent || b !== account) throw new Error("Synthetic existing authority identity mismatch.");
    if (status?.existing || status?.phase === "authorized" || status?.phase === "uncertain" || status?.review) throw new Error("Synthetic consent cannot be replaced or reset.");
    const evidence = (await createActivationFixture("review_ready", clock).fetchActivationStatus(agent, account)).review!.display;
    const seq = 1;
    status = { owner_id: `fixture-consent-${++owner}`, agent, account, operation_seq: seq, last_operation: { kind: "review" }, phase: "review_ready",
      existing: null, receipt: null, resolution: null, error: null, review: { id: String(seq), display: {
        network: "testnet", correlation: { route: evidence.route, baseline: { seq: 4, hash: "e".repeat(64) }, baseline_at_ms: clock() },
        policy_revision: 42, policy: evidence.policy, account: evidence.account, wallet_approval: evidence.wallet_approval,
        persisted_kill: { global: { engaged_at_ms: clock() - 60000, reason: { reason: "operator" } }, agents: {} },
        coverage: { network: "testnet", account, requested_start_ms: clock() - 30 * 86400000, requested_end_ms: null,
          pages: 2, terminated_by_short_page: true, local_read_started_at_ms: clock() - 500, local_read_completed_at_ms: clock(), initial_window: true },
        required_attestations: { dedicated_exclusive_account: true, never_used_for_trading: true },
        observed_at_ms: clock(), expires_at_ms: clock() + (scenario === "stale" ? -1 : 60000),
        order_limit_usd: "15", gross_exposure_limit_usd: "25", executed_limit_usd: "150", realized_loss_limit_usd: "5", max_leverage: 1,
      } } };
    if (scenario === "existing" || scenario === "legacy") {
      status.phase = "existing"; status.review = null;
      status.existing = { authentication: scenario === "legacy" ? "legacy_review_required" : "verified", agent, account,
        accounting: "known", executed_usd: "125.12345678", reserved_usd: "3", net_realized_pnl_usd: "-2.25", halt: null };
    }
    return structuredClone(status);
  }
  function retained(owner: string, id: string) {
    if (!status || status.owner_id !== owner || status.review?.id !== id) throw new Error("Synthetic stale consent owner or review.");
    return status;
  }
  const api = {
    async fetchPilotConsentStatus() { return structuredClone(status); }, reviewPilotConsent,
    async confirmPilotConsent(owner: string, id: string, attestations: PilotConsentAttestations) {
      const current = retained(owner, id);
      if (!attestations.never_used_for_in_scope_trading || !attestations.dedicated_account_exclusive_use
        || !attestations.original_baseline_and_no_reset_confirmed || attestations.typed_account.toLowerCase() !== account) throw new Error("Synthetic independent attestations required.");
      if (current.phase !== "review_ready") throw new Error("Synthetic consent already admitted.");
      const correlation = current.review!.display.correlation;
      current.operation_seq++; current.last_operation = { kind: "confirm", review_id: id };
      if (["unknown", "uncertain"].includes(scenario)) {
        current.phase = "uncertain"; current.error = { status: "uncertain", correlation, detail: "Synthetic consent publication unknown <script>inert</script>. " + "retained-consent-evidence/".repeat(10) };
        if (scenario === "unknown") throw new Error("Synthetic lost consent IPC reply.");
      } else if (scenario === "refused" || current.review!.display.expires_at_ms <= clock()) {
        current.phase = "refused"; current.error = { status: "refused", detail: "Synthetic history or freshness refusal. No baseline reset." };
      } else { current.phase = "authorized"; current.receipt = { correlation, seq: 5, hash: "f".repeat(64) }; }
      return structuredClone(current);
    },
    async discardPilotConsent(owner: string, id: string) {
      const current = retained(owner, id); if (current.phase !== "review_ready") throw new Error("Synthetic work cannot be discarded.");
      current.operation_seq++; current.last_operation = { kind: "discard", review_id: id }; current.phase = "closed"; current.review = null; return structuredClone(current);
    },
    async reconcilePilotConsent(owner: string, id: string) {
      const current = retained(owner, id); current.operation_seq++; current.last_operation = { kind: "reconcile", review_id: id };
      current.phase = "uncertain"; current.resolution = { status: "unknown", detail: "Synthetic read-only outcome remains unknown; no authorization retry." }; return structuredClone(current);
    },
  };
  if (scenario !== "idle") await reviewPilotConsent(agent, account);
  if (scenario === "recovery_required" && status) {
    const current = status as PilotConsentStatus;
    current.phase = "recovery_required";
    current.error = { status: "uncertain", correlation: current.review!.display.correlation, detail: "Synthetic terminal worker failure. Controlled recovery required; restart alone does not verify publication." };
  }
  return api;
}
let fixture: ReturnType<typeof createPilotConsentFixture> | undefined;
function local() { return fixture ??= createPilotConsentFixture(new URLSearchParams(location.search).get("consent") ?? "idle"); }
export const fetchPilotConsentStatus = async () => (await local()).fetchPilotConsentStatus();
export const reviewPilotConsent = async (agent: string, account: string) => (await local()).reviewPilotConsent(agent, account);
export const confirmPilotConsent = async (owner: string, id: string, attestations: PilotConsentAttestations) => (await local()).confirmPilotConsent(owner, id, attestations);
export const discardPilotConsent = async (owner: string, id: string) => (await local()).discardPilotConsent(owner, id);
export const reconcilePilotConsent = async (owner: string, id: string) => (await local()).reconcilePilotConsent(owner, id);
