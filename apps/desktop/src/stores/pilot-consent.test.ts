import { createPilotConsent, consentOwnsContext, consentCanEditIdentity } from "./pilot-consent";
import type { PilotConsentStatus, PilotConsentAttestations } from "../lib/bridge";
import { createPilotConsentFixture } from "../../qa/pilot-consent";
import { activationAgent as agent, activationAccount as account } from "../../qa/activation";
declare const test: (name: string, body: () => void | Promise<void>) => void;
declare const expect: (value: unknown) => { toBe(value: unknown): void; toEqual(value: unknown): void; toHaveLength(value: number): void; toBeNull(): void; toContain(value: string): void };
const attestations: PilotConsentAttestations = { typed_account: account, never_used_for_in_scope_trading: true,
  dedicated_account_exclusive_use: true, original_baseline_and_no_reset_confirmed: true };
function deferred<T>() { let resolve!: (value: T) => void, reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { resolve, reject, promise }; }
async function settle() { for (let i = 0; i < 10; i++) await Promise.resolve(); }
function fixture() {
  const timers: { ms: number; callback: () => void; active: boolean }[] = [];
  const reads: ReturnType<typeof deferred<PilotConsentStatus | null>>[] = [];
  const writes: { kind: string; attestations?: PilotConsentAttestations; identity?: { agent: string; account: string }; reply: ReturnType<typeof deferred<PilotConsentStatus>> }[] = [];
  function write(kind: string, attestations?: PilotConsentAttestations, identity?: { agent: string; account: string }) { const reply = deferred<PilotConsentStatus>(); writes.push({ kind, attestations, identity, reply }); return reply.promise; }
  const store = createPilotConsent({ status: () => { const read = deferred<PilotConsentStatus | null>(); reads.push(read); return read.promise; },
    review: (agent, account) => write("review", undefined, { agent, account }), confirm: (_o, _id, values) => write("confirm", values), discard: () => write("discard"), reconcile: () => write("reconcile"),
  }, () => true, (callback, ms) => { const timer = { callback, ms, active: true }; timers.push(timer); return () => { timer.active = false; }; });
  store.setContext({ network: "testnet", blocked: null }); store.draft.agent = agent; store.draft.account = account;
  return { store, reads, writes, timeout() { const timer = timers.find(timer => timer.active && timer.ms === 5000)!; timer.active = false; timer.callback(); },
    async open(status: PilotConsentStatus | null = null) { store.startPolling(); reads[0]!.resolve(status); await settle(); } };
}
async function ready() { return (await (await createPilotConsentFixture("review_ready")).fetchPilotConsentStatus())!; }

test("nullable consent status never invents identity and every independent attestation is required", async () => {
  const f = fixture(); await f.open(); expect(f.store.state.status).toBeNull(); expect(f.writes).toHaveLength(0);
  const reviewed = await ready(); const review = f.store.review(); f.writes[0]!.reply.resolve(reviewed); await review;
  f.reads[1]!.resolve(reviewed); await settle();
  for (const key of ["never_used_for_in_scope_trading", "dedicated_account_exclusive_use", "original_baseline_and_no_reset_confirmed"] as const) {
    await f.store.confirm(reviewed.owner_id, reviewed.review!.id, { ...attestations, [key]: false }); expect(f.writes).toHaveLength(1);
  }
  await f.store.confirm(reviewed.owner_id, reviewed.review!.id, { ...attestations, typed_account: `0x${"2".repeat(40)}` }); expect(f.writes).toHaveLength(1);
  const confirm = f.store.confirm(reviewed.owner_id, reviewed.review!.id, attestations);
  expect(f.writes[1]!.attestations).toEqual(attestations);
  f.writes[1]!.reply.resolve({ ...reviewed, phase: "authorized", operation_seq: 2, last_operation: { kind: "confirm", review_id: reviewed.review!.id },
    receipt: { correlation: reviewed.review!.display.correlation, seq: 5, hash: "receipt" } }); await confirm;
  await f.store.review(); expect(f.writes).toHaveLength(2); f.store.stopPolling();
});

test("existing authenticated and legacy consent are inspection only", async () => {
  for (const scenario of ["existing", "legacy"]) {
    const status = await (await createPilotConsentFixture(scenario)).fetchPilotConsentStatus();
    const f = fixture(); await f.open(status); await f.store.review();
    expect(f.writes).toHaveLength(0); expect(f.store.state.commandError).toContain("inspect-only"); f.store.stopPolling();
  }
});

test("same-operation teardown degradation overrides terminal results and cannot be cleared by stale success", async () => {
  const reviewed = await ready();
  const receipt = { correlation: reviewed.review!.display.correlation, seq: 5, hash: "historical-receipt" };
  for (const phase of ["authorized", "refused", "closed"] as const) {
    const terminal: PilotConsentStatus = { ...reviewed, phase, operation_seq: 2, last_operation: { kind: "confirm", review_id: reviewed.review!.id }, receipt: phase === "authorized" ? receipt : null };
    const f = fixture(); await f.open(terminal);
    let reading = f.store.refresh(); f.reads[1]!.resolve({ ...terminal, phase: "uncertain", receipt: null,
      error: { status: "uncertain", correlation: receipt.correlation, detail: "Drain failure" } }); await reading;
    expect(f.store.state.status?.phase).toBe("uncertain"); expect(consentOwnsContext(f.store.state)).toBe(true);
    reading = f.store.refresh(); f.reads[2]!.resolve({ ...terminal, phase: "recovery_required", receipt: null,
      error: { status: "uncertain", correlation: receipt.correlation, detail: "Worker is terminal; recovery required" } }); await reading;
    expect(f.store.state.status?.phase).toBe("recovery_required"); expect(consentOwnsContext(f.store.state)).toBe(true);
    if (phase === "authorized") expect(f.store.state.status?.receipt?.hash).toBe("historical-receipt");
    await f.store.reconcile(terminal.owner_id, reviewed.review!.id); await f.store.review(); expect(f.writes).toHaveLength(0);
    reading = f.store.refresh(); f.reads[3]!.resolve(terminal); await reading;
    expect(f.store.state.status?.phase).toBe("recovery_required");
    expect(consentCanEditIdentity(f.store.state)).toBe(false); f.store.stopPolling();
  }
});

test("terminal refusal permits identity correction without repeated polls overwriting the draft", async () => {
  const reviewed = await ready();
  for (const phase of ["refused", "closed"] as const) {
    const refused: PilotConsentStatus = { ...reviewed, agent: "typo-agent", account: `0x${"2".repeat(40)}`, review: null,
      phase, error: { status: "refused", detail: "No matching registry route" } };
    const f = fixture(); await f.open(refused); expect(consentCanEditIdentity(f.store.state)).toBe(true);
    f.store.draft.agent = agent; f.store.draft.account = account;
    const read = f.store.refresh(); f.reads[1]!.resolve(structuredClone(refused)); await read;
    expect(f.store.draft.agent).toBe(agent); expect(f.store.draft.account).toBe(account);
    const retry = f.store.review(); expect(f.writes[0]!.identity).toEqual({ agent, account });
    expect(consentCanEditIdentity(f.store.state)).toBe(false);
    f.writes[0]!.reply.resolve({ ...reviewed, owner_id: "corrected-owner" }); await retry;
    expect(f.store.state.status?.owner_id).toBe("corrected-owner"); expect(consentCanEditIdentity(f.store.state)).toBe(false);
    f.store.stopPolling();
  }
});

test("identity remains locked with consent receipts, unresolved outcomes or active work", async () => {
  const reviewed = await ready(); const closed: PilotConsentStatus = { ...reviewed, phase: "closed", review: null };
  const receipt = { correlation: reviewed.review!.display.correlation, seq: 5, hash: "receipt" };
  for (const status of [
    { ...closed, receipt },
    { ...closed, existing: { authentication: "verified" as const, agent, account, accounting: "unavailable" as const, detail: "Unavailable", halt: null } },
    { ...closed, error: { status: "uncertain" as const, correlation: null, detail: "Unknown" } },
    { ...closed, resolution: { status: "unknown" as const, detail: "Unknown" } },
    { ...closed, phase: "uncertain" as const },
  ]) expect(consentCanEditIdentity({ status, command: null, outcomeUnknown: false })).toBe(false);
  expect(consentCanEditIdentity({ status: closed, command: null, outcomeUnknown: true })).toBe(false);
  expect(consentCanEditIdentity({ status: closed, command: "review", outcomeUnknown: false })).toBe(false);
});

test("typed pre-admission refusal needs a fresh unchanged native observation before explicit retry", async () => {
  const f = fixture(); await f.open();
  const reviewing = f.store.review(); f.writes[0]!.reply.reject({ status: "refused", detail: "Existing registry missing" }); await reviewing;
  expect(f.store.state.outcomeUnknown).toBe(true);
  f.reads[1]!.resolve(null); await settle(); expect(f.store.state.outcomeUnknown).toBe(false);
  expect(f.writes).toHaveLength(1);
  const retry = f.store.review(); f.writes[1]!.reply.resolve(await ready()); await retry;
  expect(f.writes).toHaveLength(2); f.store.stopPolling();
});

test("corrected identity can retry after typed refusal and a fresh unchanged prior-owner observation", async () => {
  const reviewed = await ready();
  const oldOwner: PilotConsentStatus = { ...reviewed, agent: "typo-agent", account: `0x${"2".repeat(40)}`,
    phase: "refused", review: null, error: { status: "refused", detail: "No matching authority" } };
  const f = fixture(); await f.open(oldOwner);
  f.store.draft.agent = agent; f.store.draft.account = account;
  const staleRead = f.store.refresh();
  const rejected = f.store.review();
  expect(f.writes[0]!.identity).toEqual({ agent, account });
  f.writes[0]!.reply.reject({ status: "refused", detail: "Prior owner still draining" }); await rejected;
  f.reads[1]!.resolve(oldOwner); await staleRead; await settle();
  expect(f.store.state.outcomeUnknown).toBe(true);
  f.reads[2]!.resolve({ ...reviewed, owner_id: "unrelated-owner" }); await settle();
  expect(f.store.state.outcomeUnknown).toBe(true); expect(f.store.state.status?.owner_id).toBe(oldOwner.owner_id);
  const freshRead = f.store.refresh(); f.reads[3]!.resolve(structuredClone(oldOwner)); await freshRead;
  expect(f.store.state.outcomeUnknown).toBe(false); expect(f.store.blocker()).toBeNull();
  expect(f.store.draft.agent).toBe(agent); expect(f.store.draft.account).toBe(account);
  expect(f.writes).toHaveLength(1);
  const retry = f.store.review(); expect(f.writes[1]!.identity).toEqual({ agent, account });
  f.writes[1]!.reply.resolve({ ...reviewed, owner_id: "corrected-owner" }); await retry;
  expect(f.store.state.status?.owner_id).toBe("corrected-owner"); expect(f.store.state.outcomeUnknown).toBe(false);
  f.store.stopPolling();
});

test("timed-out status observers cannot prevent a later terminal read or clear its read slot", async () => {
  const reviewed = await ready(); const f = fixture(); await f.open(reviewed);
  const confirming = f.store.confirm(reviewed.owner_id, reviewed.review!.id, attestations);
  const oldRead = f.store.refresh(); expect(f.reads).toHaveLength(2);
  f.timeout(); expect(f.reads).toHaveLength(3); expect(f.store.state.outcomeUnknown).toBe(true);
  f.reads[1]!.resolve(reviewed); await oldRead;
  expect(f.store.state.reading).toBe(true); expect(f.reads).toHaveLength(3);
  f.reads[2]!.resolve({ ...reviewed, phase: "authorized", operation_seq: 2, last_operation: { kind: "confirm", review_id: reviewed.review!.id },
    receipt: { correlation: reviewed.review!.display.correlation, seq: 5, hash: "native-receipt" } }); await settle();
  expect(f.store.state.command).toBeNull(); expect(f.store.state.outcomeUnknown).toBe(false);
  expect(f.store.state.status?.receipt?.hash).toBe("native-receipt");
  f.writes[0]!.reply.resolve({ ...reviewed, phase: "confirming", operation_seq: 2, last_operation: { kind: "confirm", review_id: reviewed.review!.id } }); await confirming;
  expect(f.store.state.status?.phase).toBe("authorized"); f.store.stopPolling();
  const g = fixture(); g.store.startPolling(); g.timeout();
  g.reads[1]!.resolve({ ...reviewed, phase: "closed" }); await settle();
  expect(g.store.state.status?.phase).toBe("closed"); expect(g.store.state.reading).toBe(false);
  g.store.stopPolling();
});

test("discard closes one owner and an explicit fresh review is correlated at sequence one under a new owner", async () => {
  const reviewed = await ready(); const f = fixture(); await f.open(reviewed);
  const discard = f.store.discard(reviewed.owner_id, reviewed.review!.id);
  const closed: PilotConsentStatus = { ...reviewed, operation_seq: 2, last_operation: { kind: "discard", review_id: reviewed.review!.id }, phase: "closed" };
  f.writes[0]!.reply.resolve(closed); await discard; f.reads[1]!.resolve(closed); await settle();
  expect(f.store.state.outcomeUnknown).toBe(false); expect(consentOwnsContext(f.store.state)).toBe(false);
  const review = f.store.review(); f.writes[1]!.reply.resolve({ ...reviewed, owner_id: "next-owner" }); await review;
  expect(f.store.state.status?.owner_id).toBe("next-owner"); expect(f.store.state.outcomeUnknown).toBe(false);
  f.reads[2]!.resolve(closed); await settle(); expect(f.store.state.status?.owner_id).toBe("next-owner"); f.store.stopPolling();
});

test("context changes invalidate old reads while remount retains a single live IPC", async () => {
  const reviewed = await ready(); const f = fixture(); f.store.startPolling();
  f.store.setContext({ network: "mainnet", blocked: null }); f.store.setContext({ network: "testnet", blocked: null });
  f.store.stopPolling(); f.store.startPolling(); expect(f.reads).toHaveLength(1);
  f.reads[0]!.resolve(reviewed); await settle(); expect(f.store.state.status).toBeNull(); expect(f.reads).toHaveLength(2);
  f.reads[1]!.resolve(null); await settle(); expect(f.writes).toHaveLength(0); f.store.stopPolling();
});

test("lost consent reply retains uncertainty through null and stale reads; recovery is explicitly read-only", async () => {
  const reviewed = await ready(); const f = fixture(); await f.open(reviewed);
  const confirming = f.store.confirm(reviewed.owner_id, reviewed.review!.id, attestations);
  f.writes[0]!.reply.reject(new Error("Lost IPC")); await confirming;
  f.reads[1]!.resolve(null); await settle(); expect(f.store.state.outcomeUnknown).toBe(true); expect(consentOwnsContext(f.store.state)).toBe(true);
  let reading = f.store.refresh(); f.reads[2]!.resolve(reviewed); await reading;
  await f.store.confirm(reviewed.owner_id, reviewed.review!.id, attestations); expect(f.writes).toHaveLength(1);
  const reconcile = f.store.reconcile(reviewed.owner_id, reviewed.review!.id);
  expect(f.writes[1]!.kind).toBe("reconcile");
  f.writes[1]!.reply.resolve({ ...reviewed, operation_seq: 2, last_operation: { kind: "reconcile", review_id: reviewed.review!.id }, phase: "uncertain",
    resolution: { status: "unknown", detail: "Unverified" } }); await reconcile;
  expect(f.store.state.status?.receipt).toBeNull(); await f.store.review(); expect(f.writes).toHaveLength(2); f.store.stopPolling();
});

test("terminal native proof frees a hung observer but its late reply cannot clear newer recovery", async () => {
  const reviewed = await ready(); const f = fixture(); await f.open(reviewed);
  const old = f.store.confirm(reviewed.owner_id, reviewed.review!.id, attestations);
  const reading = f.store.refresh(); f.reads[1]!.resolve({ ...reviewed, operation_seq: 2, last_operation: { kind: "confirm", review_id: reviewed.review!.id }, phase: "uncertain" }); await reading;
  expect(f.store.state.command).toBeNull();
  const recovery = f.store.reconcile(reviewed.owner_id, reviewed.review!.id);
  f.writes[0]!.reply.resolve({ ...reviewed, phase: "authorized", operation_seq: 2, last_operation: { kind: "confirm", review_id: reviewed.review!.id } }); await old;
  expect(f.store.state.command).toBe("reconcile"); expect(f.store.state.outcomeUnknown).toBe(true);
  f.writes[1]!.reply.resolve({ ...reviewed, operation_seq: 3, last_operation: { kind: "reconcile", review_id: reviewed.review!.id }, phase: "uncertain",
    resolution: { status: "unknown", detail: "Unknown" } }); await recovery; f.store.stopPolling();
});

test("expired evidence, mainnet and concurrent setup cannot confirm", async () => {
  const reviewed = await ready(); const f = fixture(); await f.open({ ...reviewed, review: { ...reviewed.review!, display: { ...reviewed.review!.display, expires_at_ms: 1 } } });
  await f.store.confirm(reviewed.owner_id, reviewed.review!.id, attestations); expect(f.writes).toHaveLength(0);
  for (const context of [{ network: "mainnet" as const, blocked: null }, { network: "testnet" as const, blocked: "Policy work owns context" }]) {
    f.store.setContext(context); await f.store.review(); expect(f.writes).toHaveLength(0);
  }
  f.store.stopPolling();
});
