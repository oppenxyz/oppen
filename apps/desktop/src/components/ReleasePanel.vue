<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import UiButton from "./ui/UiButton.vue";
import ReadoutRows from "./housing/ReadoutRows.vue";
import { policyFields, policyFieldLabel } from "../lib/policy-review";
import { releaseReviewBlocker, type ReleaseController } from "../stores/release";

const props = defineProps<{ controller: ReleaseController }>();
const state = computed(() => props.controller.state);
const selected = ref<"agent" | "global">("agent");
const confirmed = ref(false);
const operationId = ref("");
const now = ref(Date.now());
let timer: ReturnType<typeof setInterval> | null = null;
onMounted(() => { timer = setInterval(() => { now.value = Date.now(); }, 250); });
onUnmounted(() => { if (timer) clearInterval(timer); });
const review = computed(() => state.value.status?.review);
const blocked = computed(() => props.controller.blocker());
const evidenceError = computed(() => releaseReviewBlocker(state.value.status, now.value));
watch(() => JSON.stringify([state.value.context, state.value.status?.owner_id, state.value.status?.operation_seq,
  state.value.status?.phase, review.value, !!blocked.value, !!evidenceError.value]), () => { confirmed.value = false; }, { flush: "sync" });
watch(() => state.value.unresolvedOperationId, value => { if (value) operationId.value = value; }, { immediate: true });
const scopeLabel = computed(() => review.value?.display.scope.scope === "global" ? "Global kill scope"
  : `Agent identity ${review.value?.display.scope.scope === "agent" ? review.value.display.scope.agent : state.value.context?.agent ?? "unbound"}`);
const sections = computed(() => {
  const display = review.value?.display;
  if (!display) return [];
  const { affected, remaining_kill, ...evidence } = display;
  return [
    { title: "Reviewed scope and engagement", fields: policyFields(evidence) },
    ...affected.map((member, index) => ({ title: `Affected member ${index + 1}: ${member.route.binding.agent}`, fields: policyFields(member) })),
    { title: "Stops remaining after this scope release", fields: policyFields(remaining_kill) },
  ];
});
const rows = computed(() => [
  { k: "Release", v: state.value.status?.phase.split("_").join(" ") ?? "Not observed" },
  { k: "Supervised agent", v: state.value.context?.agent ?? "Unbound" },
  { k: "Supervised account", v: state.value.context?.account ?? "Unbound" },
  { k: "Owner", v: state.value.status?.owner_id ?? "Unknown" },
  { k: "Operation sequence", v: String(state.value.status?.operation_seq ?? "Unknown") },
]);
const observations = computed(() => state.value.status ? [
  { title: "Cached policy status (not order eligibility)", fields: policyFields(state.value.status.policy_status) },
  { title: "Cached effective stops", fields: policyFields(state.value.status.cached_effective_kill) },
  ...(state.value.status.receipt ? [{ title: "Release receipt", fields: policyFields(state.value.status.receipt) }] : []),
  ...(state.value.status.resolution ? [{ title: "Read-only reconciliation", fields: policyFields(state.value.status.resolution) }] : []),
] : []);
function requestReview() {
  if (!state.value.context) return;
  void props.controller.review(selected.value === "global" ? { scope: "global" } : { scope: "agent", agent: state.value.context.agent });
}
function confirm() {
  if (state.value.status && review.value) void props.controller.confirm(state.value.status.owner_id, review.value.id, confirmed.value);
}
function discard() {
  if (state.value.status && review.value) void props.controller.discard(state.value.status.owner_id, review.value.id);
}
</script>

<template>
  <section class="release" aria-labelledby="release-heading">
    <h2 id="release-heading">TESTNET kill release</h2>
    <ReadoutRows :rows="rows" />
    <p>Release removes only the reviewed kill scope. Other stops remain. Pilot consent, cumulative usage and budgets are unchanged. Fresh activation review and confirmation are required before orders.</p>
    <p v-if="blocked" class="warning" role="status">{{ blocked }}</p>
    <p v-if="state.readError" class="warning" role="status">{{ state.readError }}</p>
    <p v-if="state.commandError" class="warning" role="alert">{{ state.commandError }}</p>
    <p v-if="state.status?.error" class="warning" role="alert">{{ state.status.error.detail }}</p>
    <p v-if="state.outcomeUnknown || state.status?.phase === 'uncertain'" class="warning">Release completion is unknown. No release retry or activation has been submitted.</p>
    <p v-for="notice in state.previousUnknown" :key="notice" class="warning">{{ notice }}</p>
    <div class="actions">
      <label>Scope <select v-model="selected" :disabled="!!blocked || !!review">
        <option value="agent">Supervised agent identity</option><option value="global">Global kill scope</option>
      </select></label>
      <UiButton :disabled="!!blocked || !!review" @click="requestReview">Review kill release</UiButton>
      <UiButton :disabled="state.reading || !state.context || !!state.context.blocked" @click="controller.refresh">Refresh release status</UiButton>
    </div>
    <section v-for="section in sections" :key="section.title" class="evidence">
      <h3>{{ section.title }}</h3>
      <dl><template v-for="row in section.fields" :key="JSON.stringify(row.path)">
        <dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd>
      </template></dl>
    </section>
    <template v-if="review && state.status?.phase === 'review_ready'">
      <p class="warning">Committing this release upgrades ledger history, even if publication is reported uncertain. Pre-ES38 builds cannot operate this history, including cleanup startup. Keep a compatible build; never delete history or reset accounting to downgrade.</p>
      <p v-if="evidenceError" class="warning">{{ evidenceError }}</p>
      <label class="check"><input v-model="confirmed" type="checkbox" :disabled="!!blocked || !!evidenceError" />
        <span>I confirm TESTNET {{ scopeLabel }} for the full affected roster above, supervised account {{ state.context?.account }}. Remaining stops and original pilot budgets stay unchanged. This does not activate orders, cancel orders or close positions.</span>
      </label>
      <div class="actions">
        <UiButton variant="primary" :disabled="!!blocked || !!evidenceError || !confirmed" @click="confirm">Confirm scope release</UiButton>
        <UiButton :disabled="!!blocked" @click="discard">Discard release review</UiButton>
      </div>
    </template>
    <div class="actions">
      <label class="operation">Durable operation ID <input v-model="operationId" spellcheck="false" autocomplete="off" /></label>
      <UiButton :disabled="!!controller.blocker(true) || !!review || !operationId.trim()" @click="controller.reconcile(operationId)">Reconcile outcome (read-only)</UiButton>
    </div>
    <section v-for="section in observations" :key="section.title" class="evidence">
      <h3>{{ section.title }}</h3>
      <dl><template v-for="row in section.fields" :key="JSON.stringify(row.path)">
        <dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd>
      </template></dl>
    </section>
    <p v-if="state.status?.receipt || state.status?.resolution">A committed release is not order eligibility. Current stops may differ from the receipt; a later HALT remains effective.</p>
  </section>
</template>

<style scoped>
.release { min-width: 0; margin-top: var(--s-5); padding-top: var(--s-4); border-top: 1px solid var(--rule-strong); font-size: var(--fs-body); line-height: 1.7; overflow-wrap: anywhere; }
h2, h3 { margin-block: var(--s-3); font: 700 var(--fs-copy) / 1.5 var(--font-mono); letter-spacing: 0; }
p { margin-block: var(--s-3); max-width: 90ch; }
.evidence { min-width: 0; border-top: 1px solid var(--rule); padding-block: var(--s-2); }
dl { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 2fr); gap: var(--s-3); margin-block: var(--s-3); }
dt, dd { min-width: 0; margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.actions { display: flex; flex-wrap: wrap; align-items: end; gap: var(--s-3); margin-block: var(--s-3); }
.check { display: flex; align-items: baseline; gap: var(--s-3); margin-block: var(--s-3); }
.check input { flex: 0 0 auto; }
.operation { flex: 1 1 280px; min-width: 0; }
input:not([type=checkbox]), select { display: block; box-sizing: border-box; width: 100%; min-width: 0; padding: var(--s-2); font: inherit; color: inherit; background: var(--surface); border: 1px solid var(--rule); }
.warning { color: var(--hazard); }
button { max-width: 100%; white-space: normal; overflow-wrap: anywhere; }
</style>
