import { expect, test } from "bun:test";
import { fileURLToPath } from "node:url";
import vue from "@vitejs/plugin-vue";
import { createServer } from "vite";
import { createSSRApp, toRaw } from "vue";
import { renderToString } from "vue/server-renderer";
import { activationAccount as account, activationAgent as agent, createActivationFixture } from "./activation";

test("activation fixture reads never confirm; explicit commands retain owner and review scope", async () => {
  const fixture = createActivationFixture("idle", () => 100_000);
  for (let i = 0; i < 3; i++) expect((await fixture.fetchActivationStatus(agent, account)).phase).toBe("idle");
  expect((await fixture.reviewActivation(agent, account)).phase).toBe("reviewing");
  const ready = await fixture.fetchActivationStatus(agent, account);
  expect(ready.policy_status.admission_inhibited).toBe(true);
  expect(ready.review!.display.expires_at_ms).toBe(160_000);
  await expect(fixture.confirmActivation(agent, account, "old-owner", ready.review!.id)).rejects.toThrow("stale");
  await expect(fixture.confirmActivation(agent, account, ready.owner_id, "old-review")).rejects.toThrow("stale");
  await expect(fixture.reviewActivation("other-agent", account)).rejects.toThrow("binding");
  expect((await fixture.fetchActivationStatus(agent, account)).phase).toBe("review_ready");
  expect((await fixture.confirmActivation(agent, account, ready.owner_id, ready.review!.id)).phase).toBe("confirming");
  const done = await fixture.fetchActivationStatus(agent, account);
  expect(done.phase).toBe("acknowledged");
  expect(done.review).toBeNull();
  expect(done.receipt?.policy_revision).toBe(42);
  expect(done.policy_status.acknowledgment).toEqual({ revision: 42, stop_generation: 7 });
  expect((await fixture.reviewActivation(agent, account)).phase).toBe("reviewing");
  const second = await fixture.fetchActivationStatus(agent, account);
  expect(second.review!.id).not.toBe(ready.review!.id);
  expect((await fixture.discardActivation(agent, account, second.owner_id, second.review!.id)).phase).toBe("idle");
});

test("activation fixture expiration and unknown IPC never fabricate an acknowledgment", async () => {
  for (const scenario of ["stale", "unknown"]) {
    const fixture = createActivationFixture(scenario, () => 100_000);
    const ready = await fixture.fetchActivationStatus(agent, account);
    const command = fixture.confirmActivation(agent, account, ready.owner_id, ready.review!.id);
    if (scenario === "unknown") await expect(command).rejects.toThrow("outcome unknown");
    else expect((await command).phase).toBe("confirming");
    const result = await fixture.fetchActivationStatus(agent, account);
    expect(result.phase).toBe(scenario === "stale" ? "refused" : "uncertain");
    expect(result.receipt).toBeNull();
    expect(result.policy_status.admission_inhibited).toBe(true);
    await expect(fixture.confirmActivation(agent, account, ready.owner_id, ready.review!.id)).rejects.toThrow("stale");
  }
  await expect(createActivationFixture("error").fetchActivationStatus(agent, account)).rejects.toThrow("status unavailable");
});

test("activation QA panel keeps full identity and consent visible with stale and uncertain evidence", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)), configFile: false, plugins: [vue()],
    optimizeDeps: { noDiscovery: true, include: [] },
    server: { middlewareMode: true, hmr: false },
  });
  try {
    const { default: Panel } = await server.ssrLoadModule("/src/components/ActivationPanel.vue");
    const { activation } = await server.ssrLoadModule("/src/stores/activation.ts");
    const state = toRaw(activation.state);
    state.context = { network: "testnet", agent, account, blocked: null };
    for (const scenario of ["review_ready", "stale", "refused", "uncertain", "acknowledged"]) {
      state.status = await createActivationFixture(scenario).fetchActivationStatus(agent, account);
      const html = await renderToString(createSSRApp(Panel));
      expect(html).toContain(account);
      expect(html).toContain("Cached policy revision");
      expect(html).toContain("not a fresh authority check or venue acceptance");
      if (scenario === "review_ready" || scenario === "stale") {
        expect(html).toContain("0x1234567890abcdef1234567890abcdef12345678");
        expect(html).toContain('type="checkbox"');
        expect(html).not.toContain(" checked");
        expect(html).toContain("Confirm activation");
        expect(html).toContain("&lt;script&gt;");
        expect(html).not.toContain("<script>");
        expect(html).toContain("134.876543210988");
        if (scenario === "stale") expect(html).toContain("expired");
      }
      if (scenario === "uncertain" || scenario === "refused") {
        expect(html).toContain("&lt;img");
        expect(html).not.toContain("<img");
        expect(html).not.toContain("Confirm activation");
        expect(html).not.toContain("Acknowledgment receipt");
      }
      if (scenario === "acknowledged") expect(html).toContain("Acknowledgment is not current order eligibility or venue acceptance");
    }
  } finally { await server.close(); }
});
