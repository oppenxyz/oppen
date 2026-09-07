<script setup lang="ts">
/**
 * How far this machine is from a first agent placing a first order
 * (`docs/spec.md` item 4).
 *
 * Three states, not two. A milestone the console cannot check reads
 * **unverifiable** and says what is missing, rather than sitting unticked
 * beside steps that really are pending — an operator waiting on a box that
 * nothing will ever tick is worse served than one told the check does not
 * exist yet.
 */
import AsciiGauge from "../ascii/AsciiGauge.vue";
import PanelHousing from "../housing/PanelHousing.vue";
import { milestones, progress } from "../../stores/tour";

const MARK: Record<string, string> = {
  done: "[×]",
  pending: "[ ]",
  unverifiable: "[?]",
};
</script>

<template>
  <PanelHousing inset label="Setup · first agent" :brackets="['tr']">
    <template #meta>
      <AsciiGauge :value="progress.fraction" :cells="12" label="Verifiable checks complete" />
      <span>{{ progress.done }} / {{ progress.total }} verifiable</span>
    </template>

    <ol class="track">
      <li v-for="m in milestones" :key="m.id" class="track__row" :class="`track__row--${m.state}`">
        <span class="track__mark" aria-hidden="true">{{ MARK[m.state] }}</span>
        <span class="track__text">
          <span class="track__label">{{ m.label }}</span>
          <span class="track__detail">{{ m.detail }}</span>
          <details v-if="m.blocked" class="track__blocked"><summary>Why this is unknown</summary>{{ m.blocked }}</details>
        </span>
        <span class="track__state">{{ m.state === "unverifiable" ? "unknown" : m.state }}</span>
      </li>
    </ol>

    <p class="track__note">
      {{ progress.done }} checks complete · {{ progress.total - progress.done }} pending · {{ progress.unverifiable }} unknown. Unknown checks are excluded from the completion gauge.
    </p>
  </PanelHousing>
</template>

<style scoped>
.track {
  display: grid;
  margin: 0;
  padding: 0;
  list-style: none;
}

.track__row {
  display: grid;
  grid-template-columns: auto 1fr auto;
  gap: var(--s-3);
  align-items: start;
  padding: var(--s-3);
  border-bottom: 1px solid var(--rule);
}

.track__mark {
  font-size: var(--fs-body);
  color: var(--bracket);
}

.track__row--done .track__mark {
  color: var(--up);
}

.track__row--unverifiable .track__mark {
  color: var(--uranium);
}

.track__text {
  display: grid;
  gap: var(--s-1);
  min-width: 0;
}

.track__label {
  color: var(--signal-dim);
}

.track__row--done .track__label {
  color: var(--signal);
}

.track__detail {
  font-size: var(--fs-body);
  line-height: 1.45;
  color: var(--body-dim);
}

.track__blocked {
  font-size: var(--fs-body-sm);
  line-height: 1.45;
  color: var(--bracket);
}

.track__state {
  font-size: var(--fs-label);
  letter-spacing: var(--ls-chip);
  text-transform: uppercase;
  color: var(--bracket);
}

.track__note {
  margin: 0;
  padding: var(--s-3);
  font-size: var(--fs-body);
  line-height: 1.5;
  color: var(--body-dim);
}
</style>
