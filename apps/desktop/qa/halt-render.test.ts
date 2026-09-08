import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import type { McpStatus } from "../src/lib/bridge";

test("persistent halt banner distinguishes uncertain durability, retrying cancellation and acknowledgments", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), configFile: false, plugins: [vue()],
    server: { middlewareMode: true, hmr: { host: "127.0.0.1", port: 0 } },
  });
  try {
    const { supervision } = await server.ssrLoadModule("/src/stores/supervision.ts");
    const state = toRaw(supervision.state);
    const { default: Banner } = await server.ssrLoadModule("/src/components/shell/AgentHaltBanner.vue");
    const status: McpStatus = {
      phase: "listening", network: "testnet", agent: "bound-agent", account: "0x1111111111111111111111111111111111111111",
      listener: "127.0.0.1:7433", reconciled: true, account_feeds_ready: true, orders_inhibited: true, detail: null,
      supervision_last_completed_ms: 123, supervision_in_progress: false, supervision_error: null,
      halt: { phase: "uncertain", cancellation: "retrying", requested_at_ms: 100, durable_revision: null,
        error: "<img src=x onerror=alert(1)>" + "uncertain-diagnostic/".repeat(30), cancellation_error: "Cancel retry failed" },
    };
    state.status = status;
    state.error = "Cached status unavailable";
    let html = await renderToString(createSSRApp(Banner));
    expect(html).toContain("Halt durability uncertain");
    expect(html).toContain("Cancellation retrying");
    expect(html).toContain("Unconfirmed");
    expect(html).toContain("bound-agent");
    expect(html).toContain(status.account!);
    expect(html).toContain("&lt;img");
    expect(html).not.toContain("<img");
    expect(html).toContain("Cached status unavailable");
    expect(html).toContain("Cancellation acknowledgments do not prove the venue is flat");
    const scope = "Pauses this agent identity, including later account assignments. Requests cancellation only for the supervised account shown. A changed registry route prevents cancellation confirmation.";
    expect(html).toContain(scope);
    const { default: header } = await server.ssrLoadModule("/src/components/shell/AppHeader.vue?raw");
    expect(header).toContain(scope);
    expect(header).toContain("This does not resume orders or halt other agents");
    expect(header).not.toContain("for this identity only");
    state.status = { ...status, halt: { ...status.halt, phase: "persisted", cancellation: "acknowledged", durable_revision: 42, error: null, cancellation_error: null } };
    html = await renderToString(createSSRApp(Banner));
    expect(html).toContain("Agent pause persisted");
    expect(html).toContain("Cancellation acknowledged");
    expect(html).toContain("Positions may remain open");
    const { default: source } = await server.ssrLoadModule("/src/components/shell/AgentHaltBanner.vue?raw");
    expect(source).toContain("width: min(100%, 100vw)");
    expect(source).toContain("overflow-wrap: anywhere");
    const { default: app } = await server.ssrLoadModule("/src/App.vue?raw");
    expect(app).toContain("<AgentHaltBanner");
    expect(app).toContain("supervision.startPolling()");
  } finally { await server.close(); }
});
