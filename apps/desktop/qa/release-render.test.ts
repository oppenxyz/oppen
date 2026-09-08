import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import { createRelease } from "../src/stores/release";
import { createReleaseFixture } from "./release";
import { activationAccount as account, activationAgent as agent } from "./activation";

test("release panel renders the complete global roster, unchanged accounting and remaining stops", async () => {
  const api = createReleaseFixture("unknown", Date.now, true);
  const controller = createRelease({ status: api.fetchKillReleaseStatus, review: api.reviewKillRelease,
    confirm: api.confirmKillRelease, discard: api.discardKillRelease, reconcile: api.reconcileKillRelease });
  const state = toRaw(controller.state);
  Object.assign(state, { context: { network: "testnet", agent, account, blocked: null },
    status: await api.reviewKillRelease(agent, account, { scope: "global" }) });
  const server = await createServer({ root: fileURLToPath(new URL("..", import.meta.url)), configFile: false, plugins: [vue()],
    optimizeDeps: { noDiscovery: true, include: [] }, server: { middlewareMode: true, hmr: false } });
  try {
    const { default: Panel } = await server.ssrLoadModule("/src/components/ReleasePanel.vue");
    const html = await renderToString(createSSRApp(Panel, { controller }));
    for (const value of [agent, account, "second-affected-agent", "12.123456789012", "Stops remaining", "budgets are unchanged", "Confirm scope release"]) expect(html).toContain(value);
    expect(html).toContain('type="checkbox"'); expect(html).not.toContain(" checked");
    expect(html).toContain("Pre-ES38 builds cannot operate this history, including cleanup startup");
    expect(html).toContain("even if publication is reported uncertain");
    expect(html).toContain("never delete history or reset accounting to downgrade");
    const ready = state.status!;
    await expect(api.confirmKillRelease(agent, account, ready.owner_id, ready.review!.id)).rejects.toThrow("lost");
    Object.assign(state, { status: await api.fetchKillReleaseStatus(agent, account) });
    const uncertain = await renderToString(createSSRApp(Panel, { controller }));
    expect(uncertain).toContain("&lt;script&gt;"); expect(uncertain).not.toContain("<script>");
    expect(uncertain).toContain("Reconcile outcome (read-only)"); expect(uncertain).not.toContain("Confirm scope release");
  } finally { await server.close(); }
});
