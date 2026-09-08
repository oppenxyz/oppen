<script setup lang="ts">
import { computed, nextTick, ref } from "vue";
import { formatLatency, formatUsd, NAV, setView, shell } from "../../stores/shell";
import { marketTransport, marketChannels } from "../../stores/market-health";
import { progress } from "../../stores/tour";
import UiButton from "../ui/UiButton.vue";
import ApertureMark from "./ApertureMark.vue";
import WordMark from "./WordMark.vue";
import { supervision } from "../../stores/supervision";

const latency = computed(() => formatLatency(shell.latencyMs));
const equity = computed(() => formatUsd(shell.equityUsd));
const network = computed(() => shell.network.toUpperCase());
const haltBlocker = computed(() => supervision.haltBlocker(shell.network));
const haltDialog = ref<HTMLDialogElement | null>(null);
const haltIdentity = ref<{ agent: string; account: string } | null>(null);
async function confirmHalt(): Promise<void> {
  const status = supervision.state.status;
  if (haltBlocker.value !== null || !status?.agent || !status.account) return;
  haltIdentity.value = { agent: status.agent, account: status.account };
  await nextTick();
  haltDialog.value?.showModal();
}
async function submitHalt(): Promise<void> {
  const identity = haltIdentity.value;
  haltDialog.value?.close();
  if (identity) await supervision.halt(shell.network, identity);
}
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
      <span>LOCAL · <span class="hdr__v">CONSOLE OPEN</span></span>
      <span data-health="summary">WS · <span class="hdr__v">{{ marketTransport }}</span> · Channels · <span class="hdr__v">{{ marketChannels }}</span></span>
      <span class="hdr__chip" :class="`hdr__chip--${shell.network}`" data-tour="network">{{ network }}</span>
      <span>LAT · <span class="hdr__v">{{ latency }}</span></span>
      <span>EQUITY · <span class="hdr__v hdr__v--signal">{{ equity }}</span></span>
      <button
        type="button"
        class="hdr__setup" data-tour="setup"
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
      <UiButton variant="hazard" size="sm" class="hdr__halt" :disabled="haltBlocker !== null" :title="haltBlocker ?? 'Confirm a halt for the bound TESTNET agent'" @click="confirmHalt">
        HALT AGENT
      </UiButton>
    </div>
    <dialog ref="haltDialog" class="halt-confirmation" aria-labelledby="halt-confirm-title" @close="haltIdentity = null">
      <h2 id="halt-confirm-title">Halt bound TESTNET agent?</h2>
      <dl v-if="haltIdentity"><dt>Agent</dt><dd>{{ haltIdentity.agent }}</dd><dt>Account</dt><dd>{{ haltIdentity.account }}</dd></dl>
      <p>Pauses this agent identity, including later account assignments. Requests cancellation only for the supervised account shown. A changed registry route prevents cancellation confirmation.</p>
      <p>Positions may remain open. Cancellation acknowledgments do not prove the venue is flat.</p>
      <p>Supervision stays running. This does not resume orders or halt other agents.</p>
      <div class="halt-confirmation__actions">
        <UiButton autofocus @click="haltDialog?.close()">Keep unchanged</UiButton>
        <UiButton variant="hazard" :disabled="haltBlocker !== null" @click="submitHalt">Halt this agent</UiButton>
      </div>
    </dialog>
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
.halt-confirmation { box-sizing: border-box; width: min(560px, calc(100vw - 32px)); margin: auto; padding: var(--s-5); border: 1px solid var(--hazard); background: var(--plate); color: var(--body); font: 400 var(--fs-copy) / 1.5 var(--font-sans); letter-spacing: 0; overflow-wrap: anywhere; }
.halt-confirmation::backdrop { background: rgb(0 0 0 / 70%); }
.halt-confirmation h2 { margin: 0 0 var(--s-3); color: var(--signal); font: 700 var(--fs-copy) / 1.5 var(--font-mono); }
.halt-confirmation dd { margin: 0 0 var(--s-2); color: var(--signal); }
.halt-confirmation p { margin-block: var(--s-3); }
.halt-confirmation__actions { display: flex; flex-wrap: wrap; gap: var(--s-3); }
@media (max-width: 1450px) {
  .hdr { display: grid; grid-template-columns: auto 1fr; height: auto; gap: 0 var(--s-4); }
  .hdr__brand, .hdr__nav { min-height: 40px; }
  .hdr__spacer { display: none; }
  .hdr__status { grid-column: 1 / -1; justify-content: flex-end; min-height: 38px; gap: var(--s-4); border-top: 1px solid var(--rule); }
}
</style>
