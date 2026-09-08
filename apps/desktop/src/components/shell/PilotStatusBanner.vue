<script setup lang="ts">
import { computed } from "vue";
import { pilot, pilotAuthentication, pilotNotice } from "../../stores/pilot";

const notice = computed(() => pilotNotice(pilot));
const checkedAt = computed(() => pilot.checkedAt === null ? null : new Date(pilot.checkedAt).toLocaleTimeString());
</script>

<template>
  <section v-if="notice" class="pilot-banner" aria-label="Pilot supervision" role="status" aria-live="polite">
    <div class="pilot-banner__summary">
      <strong class="pilot-banner__title">{{ notice.title }}</strong>
      <span class="pilot-banner__network">{{ pilot.network }}</span>
      <span v-if="pilot.error && checkedAt" class="pilot-banner__stale">Stale / last observed {{ checkedAt }}</span>
      <span v-if="pilot.status" class="pilot-banner__identity">{{ pilot.status.agent }} / {{ pilot.status.account }}</span>
    </div>
    <p>{{ notice.detail }} Orders and positions may remain open.</p>
    <p v-if="pilot.status">{{ pilotAuthentication(pilot.status) }}. Authentication does not enable trading.</p>
    <dl v-if="pilot.status?.accounting === 'known'" class="pilot-banner__totals">
      <div><dt>Executed</dt><dd>{{ pilot.status.executed_usd }} USD</dd></div>
      <div><dt>Reserved</dt><dd>{{ pilot.status.reserved_usd }} USD</dd></div>
      <div><dt>Net realized P&amp;L</dt><dd>{{ pilot.status.net_realized_pnl_usd }} USD</dd></div>
    </dl>
    <p v-else-if="pilot.status?.accounting === 'unavailable' && pilot.status.halt" class="pilot-banner__error">
      Accounting unavailable: {{ pilot.status.detail }}
    </p>
    <p v-if="pilot.error" class="pilot-banner__error">Local status read failed: {{ pilot.error }}</p>
  </section>
</template>

<style scoped>
.pilot-banner {
  flex: none;
  width: 100%;
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
.pilot-banner__summary {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--s-1) var(--s-3);
  margin-bottom: var(--s-1);
}
.pilot-banner__title {
  color: var(--signal);
  font: 700 var(--fs-copy-sm) / 1.5 var(--font-mono);
}
.pilot-banner__network { text-transform: uppercase; color: var(--bracket); }
.pilot-banner__identity { min-width: 0; font-family: var(--font-mono); font-size: var(--fs-body); }
.pilot-banner__stale, .pilot-banner__error { color: var(--hazard); }
.pilot-banner__totals { display: flex; flex-wrap: wrap; gap: var(--s-1) var(--s-5); margin-top: var(--s-1); }
.pilot-banner__totals > div { display: flex; flex-wrap: wrap; gap: var(--s-2); min-width: 0; }
dt { color: var(--bracket); }
dd { color: var(--signal-dim); font-family: var(--font-mono); }
</style>
