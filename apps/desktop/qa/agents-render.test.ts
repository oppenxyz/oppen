import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import type { StoredPolicy } from "../src/lib/bridge";

test("agent roster renders stored policy without obsolete vault authority", async () => {
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
    toRaw(operator).policy = policy;
    const { default: AgentsView } = await server.ssrLoadModule("/src/views/AgentsView.vue");
    const html = await renderToString(createSSRApp(AgentsView));
    expect(html).toContain("alpha");
    expect(html).toContain("Container route unavailable");
    expect(html).toContain("Allowed markets");
    expect(html).toContain("None");
  } finally {
    await server.close();
    globals.forEach((name, index) => {
      const descriptor = previous[index];
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else Reflect.deleteProperty(globalThis, name);
    });
  }
});
