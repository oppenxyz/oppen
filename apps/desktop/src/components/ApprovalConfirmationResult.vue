<script setup lang="ts">
import type { ApprovalConfirmation } from "../lib/bridge";
defineProps<{ confirmation: ApprovalConfirmation }>();

function json(value: unknown): string { return JSON.stringify(value, null, 2); }
function executionLabel(value: unknown): string {
  if (value !== null && typeof value === "object" && "status" in value && typeof value.status === "string") {
    if (value.status === "rejected") return "Refused";
    if (value.status === "canceled") {
      if (!("requested" in value) || !("canceled" in value) || !("failed" in value)
        || typeof value.requested !== "number" || typeof value.canceled !== "number" || !Array.isArray(value.failed)
        || !Number.isSafeInteger(value.requested) || !Number.isSafeInteger(value.canceled)
        || value.requested < 0 || value.canceled < 0 || value.canceled > value.requested
        || value.canceled + value.failed.length !== value.requested) return "Cancellation outcome unconfirmed";
      if (value.requested === 0) return "No open orders observed";
      if (value.canceled === value.requested) return "Cancellation acknowledged";
      return value.canceled > 0 ? "Cancellation partially acknowledged" : "No cancellations acknowledged";
    }
    return `${value.status === "resting" || value.status === "filled" ? "Venue" : "Execution"} status: ${value.status}`;
  }
  return "Execution response";
}
function executionErrorLabel(value: unknown): string {
  if (value !== null && typeof value === "object" && "data" in value && value.data !== null
    && typeof value.data === "object" && "action" in value.data && value.data.action === "cancel") return "Cancellation outcome unconfirmed";
  return "Confirmation error · execution unconfirmed";
}
</script>

<template>
  <section class="approval-result" role="status" :aria-label="`Confirmation outcome ${confirmation.proposal_id}`">
    <h3>{{ confirmation.result != null ? executionLabel(confirmation.result) : confirmation.error != null ? executionErrorLabel(confirmation.error) : 'Confirmation pending' }}</h3>
    <p>{{ confirmation.proposal_id }} · Review {{ confirmation.review_id }}</p>
    <p>{{ new Date(confirmation.at_ms).toLocaleString() }}</p>
    <pre v-if="confirmation.result != null">{{ json(confirmation.result) }}</pre>
    <pre v-if="confirmation.error != null">{{ json(confirmation.error) }}</pre>
  </section>
</template>

<style scoped>
.approval-result { min-width: 0; border-top: 1px solid var(--rule-strong); padding-top: var(--s-3); margin-top: var(--s-3); }
.approval-result h3, .approval-result p { margin: 0; overflow-wrap: anywhere; white-space: normal; }
.approval-result h3 { font-size: var(--fs-body-sm); font-weight: 500; color: var(--signal); letter-spacing: 0; }
.approval-result p + p { margin-top: var(--s-2); }
.approval-result pre { white-space: pre-wrap; overflow-wrap: anywhere; font: inherit; margin: var(--s-2) 0; }
</style>
