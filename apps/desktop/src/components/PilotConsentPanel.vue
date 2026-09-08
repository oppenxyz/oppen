<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, watch } from "vue";
import UiButton from "./ui/UiButton.vue";
import ReadoutRows from "./housing/ReadoutRows.vue";
import { pilotConsent, consentOwnsContext, consentCanEditIdentity, consentReviewBlocker } from "../stores/pilot-consent";
import { policyFields, policyFieldLabel } from "../lib/policy-review";
import type { PilotConsentAttestations } from "../lib/bridge";

const state = pilotConsent.state;
const draft = pilotConsent.draft;
const confirmations = reactive<PilotConsentAttestations>({ typed_account: "", never_used_for_in_scope_trading: false,
  dedicated_account_exclusive_use: false, original_baseline_and_no_reset_confirmed: false });
const now = ref(Date.now());
let timer: ReturnType<typeof setInterval> | null = null;
onMounted(() => { timer = setInterval(() => { now.value = Date.now(); }, 250); });
onUnmounted(() => { if (timer) clearInterval(timer); });
const review = computed(() => state.status?.review);
const blocked = computed(() => pilotConsent.blocker());
const evidenceError = computed(() => consentReviewBlocker(state.status, now.value));
const owns = computed(() => consentOwnsContext(state));
const canEditIdentity = computed(() => consentCanEditIdentity(state));
watch(() => JSON.stringify([state.context, state.status?.owner_id, state.status?.operation_seq, state.status?.phase, review.value, !!blocked.value, !!evidenceError.value]), () => {
  confirmations.typed_account = ""; confirmations.never_used_for_in_scope_trading = false;
  confirmations.dedicated_account_exclusive_use = false; confirmations.original_baseline_and_no_reset_confirmed = false;
}, { flush: "sync" });
const canConfirm = computed(() => !blocked.value && !evidenceError.value && state.status?.phase === "review_ready"
  && /^0x[0-9a-fA-F]{40}$/.test(confirmations.typed_account) && confirmations.typed_account.toLowerCase() === state.status.account.toLowerCase()
  && confirmations.never_used_for_in_scope_trading && confirmations.dedicated_account_exclusive_use && confirmations.original_baseline_and_no_reset_confirmed);
const caps = computed(() => {
  const display = review.value?.display;
  return display ? [
    { k: "Per order · USD", v: display.order_limit_usd }, { k: "Cumulative executed notional · USD", v: display.executed_limit_usd },
    { k: "Realized loss including fees · USD", v: display.realized_loss_limit_usd },
    { k: "Gross exposure including resting opening orders · USD", v: display.gross_exposure_limit_usd },
    { k: "Maximum leverage", v: `${display.max_leverage}x` },
  ] : [];
});
const sections = computed(() => {
  const display = review.value?.display;
  if (!display) return [];
  return [
    { title: "Identity and original baseline", value: display.correlation },
    { title: "Authenticated policy", value: { revision: display.policy_revision, policy: display.policy } },
    { title: "Persisted stops", value: display.persisted_kill },
    { title: "Account observation", value: display.account }, { title: "Wallet approval", value: display.wallet_approval },
    { title: "Observed history coverage", value: display.coverage },
    { title: "Required statements and expiry", value: { required_statements: display.required_attestations,
      observed_at_ms: display.observed_at_ms, expires_at_ms: display.expires_at_ms } },
  ].map(section => ({ title: section.title, fields: policyFields(section.value) }));
});
const results = computed(() => [
  ...(state.status?.existing ? [{ title: "Existing consent - inspect only", value: state.status.existing }] : []),
  ...(state.status?.receipt ? [{ title: "Authenticated consent receipt", value: state.status.receipt }] : []),
  ...(state.status?.resolution ? [{ title: "Read-only outcome reconciliation", value: state.status.resolution }] : []),
].map(section => ({ title: section.title, fields: policyFields(section.value) })));
function confirm() { if (canConfirm.value && state.status && review.value) void pilotConsent.confirm(state.status.owner_id, review.value.id, { ...confirmations }); }
</script>

<template>
  <section class="consent" aria-labelledby="consent-heading">
    <h2 id="consent-heading">Initial TESTNET pilot consent</h2>
    <p>Existing registry, authenticated paused policy and anchored history are required. Consent does not start MCP, activate orders, release stops or submit a venue action.</p>
    <p>Status: {{ state.status?.phase.split('_').join(' ') ?? (state.observed ? 'No consent owner' : 'Not observed') }}</p>
    <p v-if="state.status">{{ state.status.agent }} · {{ state.status.account }} · Owner {{ state.status.owner_id }}</p>
    <p v-if="blocked" class="warning" role="status">{{ blocked }}</p>
    <p v-if="state.readError" class="warning" role="status">{{ state.readError }}</p>
    <p v-if="state.commandError" class="warning" role="alert">{{ state.commandError }}</p>
    <p v-if="state.status?.error" class="warning" role="alert">{{ state.status.error.detail }}</p>
    <p v-if="state.outcomeUnknown || ['uncertain', 'recovery_required'].includes(state.status?.phase ?? '')" class="warning">Consent outcome or owner teardown is unverified. Retained receipts are historical evidence, not permission to proceed. No authorization retry or accounting reset has been submitted.</p>
    <p v-for="notice in state.priorUnknown" :key="notice" class="warning">{{ notice }}</p>
    <div class="identity">
      <label>Existing agent ID<input v-model="draft.agent" :disabled="!canEditIdentity" spellcheck="false" autocomplete="off" /></label>
      <label>Dedicated TESTNET account<input v-model="draft.account" :disabled="!canEditIdentity" spellcheck="false" autocomplete="off" /></label>
    </div>
    <div class="actions">
      <UiButton :disabled="!!blocked || owns || !canEditIdentity" @click="pilotConsent.review">Review initial consent</UiButton>
      <UiButton :disabled="state.reading" @click="pilotConsent.refresh">Refresh consent status</UiButton>
    </div>
    <p v-if="state.status?.existing?.authentication === 'legacy_review_required'" class="warning">Legacy consent requires a separate preservation review. No fresh authorization, replacement or budget reset is available here.</p>
    <p v-else-if="state.status?.existing">Existing consent is inspect-only. Account changes, restart, profit and cancellations do not renew consent or reset usage or stops.</p>
    <template v-if="review">
      <h3>Immutable pilot limits</h3><ReadoutRows :rows="caps" />
      <p>Observation coverage is bounded, not lifetime-history verification. Previously used or uncertain account history cannot use this zero-baseline ceremony; known contradictory evidence overrides any statement below.</p>
      <section v-for="section in sections" :key="section.title" class="evidence">
        <h3>{{ section.title }}</h3><dl><template v-for="row in section.fields" :key="JSON.stringify(row.path)">
          <dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd>
        </template></dl>
      </section>
      <template v-if="state.status?.phase === 'review_ready'">
        <p v-if="evidenceError" class="warning">{{ evidenceError }}</p>
        <label>Type the full reviewed account: {{ state.status.account }}<input v-model="confirmations.typed_account" :disabled="!!blocked || !!evidenceError" autocomplete="off" spellcheck="false" /></label>
        <label class="check"><input v-model="confirmations.never_used_for_in_scope_trading" type="checkbox" :disabled="!!blocked || !!evidenceError" /><span>This account has never previously been used for in-scope trading. Its history is not uncertain.</span></label>
        <label class="check"><input v-model="confirmations.dedicated_account_exclusive_use" type="checkbox" :disabled="!!blocked || !!evidenceError" /><span>This is the dedicated TESTNET account shown above and I confirm its exclusive use for this pilot.</span></label>
        <label class="check"><input v-model="confirmations.original_baseline_and_no_reset_confirmed" type="checkbox" :disabled="!!blocked || !!evidenceError" /><span>I confirm the original baseline and all five limits. Consent is immutable; usage and permanent stops are not reset by restart, profit, cancellation or account changes.</span></label>
        <div class="actions"><UiButton variant="primary" :disabled="!canConfirm" @click="confirm">Confirm initial consent</UiButton>
          <UiButton :disabled="!!blocked" @click="pilotConsent.discard(state.status!.owner_id, review.id)">Discard consent review</UiButton></div>
      </template>
    </template>
    <div v-if="state.status && state.retainedReviewId && (state.outcomeUnknown || state.status.phase === 'uncertain')" class="actions">
      <UiButton :disabled="!!pilotConsent.blocker(true)" @click="pilotConsent.reconcile(state.status!.owner_id, state.retainedReviewId!)">Reconcile consent outcome (read-only)</UiButton>
    </div>
    <section v-for="section in results" :key="section.title" class="evidence">
      <h3>{{ section.title }}</h3><dl><template v-for="row in section.fields" :key="JSON.stringify(row.path)"><dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd></template></dl>
    </section>
    <p v-if="state.status?.receipt && state.status.phase === 'authorized'">Consent recorded. Policy remains paused and activation unacknowledged; supervision has not been started by this operation.</p>
  </section>
</template>

<style scoped>
.consent { min-width: 0; font-size: var(--fs-body); line-height: 1.7; overflow-wrap: anywhere; }
h2, h3 { margin-block: var(--s-3); font: 700 var(--fs-copy) / 1.5 var(--font-mono); letter-spacing: 0; }
p { margin-block: var(--s-3); max-width: 90ch; }
.identity { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--s-3); }
input:not([type=checkbox]) { display: block; box-sizing: border-box; width: 100%; min-width: 0; font: inherit; padding: var(--s-2); color: inherit; background: var(--plate); border: 1px solid var(--rule); }
.evidence { border-top: 1px solid var(--rule); padding-block: var(--s-2); }
dl { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 2fr); gap: var(--s-3); }
dt, dd { min-width: 0; margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.actions { display: flex; flex-wrap: wrap; gap: var(--s-3); margin-block: var(--s-3); }
.check { display: flex; align-items: baseline; gap: var(--s-3); margin-block: var(--s-3); }
.check input { flex: 0 0 auto; }
.warning { color: var(--hazard); }
button { max-width: 100%; white-space: normal; overflow-wrap: anywhere; }
@media (max-width: 600px) { .identity { grid-template-columns: minmax(0, 1fr); } }
</style>
