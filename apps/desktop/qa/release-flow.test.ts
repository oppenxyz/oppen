import { expect, test } from "bun:test";
import type { KillSwitch, PolicyStatus } from "../src/lib/bridge";
import { createReleaseFixture, connectReleaseFixture } from "./release";
import { activationAgent as agent, activationAccount as account, createActivationFixture } from "./activation";

test("single-pilot fixture requires an engaged scope then carries released authority into fresh activation", async () => {
  let policy: PolicyStatus = { cached_revision: 42, acknowledgment: null, stop_generation: 7, admission_inhibited: true };
  let kill: KillSwitch = { global: null, agents: { [agent]: { engaged_at_ms: 1, reason: { reason: "operator" } } } };
  const disconnect = connectReleaseFixture(() => ({ generation: policy.stop_generation, kill: structuredClone(kill) }), status => {
    policy = structuredClone(status.policy_status); kill = structuredClone(status.cached_effective_kill);
  });
  try {
    const release = createReleaseFixture();
    const activation = createActivationFixture("idle", Date.now, {
      read: () => ({ policy: structuredClone(policy), blocked: !!kill.global || !!kill.agents[agent] }),
      publish: value => { policy = structuredClone(value); },
    });
    await expect(release.reviewKillRelease(agent, account, { scope: "global" })).rejects.toThrow("not engaged");
    await expect(activation.reviewActivation(agent, account)).rejects.toThrow("stop remains");
    const ready = await release.reviewKillRelease(agent, account, { scope: "agent", agent });
    expect(ready.review!.display.affected).toHaveLength(1);
    expect(ready.review!.display.persisted_engagement).not.toBeNull();
    const pilot = ready.review!.display.affected[0]!.pilot;
    const result = await release.confirmKillRelease(agent, account, ready.owner_id, ready.review!.id);
    expect(result.phase).toBe("released"); expect(policy.acknowledgment).toBeNull(); expect(policy.admission_inhibited).toBe(true);
    await activation.reviewActivation(agent, account);
    const reviewed = await activation.fetchActivationStatus(agent, account);
    expect(reviewed.review!.display.policy_revision).toBe(43);
    expect(reviewed.review!.display.stop_generation).toBe(8);
    expect(reviewed.review!.display.pilot.executed_usd).toBe(pilot.executed_usd);
    expect(reviewed.review!.display.pilot.reserved_usd).toBe(pilot.reserved_usd);
    await activation.confirmActivation(agent, account, reviewed.owner_id, reviewed.review!.id);
    expect((await activation.fetchActivationStatus(agent, account)).receipt?.policy_revision).toBe(43);
    expect(policy.acknowledgment).toEqual({ revision: 43, stop_generation: 8 });
    policy = { ...policy, stop_generation: 9, acknowledgment: null, admission_inhibited: true };
    kill.agents[agent] = { engaged_at_ms: 2, reason: { reason: "operator" } };
    await expect(activation.reviewActivation(agent, account)).rejects.toThrow("stop remains");
  } finally { disconnect(); }
});
