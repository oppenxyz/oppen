<script setup lang="ts">
import { computed } from "vue";
import { formatLatency, formatUsd, NAV, setView, shell, venueLabel } from "../../stores/shell";
import { progress } from "../../stores/tour";
import UiButton from "../ui/UiButton.vue";
import ApertureMark from "./ApertureMark.vue";
import WordMark from "./WordMark.vue";

const venue = computed(() => venueLabel(shell.feeds));
const latency = computed(() => formatLatency(shell.latencyMs));
const equity = computed(() => formatUsd(shell.equityUsd));
const network = computed(() => shell.network.toUpperCase());
</script>

<template>
  <header class="hdr">
    <button type="button" class="hdr__brand" aria-label="oppen — trade" @click="setView('trade')">
      <ApertureMark :size="16" />
      <WordMark />
    </button>

    <nav class="hdr__nav" aria-label="Primary" data-tour="nav">
      <button
        v-for="item in NAV"
        :key="item.view"
        type="button"
        class="hdr__tab"
        :class="{ 'hdr__tab--active': shell.view === item.view }"
        :aria-current="shell.view === item.view ? 'page' : undefined"
        @click="setView(item.view)"
      >
        {{ item.label }}
      </button>
    </nav>

    <div class="hdr__spacer" />

    <div class="hdr__status">
      <span>LOCAL · <span class="hdr__v">RUNTIME OK</span></span>
      <span>HYPERLIQUID · <span class="hdr__v">{{ venue }}</span></span>
      <span class="hdr__chip" :class="`hdr__chip--${shell.network}`" data-tour="network">{{ network }}</span>
      <span>LAT · <span class="hdr__v">{{ latency }}</span></span>
      <span>EQUITY · <span class="hdr__v hdr__v--signal">{{ equity }}</span></span>
      <button
        type="button"
        class="hdr__setup"
        :class="{ 'hdr__setup--active': shell.view === 'onboarding' }"
        @click="setView('onboarding')"
      >
        SETUP
        <!-- The count is of steps this console can actually verify, matching the
             tracker exactly. It disappears when they are all done rather than
             turning into a tick, so a finished setup is quiet. -->
        <span v-if="progress.done < progress.total" class="hdr__setup-n">
          {{ progress.done }}/{{ progress.total }}
        </span>
      </button>
      <UiButton variant="hazard" size="sm" class="hdr__halt" disabled title="Kill switch arrives with the guardrail phase">
        HALT ALL
      </UiButton>
    </div>
  </header>
</template>

<style scoped>
.hdr {
  display: flex;
  flex: none;
  align-items: stretch;
  gap: var(--s-6);
  height: var(--header-h);
  padding: 0 var(--s-4);
  border-bottom: 1px solid var(--rule);
}

.hdr__brand {
  display: flex;
  align-items: center;
  gap: 10px;
}

.hdr__nav {
  display: flex;
  align-items: stretch;
  gap: 2px;
}

.hdr__tab {
  display: flex;
  align-items: center;
  margin-bottom: -1px;
  padding: 0 var(--s-3);
  border-bottom: 1px solid transparent;
  font-size: var(--fs-label-lg);
  letter-spacing: var(--ls-nav);
  text-transform: uppercase;
  color: var(--bracket);
}

.hdr__tab:hover {
  color: var(--body);
}

.hdr__tab--active,
.hdr__tab--active:hover {
  border-bottom-color: var(--signal);
  color: var(--signal);
}

.hdr__spacer {
  flex: 1;
}

.hdr__status {
  display: flex;
  align-items: center;
  gap: var(--s-4);
  font-size: var(--fs-label-lg);
  letter-spacing: 0.14em;
  text-transform: uppercase;
  white-space: nowrap;
  color: var(--bracket);
}

.hdr__v {
  color: var(--body);
}

.hdr__v--signal {
  color: var(--signal);
}

.hdr__chip {
  padding: var(--s-1) var(--s-2);
  border: 1px solid currentColor;
  letter-spacing: var(--ls-chip);
}

.hdr__chip--testnet {
  color: var(--uranium);
}

.hdr__chip--mainnet {
  color: var(--signal);
}

.hdr__setup {
  border-bottom: 1px solid var(--rule);
  color: var(--bracket);
}

.hdr__setup:hover,
.hdr__setup--active {
  border-bottom-color: var(--signal);
  color: var(--signal);
}

.hdr__setup-n {
  margin-left: var(--s-1);
  color: var(--uranium);
}

.hdr__halt {
  padding: 6px 10px;
  font-size: var(--fs-label-lg);
  letter-spacing: var(--ls-chip);
}
</style>
