<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import PanelHousing from "./housing/PanelHousing.vue";
import UiButton from "./ui/UiButton.vue";
import type { ApprovalReviewDisplay, OriginalRequest } from "../lib/bridge";
import { approvals, approvalBinding, APPROVAL_DECISIONS, APPROVAL_PHASES } from "../stores/approvals";
import { runtime } from "../stores/runtime";
import { shell } from "../stores/shell";
import { supervision } from "../stores/supervision";

const reading = approvals.state;
const binding = computed(() => approvalBinding({
  network: shell.network, supervision: supervision.state.status, runtime: runtime.status,
  supervisionError: supervision.state.error, runtimeError: runtime.error,
  stopping: supervision.state.stopRequested || supervision.state.command !== null,
}));
watch(binding, value => approvals.setBinding(value), { immediate: true, flush: "sync" });
const now = ref(Date.now());
let clock: ReturnType<typeof setInterval> | null = null;
onMounted(() => { approvals.start(); clock = setInterval(() => { now.value = Date.now(); }, 1_000); });
onUnmounted(() => { approvals.stop(); if (clock !== null) clearInterval(clock); });
const phase = computed(() => reading.error ? "Unavailable" : reading.status ? APPROVAL_PHASES[reading.status.phase] : "Not read");
const empty = computed(() => reading.error === null && reading.status?.error === null
  && reading.status.phase === "ready" && reading.status.observed_at_ms !== null && reading.status.pending.length === 0);
function date(at: number | null): string { return at === null ? "Not observed" : new Date(at).toLocaleString(); }
function source(original: OriginalRequest | null): string {
  if (original === null) return "Unknown original request";
  const kind = original.kind;
  switch (kind.kind) {
    case "limit": return `Limit ${kind.limit_px} USD · ${kind.tif}`;
    case "market": return `Market · ${kind.slippage_bps} bp slippage`;
    case "stop_market": return `Stop market · ${kind.tpsl.toUpperCase()} ${kind.trigger_px} USD · ${kind.slippage_bps} bp slippage`;
    case "close_position": return `Close position · ${kind.position_size} · ${kind.slippage_bps} bp slippage`;
  }
}
function json(value: unknown): string { return JSON.stringify(value, null, 2); }
function orderType(order: ApprovalReviewDisplay["order_type"]): string {
  if ("limit" in order) {
    const labels = { Alo: "post only", Ioc: "immediate or cancel", Gtc: "good till canceled" };
    return `${order.limit.tif} · ${labels[order.limit.tif]}`;
  }
  const trigger = order.trigger;
  return `${trigger.tpsl === "tp" ? "Take profit" : "Stop loss"} · ${trigger.isMarket ? "Market" : "Limit"} trigger ${trigger.triggerPx} USD`;
}
function executionLabel(value: unknown): string {
  if (value !== null && typeof value === "object" && "status" in value && typeof value.status === "string") {
    if (value.status === "rejected") return "Refused";
    return `${value.status === "resting" || value.status === "filled" ? "Venue" : "Execution"} status: ${value.status}`;
  }
  return "Execution response";
}
</script>

<template>
  <PanelHousing inset label="Live approvals" :meta="phase" :brackets="['br']">
    <div class="approval-queue">
      <p v-if="!reading.binding" role="status">No current TESTNET listening binding.</p>
      <template v-else>
        <dl class="approval-queue__binding">
          <dt>Agent</dt><dd>{{ reading.binding.agent }}</dd>
          <dt>Account</dt><dd>{{ reading.binding.account }}</dd>
          <dt v-if="reading.status">Queue owner</dt><dd v-if="reading.status">{{ reading.status.owner_id }}</dd>
          <dt>Observed</dt><dd>{{ date(reading.status?.observed_at_ms ?? null) }}</dd>
        </dl>
        <div class="approval-queue__actions">
          <UiButton size="sm" :disabled="!approvals.canRefresh()" @click="approvals.refresh">Refresh</UiButton>
          <span v-if="reading.pending" role="status">{{ reading.pending === 'reject' ? 'Rejection pending' : reading.pending === 'confirm' ? 'Confirmation pending' : 'Reading' }}</span>
        </div>
        <p v-if="reading.error || reading.status?.error" class="approval-queue__error" role="status">{{ reading.error ?? reading.status?.error }}</p>
        <p v-if="reading.status?.phase === 'recovery_required'" class="approval-queue__error">Recovery required. Rejection unavailable.</p>
        <p v-else-if="reading.status?.phase === 'closed'">Queue closed.</p>
        <p v-if="reading.decision" class="approval-queue__decision" role="status">
          {{ APPROVAL_DECISIONS[reading.decision.outcome] }}<br>
          {{ reading.decision.proposal_id }} · {{ date(reading.decision.at_ms) }}
          <span v-if="reading.decision.error"><br>{{ reading.decision.error }}</span>
        </p>
        <p v-if="empty">No pending proposals at the last observation.</p>
        <p v-else-if="reading.status?.phase === 'idle'">Queue not refreshed.</p>
        <p v-else-if="reading.status === null && !reading.error">Queue not read.</p>
        <p v-if="reading.status?.pending.length && (reading.error || reading.status.phase !== 'ready')">Last observed proposals; current queue unconfirmed.</p>
        <ol v-if="reading.status?.pending.length" class="approval-queue__rows" :aria-label="reading.error ? 'Last observed proposals; current queue unavailable' : 'Pending proposals'">
          <li v-for="proposal in reading.status.pending" :key="proposal.id">
            <h3>{{ proposal.symbol }} · {{ proposal.is_buy ? 'Buy' : 'Sell' }}<span v-if="proposal.reduce_only"> · Reduce only</span></h3>
            <p class="approval-queue__id">{{ proposal.id }}</p>
            <dl>
              <dt>Price · USD</dt><dd>{{ proposal.px }}</dd>
              <dt>Size</dt><dd>{{ proposal.sz }}</dd>
              <dt>Expires</dt><dd>{{ date(proposal.expires_at_ms) }}<span v-if="now >= proposal.expires_at_ms"> · Expired locally</span></dd>
            </dl>
            <p>{{ source(proposal.original) }}</p>
            <p v-if="proposal.original">Reference · {{ proposal.original.reference_px ?? 'Unknown' }} USD · {{ date(proposal.original.reference_at_ms) }}</p>
            <p class="approval-queue__reason">{{ proposal.reason }}</p>
            <UiButton v-if="reading.status.review?.display.proposal_id !== proposal.id || !['review_ready', 'confirming'].includes(reading.status.phase)" size="sm" :disabled="now >= proposal.expires_at_ms || !approvals.canPrepare(proposal.id)" @click="approvals.prepare(proposal.id)">Review pricing</UiButton>
            <section v-if="reading.status.review?.display.proposal_id === proposal.id" class="approval-queue__review" aria-label="Retained pricing review">
              <h3>Exact candidate · {{ reading.status.review.display.symbol }} · {{ reading.status.review.display.is_buy ? 'Buy' : 'Sell' }}</h3>
              <p>{{ source(reading.status.review.display.original) }}</p>
              <dl>
                <dt>Proposal price</dt><dd>{{ reading.status.review.display.original_px }} USD</dd>
                <dt>Reference</dt><dd>{{ reading.status.review.display.reference_px }} USD</dd>
                <dt>Quote time</dt><dd>{{ date(reading.status.review.display.reference_at_ms) }}</dd>
                <dt>Drift</dt><dd>{{ reading.status.review.display.drift_bps ?? 'Unknown' }} bp</dd>
              </dl>
              <details><summary>Technical evidence · route and policy</summary><pre>{{ json(reading.status.review) }}</pre></details>
              <p class="approval-queue__reason">{{ reading.status.review.reason }}</p>
              <dl>
                <dt>Builder</dt><dd>{{ reading.status.review.display.builder?.b ?? 'None' }}</dd>
                <template v-if="reading.status.review.display.builder">
                  <dt>Builder fee</dt><dd>{{ reading.status.review.display.builder.f }} tenths of a basis point</dd>
                </template>
                <dt>Account</dt><dd>{{ reading.status.review.display.account }}</dd>
                <dt>Pairing</dt><dd>{{ reading.status.review.pairing_id.network }} · {{ reading.status.review.pairing_id.issued_seq }}</dd>
                <dt>Review expires</dt><dd>{{ date(reading.status.review.display.expires_at_ms) }}<span v-if="now >= reading.status.review.display.expires_at_ms"> · Expired</span></dd>
              </dl>
              <div class="approval-queue__candidate" aria-label="Candidate to submit">
                <h3>{{ reading.status.review.display.symbol }} · {{ reading.status.review.display.is_buy ? 'Buy' : 'Sell' }}</h3>
                <p>{{ reading.status.review.display.sz }} @ {{ reading.status.review.display.px }} USD</p>
                <p>{{ reading.status.review.display.notional_usd }} USD notional · {{ reading.status.review.display.reduce_only ? 'Reduce only' : 'Not reduce only' }}</p>
                <p>{{ orderType(reading.status.review.display.order_type) }}</p>
              </div>
              <div class="approval-queue__actions">
                <UiButton size="sm" variant="hazard" :disabled="now >= reading.status.review.display.expires_at_ms || !approvals.canConfirm()" @click="approvals.confirm">Confirm and submit</UiButton>
                <UiButton size="sm" :disabled="!approvals.canDiscard()" @click="approvals.discard">Discard review</UiButton>
              </div>
            </section>
            <UiButton v-if="reading.confirmation?.proposal.id !== proposal.id" size="sm" :disabled="now >= proposal.expires_at_ms || !approvals.canReject(proposal.id)" @click="approvals.prepareReject(proposal.id)">Reject</UiButton>
            <section v-else-if="reading.confirmation" class="approval-queue__confirm" aria-label="Confirm proposal rejection">
              <h3>Reject proposal?</h3>
              <p>{{ reading.confirmation.proposal.symbol }} · {{ reading.confirmation.proposal.is_buy ? 'Buy' : 'Sell' }} · {{ reading.confirmation.proposal.reduce_only ? 'Reduce only' : 'Order' }}</p>
              <dl>
                <dt>Price · USD</dt><dd>{{ reading.confirmation.proposal.px }}</dd>
                <dt>Size</dt><dd>{{ reading.confirmation.proposal.sz }}</dd>
                <dt>Agent</dt><dd>{{ reading.confirmation.proposal.agent }}</dd>
                <dt>Account</dt><dd>{{ reading.confirmation.proposal.account }}</dd>
                <dt>Proposal</dt><dd>{{ reading.confirmation.proposal.id }}</dd>
              </dl>
              <p class="approval-queue__reason">{{ reading.confirmation.proposal.reason }}</p>
              <div class="approval-queue__actions">
                <UiButton size="sm" variant="hazard" :disabled="now >= proposal.expires_at_ms || !approvals.canReject(proposal.id)" @click="approvals.reject">Confirm rejection</UiButton>
                <UiButton size="sm" @click="approvals.cancelReject">Cancel</UiButton>
              </div>
            </section>
          </li>
        </ol>
        <section v-if="reading.execution" class="approval-queue__decision" role="status">
          <h3>{{ reading.execution.result != null ? executionLabel(reading.execution.result) : reading.execution.error != null ? 'Confirmation error · execution unconfirmed' : 'Confirmation pending' }}</h3>
          <p>{{ reading.execution.proposal_id }} · Review {{ reading.execution.review_id }}</p>
          <p>{{ date(reading.execution.at_ms) }}</p>
          <pre v-if="reading.execution.result != null">{{ json(reading.execution.result) }}</pre>
          <pre v-if="reading.execution.error != null">{{ json(reading.execution.error) }}</pre>
        </section>
      </template>
    </div>
  </PanelHousing>
</template>

<style scoped>
.approval-queue { min-width: 0; font-size: var(--fs-body-sm); line-height: 1.55; letter-spacing: 0; }
.approval-queue p, .approval-queue dd, .approval-queue h3 { margin: 0; overflow-wrap: anywhere; white-space: normal; }
.approval-queue p + p { margin-top: var(--s-2); }
.approval-queue dl { display: grid; grid-template-columns: minmax(0, 85px) minmax(0, 1fr); gap: var(--s-1) var(--s-2); margin: 0 0 var(--s-3); }
.approval-queue dt, .approval-queue__id { color: var(--bracket); }
.approval-queue__actions { display: flex; flex-wrap: wrap; align-items: center; gap: var(--s-2); margin: var(--s-3) 0; }
.approval-queue__actions :deep(button) { white-space: normal; overflow-wrap: anywhere; max-width: 100%; letter-spacing: 0; }
.approval-queue__rows { padding: 0; margin: var(--s-3) 0 0; list-style: none; }
.approval-queue__rows li { min-width: 0; padding: var(--s-3) 0; border-top: 1px solid var(--rule); }
.approval-queue h3 { font-size: var(--fs-body-sm); font-weight: 500; color: var(--signal); letter-spacing: 0; }
.approval-queue__rows dl { margin-top: var(--s-2); }
.approval-queue .approval-queue__reason { white-space: pre-wrap; margin: var(--s-3) 0; }
.approval-queue__error { color: var(--hazard); }
.approval-queue__decision, .approval-queue__confirm { border-top: 1px solid var(--rule-strong); padding-top: var(--s-3); margin-top: var(--s-3); }
.approval-queue__review { border-top: 1px solid var(--rule-strong); padding-top: var(--s-3); margin: var(--s-3) 0; }
.approval-queue pre { white-space: pre-wrap; overflow-wrap: anywhere; font: inherit; margin: var(--s-2) 0; }
.approval-queue summary { cursor: pointer; }
</style>
