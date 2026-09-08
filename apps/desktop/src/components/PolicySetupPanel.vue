<script setup lang="ts">
import { computed, ref, watch } from "vue";
import UiButton from "./ui/UiButton.vue";
import PolicyReviewDiff from "./PolicyReviewDiff.vue";
import { policyFields, policyFieldLabel } from "../lib/policy-review";
import { policySetup } from "../stores/policy-setup";
import { recordedAgents } from "../stores/operator";

const { state, draft, edits } = policySetup;
const review = computed(() => state.status?.review);
const blocked = computed(() => policySetup.blocker());
const symbol = ref("");
const symbolError = ref("");
const confirmed = ref(false);
watch([() => review.value?.id, () => state.status?.phase], () => { confirmed.value = false; });
const phaseLabel = computed(() => ({
  idle: "No review prepared", reviewing: "Review in progress", review_ready: "Review ready",
  persisting: "Persistence in progress", saved: "Globally paused policy saved", failed: "Policy setup failed",
  uncertain: "Persistence outcome uncertain", recovery_required: "Policy recovery required", stopped: "Policy setup stopped",
})[state.status?.phase ?? "idle"]);
const canEdit = computed(() => !blocked.value && !review.value && ["idle", "failed"].includes(state.status?.phase ?? ""));
const canPersist = computed(() => !blocked.value && confirmed.value && !!review.value
  && ["review_ready", "uncertain"].includes(state.status?.phase ?? ""));
const routeRows = computed(() => review.value ? policyFields(review.value.route) : []);
const sourceRows = computed(() => review.value?.legacy ? policyFields(review.value.legacy) : []);
function addSymbol(): void {
  const value = symbol.value.trim();
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]*(?::[A-Za-z0-9][A-Za-z0-9._-]*)?$/.test(value)) {
    symbolError.value = "Enter a canonical symbol, optionally namespace:name."; return;
  }
  if (!edits.symbols.includes(value)) edits.symbols.push(value);
  symbol.value = "";
  symbolError.value = "";
}
</script>

<template>
  <section class="policy-setup" aria-labelledby="policy-setup-heading">
    <h2 id="policy-setup-heading">TESTNET paused policy setup</h2>
    <p>Requires an existing TESTNET ledger, authentication key and authorized agent/account registry route. Missing prerequisites block setup; this action does not create them.</p>
    <p>Completion is globally paused. This does not activate trading, authorize a pilot, acknowledge policy, create pairing, cancel orders or close positions. An existing signed policy must already be globally paused; this is not an emergency halt.</p>
    <p role="status">{{ state.status ? phaseLabel : 'Policy setup status not read' }}<span v-if="state.checkedAt">. Last observed {{ new Date(state.checkedAt).toLocaleTimeString() }}.</span></p>
    <p v-if="blocked" class="warning" role="status">{{ blocked }}</p>
    <p v-if="state.readError" class="warning" role="status">{{ state.readError }} Previous evidence remains visible.</p>
    <p v-if="state.commandError" class="warning" role="alert">{{ state.commandError }}</p>
    <p v-if="state.status?.error" class="warning" role="alert">{{ state.status.error.kind }}: {{ state.status.error.detail }}</p>
    <p v-if="state.status?.phase === 'recovery_required'" class="warning" role="alert">Outcome uncertain. Controlled restart and reconciliation required; restart alone does not verify publication. Retry, discard and editing are unavailable in this runtime.</p>
    <UiButton size="sm" :disabled="state.reading" @click="policySetup.refresh">Refresh setup status</UiButton>

    <form v-if="!review && state.status?.phase !== 'recovery_required'" class="policy-form" @submit.prevent="policySetup.review()">
      <fieldset :disabled="!canEdit">
        <legend>Existing registry identity</legend>
        <label>Agent ID<input v-model="draft.agent" list="policy-setup-agents" autocomplete="off" /></label>
        <datalist id="policy-setup-agents"><option v-for="agent in recordedAgents" :key="agent" :value="agent" /></datalist>
        <label>Authorized TESTNET account<input v-model="draft.account" placeholder="0x..." autocomplete="off" spellcheck="false" /></label>
        <p>Recorded agent suggestions are unverified. Review authenticates the route in Rust using the existing key.</p>
      </fieldset>
      <fieldset :disabled="!canEdit">
        <legend>Candidate limits</legend>
        <p>Initial proposals apply only if explicitly reviewed and saved. Existing values appear in the complete review; use existing limits to retain them before preparing a replacement candidate.</p>
        <div class="limits">
          <label>Maximum order · USD<input v-model="edits.max_order_usd" inputmode="decimal" autocomplete="off" /></label>
          <label>Maximum position · USD<input v-model="edits.max_position_usd" inputmode="decimal" autocomplete="off" /></label>
          <label>Maximum gross open exposure · USD<input v-model="edits.max_open_exposure_usd" inputmode="decimal" autocomplete="off" /></label>
          <label>Maximum leverage<input v-model.number="edits.max_leverage" type="number" min="1" max="1" step="1" /></label>
        </div>
        <label class="check"><input v-model="edits.approval_required" type="checkbox" /> Approval required</label>
        <p>This setup requires approval ON and 1x leverage, positive order cap at most 15 USD and gross exposure at most 25 USD. Existing values outside this slice remain visible and must be explicitly changed before saving.</p>
        <label>Canonical symbol<input v-model="symbol" autocomplete="off" spellcheck="false" @keydown.enter.prevent="addSymbol" /></label>
        <UiButton size="sm" :disabled="!symbol.trim()" @click="addSymbol">Add symbol</UiButton>
        <p v-if="symbolError" role="alert">{{ symbolError }}</p>
        <div class="symbols">
          <label v-for="item in edits.symbols" :key="item" class="check"><input type="checkbox" checked @change="edits.symbols = edits.symbols.filter(value => value !== item)" />{{ item }}</label>
        </div>
        <p v-if="edits.symbols.length === 0">No symbols selected: orders refused.</p>
        <p>Other risk settings, freshness limits, unrelated agent policies, account limits and existing stops are retained by the native policy owner and shown in review.</p>
      </fieldset>
      <fieldset :disabled="!canEdit">
        <legend>Source acknowledgments</legend>
        <label class="check"><input v-model="draft.writersStopped" type="checkbox" /> I have stopped all other policy writers, including legacy processes.</label>
        <label class="check"><input v-model="draft.emptySourceConfirmed" type="checkbox" /> If the legacy source is absent, I explicitly confirm initialization from an empty source. The review must show that absence.</label>
        <p>An existing source is never replaced by this acknowledgment. Review preserves its raw provenance; unavailable or invalid evidence blocks setup.</p>
      </fieldset>
      <UiButton type="submit" variant="primary" :disabled="!canEdit || !draft.writersStopped || !draft.agent.trim() || !draft.account.trim()">Review paused policy</UiButton>
    </form>

    <template v-if="review">
      <h3>Review {{ review.id }} · {{ review.agent }}</h3>
      <p>Account: {{ review.account }}. Expected policy revision: {{ review.expected_revision ?? 'Initial migration' }}.</p>
      <p>{{ review.legacy ? 'Legacy source is unsigned evidence, not authenticated policy consent.' : 'Existing policy snapshot authenticated during native review.' }} Saving retains unrelated policies and stops. This review does not prove cancellation or flatness.</p>
      <details>
        <summary>Authenticated registry route</summary>
        <dl><template v-for="row in routeRows" :key="JSON.stringify(row.path)"><dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd></template></dl>
      </details>
      <details v-if="review.legacy">
        <summary>Read-only legacy source evidence · {{ review.legacy.file_present ? 'File present' : 'File absent' }}</summary>
        <dl><template v-for="row in sourceRows" :key="JSON.stringify(row.path)"><dt>{{ policyFieldLabel(row.path) }}</dt><dd>{{ row.value }}</dd></template></dl>
      </details>
      <PolicyReviewDiff :before="review.before" :proposed="review.proposed" />
      <template v-if="state.status?.phase === 'review_ready' || state.status?.phase === 'uncertain'">
        <p v-if="state.status.phase === 'uncertain'" class="warning">The write may already be durable. Only an explicit retry of this same retained review is available; no new candidate is generated.</p>
        <label class="check"><input v-model="confirmed" type="checkbox" :disabled="!!blocked" /> I reviewed every changed and retained field for {{ review.agent }} / {{ review.account }} and confirm globally paused persistence only.</label>
        <UiButton variant="primary" :disabled="!canPersist" @click="policySetup.persist(review.id)">{{ state.status.phase === 'uncertain' ? 'Retry this reviewed write' : 'Save globally paused policy' }}</UiButton>
      </template>
      <div v-if="state.status?.phase === 'review_ready' || state.status?.phase === 'failed'" class="actions">
        <UiButton :disabled="!!blocked || !review.before?.guardrails[review.agent]" @click="policySetup.editReviewed(true)">Use existing limits</UiButton>
        <UiButton :disabled="!!blocked" @click="policySetup.editReviewed(false)">Edit candidate</UiButton>
        <UiButton :disabled="!!blocked" @click="policySetup.discard(review.id)">Discard review</UiButton>
      </div>
      <p v-if="state.status?.phase === 'saved'">Native receipt revision: {{ state.status.receipt_revision ?? 'Unavailable' }}. Saved globally paused; trading remains inactive. No stops were released.</p>
      <UiButton v-if="state.status?.phase === 'saved'" :disabled="!!blocked" @click="policySetup.discard(review.id)">Clear completed review</UiButton>
      <p v-if="state.status?.phase === 'stopped'" class="warning">Runtime stopped. Retained evidence is not a new confirmation of write completion. Restart is required.</p>
    </template>
  </section>
</template>

<style scoped>
.policy-setup { min-width: 0; margin-top: var(--s-5); padding-top: var(--s-4); border-top: 1px solid var(--rule-strong); font-size: var(--fs-body); line-height: 1.7; overflow-wrap: anywhere; }
h2, h3 { margin-block: var(--s-3); font: 700 var(--fs-copy) / 1.5 var(--font-mono); letter-spacing: 0; }
p { margin-block: var(--s-3); max-width: 90ch; }
.policy-form { display: grid; gap: var(--s-3); margin-top: var(--s-4); }
fieldset { border: 0; border-top: 1px solid var(--rule); padding: var(--s-3) 0; min-width: 0; }
legend { color: var(--signal); padding-right: var(--s-3); }
label { display: grid; gap: var(--s-2); margin-block: var(--s-3); min-width: 0; }
input:not([type=checkbox]) { box-sizing: border-box; width: 100%; min-width: 0; padding: var(--s-3); border: 1px solid var(--rule-strong); background: var(--void); color: var(--signal); font: inherit; letter-spacing: 0; }
.limits { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--s-3); }
.check { display: flex; align-items: baseline; gap: var(--s-3); }
.check input { flex: 0 0 auto; }
.symbols, .actions { display: flex; flex-wrap: wrap; gap: var(--s-3); margin-block: var(--s-3); }
.warning { color: var(--hazard); }
details { border-block-end: 1px solid var(--rule); padding-block: var(--s-3); }
summary { cursor: pointer; }
dl { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 2fr); gap: var(--s-3); margin-block: var(--s-3); }
dt, dd { min-width: 0; margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
button { max-width: 100%; white-space: normal; overflow-wrap: anywhere; }
</style>
