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
  } finally {
    Object.assign(queue, before.queue); Object.assign(supervisor, before.supervisor);
    Object.assign(tasks, before.tasks); context.network = before.network;
  }
});
