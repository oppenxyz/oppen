import { expect, test } from "bun:test";
import { plugin, Transpiler } from "bun";
import { readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { compileScript, parse } from "@vue/compiler-sfc";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import { approvals } from "../stores/approvals";
import { supervision } from "../stores/supervision";
import { runtime } from "../stores/runtime";
import { shell } from "../stores/shell";

// Compile only this panel and its two existing visual dependencies in memory.
// No Vite listener, native app, DOM, or transport is started by this test.
plugin({
  name: "approval-panel-ssr-test",
  setup(build) {
    build.onLoad({ filter: /\/(ApprovalQueuePanel|PanelHousing|UiButton)\.vue$/ }, async ({ path }) => {
      const { descriptor } = parse(await readFile(path, "utf8"), { filename: path });
      const compiled = compileScript(descriptor, { id: "approval-panel-test", inlineTemplate: true, templateOptions: { ssr: true } }).content;
      return { contents: new Transpiler({ loader: "ts" }).transformSync(compiled),
        loader: "js", resolveDir: dirname(path) };
    });
  },
});

test("queue and row-local confirmation render identity, money and untrusted text inertly", async () => {
  const { default: Panel } = await import("./ApprovalQueuePanel.vue");
  const queue = toRaw(approvals.state);
  const supervisor = toRaw(supervision.state);
  const tasks = toRaw(runtime);
  const context = toRaw(shell);
  const before = { queue: { ...queue }, supervisor: { ...supervisor }, tasks: { ...tasks }, network: context.network };
  const binding = { agent: "bound-agent", account: `0x${"1".repeat(40)}` };
  const original = { kind: { kind: "market", slippage_bps: "100" }, reference_px: "100", reference_at_ms: 999 };
  const first = { ...binding, id: "approval-first", symbol: "BTC", is_buy: true, px: "101.000000000001", sz: "0.123456789012", reduce_only: false,
    reason: "<img src=x onerror=alert(1)>\n<script>bad()</script>", expires_at_ms: Date.now() + 120_000, original };
  const second = { ...first, id: "approval-second", reason: "Second proposal", original: null };
  try {
    context.network = "testnet";
    supervisor.status = { ...binding, network: "testnet", phase: "listening" };
    supervisor.error = null; supervisor.stopRequested = false; supervisor.command = null;
    tasks.status = { phase: "running", binding: null, detail: null }; tasks.error = null;
    queue.binding = binding;
    queue.status = { ...binding, owner_id: "owner-observed", phase: "ready", observed_at_ms: 1000, pending: [first, second], decision: null, error: null };
    queue.error = "<svg onload=bad()>";
    queue.confirmation = { owner_id: "owner-observed", proposal: first };
    queue.decision = { proposal_id: first.id, outcome: "not_pending", at_ms: 1001, error: null };
    const html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("&lt;img");
    expect(html).toContain("&lt;script&gt;");
    expect(html).toContain("&lt;svg");
    expect(html).not.toContain("<img");
    expect(html).not.toContain("<script");
    expect(html).toContain("Not pending; no rejection recorded");
    expect(html).toContain("Unknown original request");
    expect(html).toContain("Last observed proposals; current queue unavailable");
    expect(html).not.toContain("No pending proposals at the last observation");
    const confirmation = html.slice(html.indexOf('aria-label="Confirm proposal rejection"'), html.indexOf("approval-second"));
    expect(confirmation).toContain("Confirm rejection");
    expect(confirmation).toContain(binding.agent);
    expect(confirmation).toContain(binding.account);
    expect(confirmation).toContain(first.id);
    expect(confirmation).toContain(first.px);
    expect(confirmation).toContain(first.sz);
    expect(confirmation).toContain("BTC");
    expect(confirmation).toContain("Buy");
    expect(confirmation).toContain("&lt;img");
    expect(html).not.toContain(">Approve<");
    queue.status.review = { id: "retained-1", owner_id: "owner-observed", reason: first.reason,
      pairing_id: { network: "testnet", issued_seq: 8 }, display: {
        ...first, proposal_id: first.id, original_px: "101", reference_px: "100.25", reference_at_ms: 1002,
        drift_bps: "25.1234", asset_index: 0, notional_usd: "12.469135690336", order_type: { limit: { tif: "Ioc" } },
        cloid: "0x012345", grouping: "na", builder: null, policy_revision: 14, policy_hash: "verified-policy-hash",
        reviewed_at_ms: 1003, route: { network: "testnet", binding_seq: 9, binding: {
          agent: binding.agent, container: binding.account, vault_address: null,
          wallet: { generation: 1, address: `0x${"2".repeat(40)}`, approved_at_ms: 1, valid_until_ms: 200000 } } },
      } };
    queue.execution = { review_id: "retained-1", proposal_id: first.id, at_ms: 1004, result: null,
      error: { code: -32000, message: "<script>bad()</script>", data: { cloid: "0x012345", retryable: false } } };
    const reviewed = await renderToString(createSSRApp(Panel));
    for (const exact of ["100.25", "25.1234", "12.469135690336", "verified-policy-hash", "0x012345", "Ioc", "Confirm and submit", "execution unconfirmed"]) expect(reviewed).toContain(exact);
    expect(reviewed).not.toContain("<script");
    expect(reviewed).toContain("&lt;script&gt;");
    const candidate = reviewed.slice(reviewed.indexOf('aria-label="Candidate to submit"'), reviewed.indexOf("Confirm and submit"));
    for (const exact of ["BTC", "Buy", first.px, first.sz, "12.469135690336", "Not reduce only", "Ioc", "immediate or cancel"]) expect(candidate).toContain(exact);
    expect(candidate).not.toContain("<details");
    expect(candidate).not.toContain("<pre");
    const technicalEnd = reviewed.indexOf("</details>", reviewed.indexOf("Technical evidence"));
    const visibleIdentity = reviewed.slice(technicalEnd, reviewed.indexOf('aria-label="Candidate to submit"'));
    for (const exact of [binding.account, "Pairing", "testnet", "Review expires", "Builder", "None"]) expect(visibleIdentity).toContain(exact);
    queue.status.review.display.order_type = { trigger: { isMarket: true, triggerPx: "99.123456", tpsl: "sl" } };
    queue.status.review.display.reduce_only = true;
    queue.status.review.display.builder = { b: `0x${"3".repeat(40)}`, f: 10 };
    const triggered = await renderToString(createSSRApp(Panel));
    const triggerCandidate = triggered.slice(triggered.indexOf('aria-label="Candidate to submit"'), triggered.indexOf("Confirm and submit"));
    expect(triggerCandidate).toContain("Stop loss · Market trigger 99.123456 USD");
    expect(triggerCandidate).toContain("Reduce only");
    expect(triggered).toContain("10 tenths of a basis point");
    expect(triggered).toContain(queue.status.review.display.builder.b);
    queue.execution = { ...queue.execution, error: null, result: { status: "rejected", cloid: "0x012345" } };
    const refused = await renderToString(createSSRApp(Panel));
    expect(refused).toContain("Refused");
    expect(refused).not.toContain("Venue status: filled");
  } finally {
    Object.assign(queue, before.queue); Object.assign(supervisor, before.supervisor);
    Object.assign(tasks, before.tasks); context.network = before.network;
  }
});
