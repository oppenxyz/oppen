<script setup lang="ts">
import { computed, ref } from "vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows from "../components/housing/ReadoutRows.vue";
import UiButton from "../components/ui/UiButton.vue";
import { setNetwork, shell, refreshKeychain, type Network } from "../stores/shell";
import { motionPaused, motionReduced, setMotionPaused } from "../lib/clock";

const SECTIONS = ["Permissions & limits", "Keys & venues", "Models & API keys", "Local data", "MCP server", "Display"] as const;
const section = ref<(typeof SECTIONS)[number]>("Permissions & limits");
const pendingNetwork = ref<Network | null>(null);
const networkError = ref("");
function switchNetwork(): void {
  if (!pendingNetwork.value) return;
  try { setNetwork(pendingNetwork.value); }
  catch { networkError.value = "Network preference could not be saved. Current network is unchanged."; }
}
const local = computed(() => [
  { k: "Console", v: "Open on this device" },
  { k: "Keychain", v: shell.keychain === null ? "Not checked" : shell.keychain.reachable ? "Reachable" : "Unavailable" },
  { k: "Market feed", v: shell.feeds.wsMarket },
  { k: "Account read", v: shell.accountError ? "Failed" : shell.account ? "Received" : "Not received" },
  { k: "Network", v: shell.network.toUpperCase() },
]);
</script>

<template>
  <div class="settings">
    <PanelHousing>
      <nav aria-label="Settings sections" class="snav">
        <button v-for="item in SECTIONS" :key="item" type="button" :aria-current="section === item ? 'page' : undefined" @click="section = item">{{ item }}</button>
      </nav>
    </PanelHousing>
    <div class="settings__content">
      <PanelHousing inset :label="section" :brackets="['tl']" data-tour="settings">
        <template v-if="section === 'Permissions & limits'">
          <p class="copy">Risk limits and approval settings are enforced by Rust. This console does not yet read or edit the gateway's active policies.</p>
          <ReadoutRows size="md" :rows="[
            { k: 'Total notional cap · USD', v: 'Not read' },
            { k: 'Daily loss halt · USD', v: 'Not read' },
            { k: 'Margin utilization limit · %', v: 'Not read' },
            { k: 'Running agent limit', v: 'Not read' },
            { k: 'Human approval setting', v: 'Not read' },
          ]" />
        </template>
        <template v-else-if="section === 'Keys & venues'">
          <p class="copy">Agent signing keys stay in the OS keychain. The console reads reachability only; private keys never enter the interface.</p>
          <ReadoutRows :rows="local" size="md" />
          <UiButton @click="refreshKeychain">Recheck keychain</UiButton>
          <p v-if="shell.keychain?.detail" class="copy">{{ shell.keychain.detail }}</p>
          <fieldset class="network"><legend>Network · saved on this device</legend>
            <UiButton v-for="network in (['testnet', 'mainnet'] as const)" :key="network" :disabled="shell.network === network" @click="pendingNetwork = network">{{ network }}</UiButton>
          </fieldset>
          <div v-if="pendingNetwork" class="confirmation" role="group" aria-label="Confirm network change">
            <p>Switch to {{ pendingNetwork.toUpperCase() }} and reload the console? {{ pendingNetwork === 'mainnet' ? 'Mainnet uses real funds.' : 'Testnet uses test funds.' }} The network choice persists after restart.</p>
            <UiButton @click="pendingNetwork = null">Keep {{ shell.network }}</UiButton>
            <UiButton variant="primary" @click="switchNetwork">Switch to {{ pendingNetwork }}</UiButton>
            <p v-if="networkError" role="alert">{{ networkError }}</p>
          </div>
        </template>
        <template v-else-if="section === 'Models & API keys'">
          <EmptyState line="External MCP agents manage their own models and provider credentials. Hosted model loops are planned for v1.5." />
        </template>
        <template v-else-if="section === 'Local data'">
          <p class="copy">The gateway owns its append-only event ledger. This console has no ledger path configured and does not claim an empty history.</p>
          <p class="copy">Display and network preferences are stored locally. No telemetry is configured.</p>
        </template>
        <template v-else-if="section === 'MCP server'">
          <p class="copy">The gateway uses streamable HTTP on localhost. A token binds an external agent to one account/container. Its live pairing registry is not connected to this console.</p>
          <ReadoutRows size="md" :rows="[{ k: 'Gateway status', v: 'Not read' }, { k: 'Pairings', v: 'Unknown' }, { k: 'Default endpoint', v: '127.0.0.1:7433/mcp' }]" />
          <p class="copy">Use Builder for the current testnet development setup steps.</p>
        </template>
        <template v-else>
          <p class="copy">Dark theme · Space Mono + Archivo. Bright values carry facts; subdued rules define the housings.</p>
          <label class="motion"><input type="checkbox" :checked="motionPaused" @change="setMotionPaused(($event.target as HTMLInputElement).checked)" /> Pause decorative motion</label>
          <p class="copy">{{ motionReduced ? 'Your system requests reduced motion. Decoration stays still.' : 'Terrain and decorative sampling pause when the window is hidden.' }} Market data, orders and freshness tracking continue.</p>
        </template>
      </PanelHousing>
      <PanelHousing v-if="section === 'Permissions & limits'" inset label="Kill switch · status not read" data-tour="kill">
        <p class="copy">A halt pauses new orders and cancels resting orders; it does not close positions. The console has no connection to the gateway's halt control yet.</p>
        <p class="copy">Dead-man protection must be verified per container. This interface has no arming or coverage reading.</p>
        <UiButton variant="hazard" disabled>Halt control unavailable</UiButton>
      </PanelHousing>
      <PanelHousing v-if="section === 'Keys & venues'" inset label="Local machine" :brackets="['br']">
        <pre class="machine" aria-hidden="true">+-----------------------------+
: OPPEN · LOCAL CONSOLE        :
: +---------+   +-----------+  :
: :KEYCHAIN :   : READ MODEL:  :
: +----+----+   +-----+-----+  :
+------+-------------+--------+
       :             :
       +-----+-------+
             :
      +------+------+
      : HYPERLIQUID :
      +-------------+</pre>
        <ReadoutRows :rows="local" />
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.settings { display: grid; flex: 1; grid-template-columns: 220px minmax(0, 1fr); gap: var(--panel-gap); min-width: 0; min-height: 0; padding: var(--panel-gap); }
.settings__content { display: grid; grid-template-columns: minmax(0, 1fr); align-content: start; gap: var(--panel-gap); min-height: 0; overflow: auto; }
.snav { display: grid; }
.snav button { padding: var(--s-3); border-bottom: 1px solid var(--rule); border-left: 2px solid transparent; font: inherit; font-size: var(--fs-body); text-align: left; color: var(--body); }
.snav button[aria-current] { border-left-color: var(--signal); color: var(--signal); background: var(--void); }
.copy { margin-bottom: var(--s-4); max-width: 85ch; line-height: 1.7; }
.network { display: flex; gap: var(--s-3); margin-top: var(--s-5); padding: var(--s-3); border: 1px solid var(--rule-strong); }
.network legend { padding-inline: var(--s-2); color: var(--body); }
.confirmation { margin-top: var(--s-3); padding: var(--s-4); border: 1px solid var(--uranium); }
.confirmation p { margin-bottom: var(--s-3); }
.confirmation button + button { margin-left: var(--s-3); }
.motion { display: flex; align-items: center; gap: var(--s-3); margin-block: var(--s-4); color: var(--signal); font-size: var(--fs-copy); }
.machine { margin: 0 auto var(--s-4); font: 12px/1.4 Menlo, Consolas, monospace; color: var(--bracket); }
</style>
