<script setup lang="ts">
import CharacterMatrix from "../components/ascii/CharacterMatrix.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import WordMark from "../components/shell/WordMark.vue";
import SetupTracker from "../components/tour/SetupTracker.vue";
import UiButton from "../components/ui/UiButton.vue";
import { startTour, TOUR } from "../stores/tour";
</script>

<template>
  <div class="onboarding">
    <PanelHousing class="cover">
      <div class="cover__inner">
        <div class="cover__field">
          <CharacterMatrix size="xl" tone="field" />
        </div>
        <div class="cover__mark">
          <WordMark size="cover" accent />
          <div class="cover__tagline">Local-first · Agentic · Perps</div>
        </div>
      </div>
    </PanelHousing>

    <div class="onboarding__side">
      <SetupTracker />

      <PanelHousing inset label="Walkthrough" :brackets="['tr']" class="walk">
        <template #meta><span>{{ TOUR.length }} stops</span></template>
        <p class="copy walk__copy">
          A guided pass over every screen, pointing at the real controls rather than a picture of
          them. It names what each panel is for, and says plainly where something is not built yet.
        </p>
        <p class="copy walk__copy walk__copy--dim">
          Arrow keys move between stops, Escape leaves. You can start it again any time from
          SETUP in the header.
        </p>
      </PanelHousing>

      <div class="actions">
        <UiButton block disabled title="Arrives with the keychain phase">Import agent wallet</UiButton>
        <UiButton block variant="primary" @click="startTour">Take the walkthrough →</UiButton>
      </div>
    </div>
  </div>
</template>

<style scoped>
.onboarding {
  display: grid;
  flex: 1;
  grid-template-columns: 1fr 1fr;
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.cover__inner {
  position: relative;
  display: flex;
  flex: 1;
  align-items: center;
  justify-content: center;
  min-height: 0;
  overflow: hidden;
}

.cover__field {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}

.cover__mark {
  position: relative;
  text-align: center;
}

.cover__tagline {
  margin-top: var(--s-5);
  font-size: var(--fs-label-lg);
  letter-spacing: var(--ls-tagline);
  text-transform: uppercase;
  color: var(--bracket);
}

.onboarding__side {
  display: grid;
  grid-template-rows: auto 1fr auto;
  overflow-y: auto;
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-height: 0;
}

.walk__copy {
  max-width: 52ch;
  padding: var(--s-3);
}

.walk__copy--dim {
  padding-top: 0;
  color: var(--body-dim);
}

.actions {
  display: flex;
  gap: var(--s-2);
}
</style>
