<script setup lang="ts">
import TerrainField from "../components/ascii/TerrainField.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import WordMark from "../components/shell/WordMark.vue";
import SetupTracker from "../components/tour/SetupTracker.vue";
import UiButton from "../components/ui/UiButton.vue";
import { setView, shell } from "../stores/shell";
import { startTour, TOUR } from "../stores/tour";
</script>

<template>
  <div class="onboarding">
    <PanelHousing class="cover">
      <div class="cover__inner">
        <div class="cover__field">
          <TerrainField />
        </div>
        <div class="cover__mark">
          <div class="cover__plate"><i v-for="corner in ['tl', 'tr', 'bl', 'br']" :key="corner" :class="`corner corner--${corner}`" aria-hidden="true" /><WordMark size="cover" accent /></div>
          <div class="cover__tagline">Local-first · Agentic · Perps</div>
        </div>
      </div>
    </PanelHousing>

    <div class="onboarding__side">
      <PanelHousing inset label="Next step">
        <p class="copy">{{ shell.account ? 'Continue with the external MCP setup. Agent registration and approvals are not yet available in this console.' : 'Connect an account to read balances and positions. Public market data is available without one.' }}</p>
        <UiButton @click="setView('builder')">Open MCP setup</UiButton>
      </PanelHousing>
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

.cover__plate { position: relative; padding: 26px 32px; background: #000; }
.corner { position: absolute; width: 14px; height: 14px; border-color: var(--signal); border-style: solid; border-width: 0; }
.corner--tl { top: 0; left: 0; border-top-width: 1px; border-left-width: 1px; }
.corner--tr { top: 0; right: 0; border-top-width: 1px; border-right-width: 1px; }
.corner--bl { bottom: 0; left: 0; border-bottom-width: 1px; border-left-width: 1px; }
.corner--br { bottom: 0; right: 0; border-bottom-width: 1px; border-right-width: 1px; }
.cover__tagline {
  margin-top: var(--s-5);
  font-size: var(--fs-label-lg);
  letter-spacing: var(--ls-tagline);
  text-transform: uppercase;
  color: var(--bracket);
}

.onboarding__side {
  display: grid;
  grid-auto-rows: max-content;
  align-content: start;
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
