<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import UiButton from "./ui/UiButton.vue";
import ReadoutRows from "./housing/ReadoutRows.vue";
import { activation, activationReviewBlocker } from "../stores/activation";
import { activationConsentKey, activationSections } from "../lib/activation-review";
import { policyFields, policyFieldLabel } from "../lib/policy-review";

const state = activation.state;
const confirmed = ref(false);
const now = ref(Date.now());
let timer: ReturnType<typeof setInterval> | null = null;
onMounted(() => { timer = setInterval(() => { now.value = Date.now(); }, 250); });
onUnmounted(() => { if (timer) clearInterval(timer); });
const blocked = computed(() => activation.blocker());
const review = computed(() => state.status?.review);
const evidenceBlocker = computed(() => activationReviewBlocker(state.status, now.value));
const consentKey = computed(() => activationConsentKey(state.status, JSON.stringify(state.context), !!blocked.value || !!evidenceBlocker.value));
watch(consentKey, () => { confirmed.value = false; }, { flush: "sync" });
const sections = computed(() => review.value ? activationSections(review.value.display) : []);
const receiptRows = computed(() => state.status?.receipt ? policyFields(state.status.receipt) : []);
const phaseLabel = computed(() => state.status ? ({
  idle: "Not reviewed", reviewing: "Reviewing", review_ready: "Review ready", confirming: "Confirmation in progress",
  acknowledged: "Activation acknowledged", refused: "Activation refused", uncertain: "Activation outcome uncertain", closed: "Activation owner closed",
})[state.status.phase] : "Not observed");
const canReview = computed(() => !blocked.value && ["idle", "refused", "acknowledged"].includes(state.status?.phase ?? ""));
const canConfirm = computed(() => !blocked.value && !evidenceBlocker.value && confirmed.value && state.status?.phase === "review_ready");
const canDiscard = computed(() => !blocked.value && !!review.value && state.status?.phase === "review_ready");
const rows = computed(() => [
  { k: "Activation", v: phaseLabel.value },
  { k: "Supervised agent", v: state.context?.agent ?? "Not bound" },
  { k: "Supervised account", v: state.context?.account ?? "Not bound" },
  { k: "Last status observed", v: state.checkedAt ? new Date(state.checkedAt).toLocaleString() : "Not observed" },
  { k: "Cached policy revision", v: state.status?.policy_status.cached_revision != null ? String(state.status.policy_status.cached_revision) : "Unknown" },
  { k: "Cached stop generation", v: state.status ? String(state.status.policy_status.stop_generation) : "Unknown" },
  { k: "Cached acknowledgment", v: state.status?.policy_status.acknowledgment
    ? `Revision ${state.status.policy_status.acknowledgment.revision}, stop generation ${state.status.policy_status.acknowledgment.stop_generation}` : "None observed" },
  { k: "Local admission inhibited", v: state.status ? state.status.policy_status.admission_inhibited ? "Yes" : "No - not current order eligibility" : "Unknown" },
]);
const reviewRows = computed(() => {
  const display = review.value?.display;
  return display ? [
    { k: "Policy revision", v: String(display.policy_revision) },
    { k: "Stop generation", v: String(display.stop_generation) },
    { k: "Gross exposure · USD", v: display.gross_exposure_usd },
    { k: "Remaining committed capacity · USD", v: display.remaining_committed_usd },
    { k: "Evidence observed", v: new Date(display.observed_at_ms).toLocaleString() },
    { k: "Review expires", v: new Date(display.expires_at_ms).toLocaleString() },
    { k: "Venue wallet approval expires", v: new Date(display.wallet_approval.validUntil).toLocaleString() },
  ] : [];
});
function confirm(): void {
  if (canConfirm.value && state.status && review.value) void activation.confirm(state.status.owner_id, review.value.id, confirmed.value);
}
function discard(): void {
  if (canDiscard.value && state.status && review.value) void activation.discard(state.status.owner_id, review.value.id);
}
</script>

<template>
  <section class="activation" aria-labelledby="activation-heading">
    <h2 id="activation-heading">TESTNET activation</h2>
    <ReadoutRows :rows="rows" />
    <p>Cached policy status is not a fresh authority check or venue acceptance.</p>
    <p>Starting a new review inhibits order admission until confirmation. It does not release stops or reset budgets.</p>
    <p v-if="blocked" class="warning" role="status">{{ blocked }}</p>
    <p v-if="state.readError" class="warning" role="status">{{ state.readError }}</p>
    <p v-if="state.commandError" class="warning" role="alert">{{ state.commandError }}</p>
    <p v-for="notice in state.previousUnknown" :key="notice" class="warning" role="status">{{ notice }}</p>
    <p v-if="state.status?.error" class="warning" role="alert">{{ state.status.error.kind }}: {{ state.status.error.detail }}</p>
    <p v-if="state.outcomeUnknown || state.status?.phase === 'uncertain'" class="warning">Completion is unconfirmed. Do not assume orders are eligible. No confirmation retry has been submitted.</p>
    <div class="actions">
      <UiButton :disabled="!canReview" @click="activation.review">Review activation</UiButton>
      <UiButton size="sm" :disabled="state.reading || !state.context || !!state.context.blocked" @click="activation.refresh">Refresh activation status</UiButton>
    </div>
    <template v-if="review">
      <h3>Review {{ review.id }}</h3>
      <p>Owner: {{ state.status?.owner_id }}</p>
      <ReadoutRows :rows="reviewRows" />
      <section v-for="section in sections" :key="section.title" class="evidence">
        <h3>{{ section.title }}</h3>
        <dl><template v-for="row in section.fields" :key="JSON.stringify(row.path)">
          <dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd>
        </template></dl>
      </section>
      <p v-if="state.status?.phase === 'review_ready' && evidenceBlocker" class="warning" role="status">{{ evidenceBlocker }}</p>
      <template v-if="state.status?.phase === 'review_ready'">
        <label class="check"><input v-model="confirmed" type="checkbox" :disabled="!!blocked || !!evidenceBlocker" />
          <span>I confirm TESTNET agent {{ review.display.route.binding.agent }} and account {{ review.display.route.binding.container }}. I reviewed the wallet approval, limits, pilot budget and account evidence above. Confirmation may allow orders under existing authority; it does not release stops or reset budgets.</span>
        </label>
        <div class="actions">
          <UiButton variant="primary" :disabled="!canConfirm" @click="confirm">Confirm activation</UiButton>
          <UiButton :disabled="!canDiscard" @click="discard">Discard review</UiButton>
        </div>
      </template>
    </template>
    <template v-if="state.status?.receipt">
      <h3>Acknowledgment receipt</h3>
      <dl><template v-for="row in receiptRows" :key="JSON.stringify(row.path)">
        <dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd>
      </template></dl>
      <p>Acknowledgment is not current order eligibility or venue acceptance. Orders still require current guardrails, feed and authority checks. Positions may remain open.</p>
    </template>
  </section>
</template>

<style scoped>
.activation { min-width: 0; margin-top: var(--s-5); padding-top: var(--s-4); border-top: 1px solid var(--rule-strong); font-size: var(--fs-body); line-height: 1.7; overflow-wrap: anywhere; }
h2, h3 { margin-block: var(--s-3); font: 700 var(--fs-copy) / 1.5 var(--font-mono); letter-spacing: 0; }
p { margin-block: var(--s-3); max-width: 90ch; }
.evidence { min-width: 0; border-top: 1px solid var(--rule); padding-block: var(--s-2); }
dl { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 2fr); gap: var(--s-3); margin-block: var(--s-3); }
dt, dd { min-width: 0; margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.actions { display: flex; flex-wrap: wrap; gap: var(--s-3); margin-block: var(--s-3); }
.check { display: flex; align-items: baseline; gap: var(--s-3); margin-block: var(--s-3); }
.check input { flex: 0 0 auto; }
.warning { color: var(--hazard); }
button { max-width: 100%; white-space: normal; overflow-wrap: anywhere; }
</style>
