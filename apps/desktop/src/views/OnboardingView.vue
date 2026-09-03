<script setup lang="ts">
import AsciiGauge from "../components/ascii/AsciiGauge.vue";
import BootSequence from "../components/ascii/BootSequence.vue";
import CharacterMatrix from "../components/ascii/CharacterMatrix.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import WordMark from "../components/shell/WordMark.vue";
import UiButton from "../components/ui/UiButton.vue";

const STEPS = ["Runtime", "Agent wallet", "Approve · master", "Pair agent"] as const;
const ACTIVE_STEP = 0;
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
      <PanelHousing>
        <ol class="steps">
          <li
            v-for="(step, index) in STEPS"
            :key="step"
            class="step"
            :class="{ 'step--active': index === ACTIVE_STEP }"
            :aria-current="index === ACTIVE_STEP ? 'step' : undefined"
          >
            <span class="step__n">0{{ index + 1 }}</span>
            <span class="step__name">{{ step }}</span>
          </li>
        </ol>
      </PanelHousing>

      <PanelHousing inset label="01 · Installing local runtime" :brackets="['tr']" class="stepbody">
        <template #meta>
          <AsciiGauge :value="(ACTIVE_STEP + 1) / STEPS.length" :cells="16" label="Setup" />
          <span>01 / 04</span>
        </template>
        <BootSequence class="stepbody__boot" />
        <p class="copy stepbody__copy">
          Everything runs on this machine: your keys, your agents, the execution engine. Your master wallet signs three
          approvals once, in your own wallet — it never enters oppen. You start on testnet; mainnet is an explicit
          switch.
        </p>
      </PanelHousing>

      <div class="actions">
        <UiButton block disabled title="Arrives with the keychain phase">Import agent wallet</UiButton>
        <UiButton block variant="primary" disabled title="Arrives with the keychain phase">Generate agent wallet →</UiButton>
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
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-height: 0;
}

.steps {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
}

.step {
  padding: var(--s-3);
  border-right: 1px solid var(--rule);
  border-bottom: 1px solid transparent;
  margin-bottom: -1px;
}

.step:last-child {
  border-right: 0;
}

.step--active {
  border-bottom-color: var(--signal);
}

.step__n {
  display: block;
  font-size: var(--fs-label);
  letter-spacing: var(--ls-chip);
  color: var(--bracket);
}

.step__name {
  display: block;
  margin-top: var(--s-1);
  color: var(--bracket);
}

.step--active .step__name {
  color: var(--signal);
}

.stepbody {
  padding: var(--s-5);
}

.stepbody__boot {
  flex: 1;
  margin-top: var(--s-1);
}

.stepbody__copy {
  max-width: 52ch;
}

.actions {
  display: flex;
  gap: var(--s-2);
}
</style>
