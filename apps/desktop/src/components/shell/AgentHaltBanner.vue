<script setup lang="ts">
import { computed } from "vue";
import { haltNotice, haltReleased, supervision } from "../../stores/supervision";

const reading = supervision.state;
const notice = computed(() => haltNotice(reading));
</script>

<template>
  <section v-if="notice" class="halt-banner" role="status" aria-live="polite" aria-label="Bound agent halt">
    <div class="halt-banner__summary">
      <strong>{{ notice.title }}</strong>
      <span>{{ reading.status?.network.toUpperCase() ?? 'Network unavailable' }} · {{ reading.status?.agent ?? 'Identity unavailable' }}</span>
      <span>{{ reading.status?.account }}</span>
    </div>
    <p>{{ notice.cancellation }}. Durable revision: {{ notice.revision ?? 'Unconfirmed' }}.</p>
    <p v-if="!reading.haltRequested && haltReleased(reading.status)">Historical HALT evidence retained. The verified release does not acknowledge activation or reset pilot accounting.</p>
    <p v-else>Pauses this agent identity, including later account assignments. Requests cancellation only for the supervised account shown. A changed registry route prevents cancellation confirmation.</p>
    <p>Cancellation acknowledgments do not prove the venue is flat. Positions may remain open. No resume is performed.</p>
    <p v-if="reading.status?.halt.error">Persistence: {{ reading.status.halt.error }}</p>
    <p v-if="reading.status?.halt.cancellation_error">Cancellation: {{ reading.status.halt.cancellation_error }}</p>
    <p v-if="reading.haltError">Halt request: {{ reading.haltError }}</p>
    <p v-if="reading.error">Status unavailable: {{ reading.error }} Last observed halt evidence is retained.</p>
  </section>
</template>

<style scoped>
.halt-banner {
  box-sizing: border-box;
  flex: none;
  width: min(100%, 100vw);
  max-width: 100vw;
  min-width: 0;
  padding: var(--s-3) var(--s-4);
  border-bottom: 1px solid var(--rule-strong);
  border-left: 2px solid var(--hazard);
  background: var(--plate);
  color: var(--body);
  font: 400 var(--fs-copy-sm) / 1.5 var(--font-sans);
  letter-spacing: 0;
  overflow-wrap: anywhere;
}
.halt-banner__summary { display: flex; flex-wrap: wrap; gap: var(--s-1) var(--s-3); }
.halt-banner strong { color: var(--hazard); font-family: var(--font-mono); }
.halt-banner p { margin: var(--s-1) 0 0; }
</style>
