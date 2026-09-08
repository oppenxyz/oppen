import { expect, test } from "bun:test";
import { plugin, Transpiler } from "bun";
import { readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { compileScript, parse } from "@vue/compiler-sfc";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import { activation } from "../stores/activation";

plugin({
  name: "activation-panel-ssr-test",
  setup(build) {
    build.onLoad({ filter: /\/(ActivationPanel|ReadoutRows|UiButton)\.vue$/ }, async ({ path }) => {
      const { descriptor } = parse(await readFile(path, "utf8"), { filename: path });
      const compiled = compileScript(descriptor, { id: "activation-panel-test", inlineTemplate: true, templateOptions: { ssr: true } }).content;
      return { contents: new Transpiler({ loader: "ts" }).transformSync(compiled), loader: "js", resolveDir: dirname(path) };
    });
  },
});

test("activation renders full inert evidence and separates cached acknowledgment from eligibility", async () => {
  const { default: Panel } = await import("./ActivationPanel.vue");
  const state = toRaw(activation.state);
  const before = { ...state };
  const now = Date.now();
  const account = `0x${"1".repeat(40)}`;
  const signer = `0x${"2".repeat(40)}`;
  const route = { network: "testnet", binding_seq: 9, binding: { agent: "fixture-agent", container: account, vault_address: null,
    wallet: { generation: 1, address: signer, approved_at_ms: now, valid_until_ms: now + 120_000 } } };
  try {
    state.context = { network: "testnet", agent: "fixture-agent", account, blocked: null };
    state.status = { operation_seq: 1, last_operation: { kind: "review" }, owner_id: "1", agent: "fixture-agent", account, phase: "review_ready", receipt: null, error: null,
      policy_status: { cached_revision: 4, acknowledgment: null, stop_generation: 7, admission_inhibited: true },
      review: { id: "2", display: { route, policy_revision: 4, stop_generation: 7,
        policy: { symbols: ["TEST"], max_order_usd: "15.000000000001", max_position_usd: "25", max_slippage_bps: "50",
          order_rate: { count: 3, per_ms: 1000 }, reduce_only: false, approval_required: true,
          risk: { max_leverage: 1, margin_mode: "cross", max_open_exposure_usd: "25", max_risk_usd: null },
          loss: { max_daily_loss_usd: "10", max_drawdown_usd: null }, freshness: { max_market_age_ms: 5000, max_account_age_ms: 5000 },
          max_mark_divergence_bps: "100", mark_divergence_window_ms: 5000 },
        pilot: { agent: "fixture-agent", account, authorized_at_ms: now, baseline: { seq: 2, hash: "baseline-hash" },
          executed_usd: "12.123456789012", reserved_usd: "3", net_realized_pnl_usd: "-1.25", halt: null },
        account: { contract_version: 0, network: "testnet", address: account, as_of_ms: now, feed_age_ms: 0, feed: "live",
          balances: { equity_usd: "100", perps_account_value_usd: "100", spot_usdc_available: "0", total_margin_used_usd: "5", withdrawable_usd: "95" }, positions: [], orders: [] },
        wallet_approval: { name: "<script>untrusted wallet name</script>", address: signer, validUntil: now + 120_000 },
        observed_at_ms: now, expires_at_ms: now + 60_000, gross_exposure_usd: "5", remaining_committed_usd: "134.876543210988" } } };
    const reviewed = await renderToString(createSSRApp(Panel));
    for (const value of [account, signer, "15.000000000001", "12.123456789012", "134.876543210988", "baseline-hash", "max market age ms", "approval required", "Confirm activation"]) expect(reviewed).toContain(value);
    expect(reviewed).toContain("&lt;script&gt;"); expect(reviewed).not.toContain("<script>");
    expect(reviewed).toContain('type="checkbox"'); expect(reviewed).not.toContain(" checked");
    expect(reviewed).toContain("I confirm TESTNET agent fixture-agent and account");
    state.status = { ...state.status, phase: "acknowledged", review: null,
      receipt: { route, policy_revision: 4, stop_generation: 7, acknowledged_at_ms: now, audit_seq: 12, audit_hash: "receipt-hash" },
      policy_status: { cached_revision: 5, acknowledgment: { revision: 4, stop_generation: 7 }, stop_generation: 8, admission_inhibited: true } };
    state.readError = "<img src=x onerror=bad()>";
    const acknowledged = await renderToString(createSSRApp(Panel));
    expect(acknowledged).toContain("receipt-hash");
    expect(acknowledged).toContain("Local admission inhibited");
    expect(acknowledged).toContain("Acknowledgment is not current order eligibility or venue acceptance");
    expect(acknowledged).toContain("&lt;img"); expect(acknowledged).not.toContain("<img");
    state.status = { ...state.status, phase: "uncertain", receipt: null, error: { kind: "worker", detail: "Publication unconfirmed" } };
    const uncertain = await renderToString(createSSRApp(Panel));
    expect(uncertain).toContain("Completion is unconfirmed");
    expect(uncertain).not.toContain("Confirm activation");
    expect(uncertain).not.toContain("Acknowledgment receipt");
  } finally { Object.assign(state, before); }
});
