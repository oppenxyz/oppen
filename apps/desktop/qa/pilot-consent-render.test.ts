import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import { createPilotConsentFixture } from "./pilot-consent";

test("initial consent renders paused authority, all five caps, bounded coverage and three unchecked statements", async () => {
  const server = await createServer({ root: fileURLToPath(new URL("..", import.meta.url)), configFile: false, plugins: [vue()],
    optimizeDeps: { noDiscovery: true, include: [] }, server: { middlewareMode: true, hmr: false } });
  try {
    const { default: Panel } = await server.ssrLoadModule("/src/components/PilotConsentPanel.vue");
    const { pilotConsent } = await server.ssrLoadModule("/src/stores/pilot-consent.ts");
    const state = toRaw(pilotConsent.state);
    Object.assign(state, { observed: true, readError: null, context: { network: "testnet", blocked: null },
      status: await (await createPilotConsentFixture("review_ready")).fetchPilotConsentStatus() });
    let html = await renderToString(createSSRApp(Panel));
    for (const label of ["Per order", "Cumulative executed notional", "Realized loss including fees", "Gross exposure including resting opening orders", "Maximum leverage",
      "Persisted stops", "global / engaged at ms", "requested start ms", "requested end ms", "terminated by short page", "pages", "bounded, not lifetime-history verification",
      "Type the full reviewed account", "0x0000000000000000000000000000000000000001"]) expect(html).toContain(label);
    expect((html.match(/type="checkbox"/g) ?? []).length).toBe(3); expect(html).not.toContain(" checked");
    expect(html).toContain("&lt;script&gt;"); expect(html).not.toContain("<script>");
    for (const scenario of ["existing", "legacy"]) {
      state.status = await (await createPilotConsentFixture(scenario)).fetchPilotConsentStatus();
      html = await renderToString(createSSRApp(Panel));
      expect(html).toContain("125.12345678"); expect(html).not.toContain("Confirm initial consent");
      expect(html).toContain(scenario === "legacy" ? "separate preservation review" : "inspect-only");
    }
    state.status = await (await createPilotConsentFixture("recovery_required")).fetchPilotConsentStatus();
    html = await renderToString(createSSRApp(Panel));
    expect(html).toContain("This owner cannot reconcile");
    expect(html).toContain("restart alone does not verify publication");
    expect(html).not.toContain("Reconcile consent outcome (read-only)");
    expect(html).not.toContain("Confirm initial consent");
  } finally { await server.close(); }
});
