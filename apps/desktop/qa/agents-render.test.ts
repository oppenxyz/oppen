import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import type { PolicyInspection, StoredPolicy } from "../src/lib/bridge";

test.each(["unverified_legacy", "unverified_ledger"] as const)("agent roster renders %s without obsolete vault authority", async (provenance) => {
  const globals = ["window", "document"] as const;
  const previous = globals.map(name => Object.getOwnPropertyDescriptor(globalThis, name));
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    matchMedia: () => ({ matches: true, addEventListener() {} }),
  } });
  Object.defineProperty(globalThis, "document", { configurable: true, value: {
    hidden: true, addEventListener() {},
  } });
  const server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)),
    configFile: false,
    plugins: [vue()],
    server: { middlewareMode: true, hmr: { host: "127.0.0.1", port: 0 } },
  });
  try {
    const { operator } = await server.ssrLoadModule("/src/stores/operator.ts");
    const policy: StoredPolicy = {
      guardrails: {
        alpha: {
          symbols: [], max_order_usd: "15", max_position_usd: "25",
          max_slippage_bps: "5", order_rate: { count: 1, per_ms: 1000 },
          reduce_only: false, approval_required: true,
          risk: { max_leverage: 1, margin_mode: "isolated", max_open_exposure_usd: "25", max_risk_usd: null },
          loss: { max_daily_loss_usd: "5", max_drawdown_usd: null },
        },
      },
      account_limits: { max_daily_loss_usd: "5", max_drawdown_usd: null },
      kill: { global: null, agents: {} },
    };
    const inspection: PolicyInspection = {
      provenance, revision: provenance === "unverified_ledger" ? 17 : null,
      observed_at_ms: 1000, state: policy,
    };
    toRaw(operator).policy = inspection;
    const { default: AgentsView } = await server.ssrLoadModule("/src/views/AgentsView.vue");
    const html = await renderToString(createSSRApp(AgentsView));
    expect(html).toContain("alpha");
    expect(html).toContain("Container route unavailable");
    expect(html).toContain("Allowed markets");
    expect(html).toContain("None");
    expect(html).toContain(provenance === "unverified_ledger" ? "Unverified ledger policy" : "Unverified legacy policy");
    if (provenance === "unverified_ledger") expect(html).toContain("Revision 17");
    const { default: SettingsView } = await server.ssrLoadModule("/src/views/SettingsView.vue");
    const settings = await renderToString(createSSRApp(SettingsView));
    expect(settings).toContain(provenance === "unverified_ledger" ? "Unverified ledger policy" : "Unverified legacy policy");
    expect(settings).toContain("Runtime policy not read.");
    expect(settings).toContain("Snapshot revision");
  } finally {
    await server.close();
    globals.forEach((name, index) => {
      const descriptor = previous[index];
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else Reflect.deleteProperty(globalThis, name);
    });
  }
});
