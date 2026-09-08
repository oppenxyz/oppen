<script setup lang="ts">
import { computed, nextTick, ref } from "vue";
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows from "../components/housing/ReadoutRows.vue";
import UiButton from "../components/ui/UiButton.vue";
import { setNetwork, shell, refreshKeychain, type Network } from "../stores/shell";
import { decimal } from "../lib/display";
import { operator, recordedAgents, storedPolicy, policySourceLabel, refreshOperator } from "../stores/operator";
import { inTauri } from "../lib/bridge";
import { MCP_PHASES, pauseSweepLabel, supervision, supervisionInputError } from "../stores/supervision";
import { SETTINGS_SECTIONS as SECTIONS, settingsSection as section } from "../stores/settings";
import { motionPaused, motionReduced, setMotionPaused } from "../lib/clock";

const supervisionAgent = ref("");
const supervisionAccount = ref("");
const confirmStop = ref(false);
const mcp = supervision.state;
const inputError = computed(() => supervisionInputError(shell.network, supervisionAgent.value, supervisionAccount.value));
const canStart = computed(() => inTauri() && inputError.value === null && mcp.command === null && !mcp.stopRequested
  && mcp.error === null && mcp.status?.phase === 'idle');
const mcpRows = computed(() => [
  { k: "Supervision", v: mcp.status ? MCP_PHASES[mcp.status.phase] : "Not read" },
  { k: "Network", v: mcp.status?.network.toUpperCase() ?? "Unknown" },
  { k: "Agent", v: mcp.status?.agent ?? "Not selected" },
  { k: "Account", v: mcp.status?.account ?? "Not selected" },
  { k: "Listener", v: mcp.status?.listener ?? "Not listening" },
  { k: "Account reconciled", v: mcp.status?.reconciled === true ? "Yes" : mcp.status?.reconciled === false ? "No" : "Unknown" },
  { k: "Account feeds ready", v: mcp.status?.account_feeds_ready === true ? "Yes" : mcp.status?.account_feeds_ready === false ? "No" : "Unknown" },
  { k: "Pause enforcement", v: pauseSweepLabel(mcp.status) },
  { k: "Last completed pause sweep", v: mcp.status?.supervision_last_completed_ms != null ? new Date(mcp.status.supervision_last_completed_ms).toLocaleString() : "Never observed" },
  { k: "Orders inhibited", v: mcp.status ? (mcp.status.orders_inhibited ? "Yes" : "No - not an authorization to trade") : "Unknown" },
]);
async function stopRuntime(): Promise<void> {
  confirmStop.value = false;
  await supervision.stop();
}
const pendingNetwork = ref<Network | null>(null);
const networkError = ref("");
const networkChoices = ref<HTMLElement | null>(null);
const networkConfirmation = ref<HTMLElement | null>(null);
async function chooseNetwork(network: Network): Promise<void> {
  networkError.value = "";
  pendingNetwork.value = network;
  await nextTick();
  networkConfirmation.value?.querySelector<HTMLButtonElement>("button")?.focus();
}
async function cancelNetwork(): Promise<void> {
  pendingNetwork.value = null;
  await nextTick();
  networkChoices.value?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
}
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
          <p class="copy" role="status">{{ policySourceLabel }}. Runtime policy not read.</p>
          <ReadoutRows size="md" :rows="[
            { k: 'Snapshot revision', v: operator.policy?.revision != null ? String(operator.policy.revision) : 'Unavailable' },
            { k: 'Account daily loss halt · USD', v: storedPolicy ? (storedPolicy.account_limits.max_daily_loss_usd === null ? 'Unset' : decimal(storedPolicy.account_limits.max_daily_loss_usd)) : 'Not read' },
            { k: 'Account drawdown halt · USD', v: storedPolicy ? (storedPolicy.account_limits.max_drawdown_usd === null ? 'Unset' : decimal(storedPolicy.account_limits.max_drawdown_usd)) : 'Not read' },
            { k: 'Stored agent policies', v: storedPolicy ? String(Object.keys(storedPolicy.guardrails).length) : 'Not read' },
          ]" />
          <p v-if="operator.error || operator.policyError" class="copy" role="status">{{ operator.error ?? operator.policyError }}</p>
          <p class="copy">{{ operator.policyReadMs ? `Last successful policy read: ${new Date(operator.policyReadMs).toLocaleString()}.` : 'Policy has not been read.' }} {{ operator.policy && (operator.error || operator.policyError) ? 'Showing that saved reading after a failed refresh.' : '' }}</p>
          <UiButton @click="refreshOperator" :disabled="operator.reading">Refresh stored policy</UiButton>
        </template>
        <template v-else-if="section === 'Keys & venues'">
          <p class="copy">Agent signing keys stay in the OS keychain. The console reads reachability only; private keys never enter the interface.</p>
          <ReadoutRows :rows="local" size="md" />
          <UiButton @click="refreshKeychain">Recheck keychain</UiButton>
          <p v-if="shell.keychain?.detail" class="copy">{{ shell.keychain.detail }}</p>
          <fieldset ref="networkChoices" class="network"><legend>Network · saved on this device</legend>
            <UiButton v-for="network in (['testnet', 'mainnet'] as const)" :key="network" :disabled="shell.network === network" @click="chooseNetwork(network)">{{ network }}</UiButton>
          </fieldset>
          <div v-if="pendingNetwork" ref="networkConfirmation" class="confirmation" @keydown.esc.stop.prevent="cancelNetwork" role="group" aria-label="Confirm network change">
            <p>Switch to {{ pendingNetwork.toUpperCase() }} and reload the console? {{ pendingNetwork === 'mainnet' ? 'Mainnet uses real funds.' : 'Testnet uses test funds.' }} The network choice persists after restart.</p>
            <UiButton @click="cancelNetwork">Keep {{ shell.network }}</UiButton>
            <UiButton variant="primary" @click="switchNetwork">Switch to {{ pendingNetwork }}</UiButton>
            <p v-if="networkError" role="alert">{{ networkError }}</p>
          </div>
        </template>
        <template v-else-if="section === 'Models & API keys'">
          <EmptyState line="External MCP agents manage their own models and provider credentials. Hosted model loops are planned for v1.5." />
        </template>
        <template v-else-if="section === 'Local data'">
          <p class="copy">The gateway owns its append-only event ledger. The console reads its existing files without creating or migrating them. Launch both with the same OPPEN_DATA_DIR to connect stored policies and history.</p>
          <ReadoutRows size="md" :rows="[
            { k: 'Ledger head', v: operator.ledger ? `#${operator.ledger.head_seq}` : 'Not read' },
            { k: 'Events in current window', v: operator.ledger ? String(operator.ledger.events.length) : 'Unknown' },
            { k: 'Last ledger read', v: operator.ledgerReadMs ? new Date(operator.ledgerReadMs).toLocaleString() : 'Never' },
          ]" />
          <p v-if="operator.error || operator.ledgerError" class="copy" role="status">{{ operator.error ?? operator.ledgerError }}</p>
          <UiButton @click="refreshOperator" :disabled="operator.reading">Refresh gateway files</UiButton>
          <p class="copy">Display and network preferences are stored locally. No telemetry is configured.</p>
        </template>
        <template v-else-if="section === 'MCP server'">
          <h2 class="supervision-title">TESTNET supervision</h2>
          <ReadoutRows size="md" :rows="mcpRows" />
          <p v-if="mcp.status?.detail" class="copy supervision-warning" role="status">{{ mcp.status.detail }}</p>
          <p v-if="mcp.status?.phase === 'failed'" class="copy supervision-warning" role="status">Startup failure closed runtime admission. Resolve the reported blocker and restart the app; supervision cannot be retried in this runtime.</p>
          <p v-if="mcp.status?.supervision_error" class="copy supervision-warning" role="status">Pause enforcement error: {{ mcp.status.supervision_error }}</p>
          <p v-if="mcp.error" class="copy supervision-warning" role="status">Status unavailable: {{ mcp.error }} {{ mcp.checkedAt ? `Last observed ${new Date(mcp.checkedAt).toLocaleTimeString()}.` : '' }}</p>
          <p class="copy">Start supervision reads the existing authority key in Rust and may enforce existing pauses and cancel resting orders under the existing authorized pilot. Positions may remain open. A listening connection is not trading activation.</p>
          <p class="copy">Uses existing TESTNET pilot authorization, registry, policy and pairing records. The account must exactly match the configured TESTNET account. This action does not create setup, authorize a pilot, acknowledge policy or switch networks.</p>
          <form class="supervision-form" @submit.prevent="supervision.start(shell.network, supervisionAgent, supervisionAccount)">
            <label>Agent ID
              <input v-model="supervisionAgent" list="supervision-agents" autocomplete="off" :disabled="mcp.command !== null || mcp.stopRequested" />
            </label>
            <datalist id="supervision-agents"><option v-for="agent in recordedAgents" :key="agent" :value="agent" /></datalist>
            <label>Authorized TESTNET account
              <input v-model="supervisionAccount" placeholder="0x..." autocomplete="off" spellcheck="false" :disabled="mcp.command !== null || mcp.stopRequested" />
            </label>
            <p class="copy">Recorded agent suggestions are unverified. Use the identity already confirmed in the durable pilot authorization.</p>
            <p v-if="shell.network !== 'testnet' || (supervisionAgent && supervisionAccount && inputError)" class="supervision-warning" role="status">{{ inputError }}</p>
            <div class="supervision-actions">
              <UiButton type="submit" variant="primary" :disabled="!canStart">{{ mcp.command === 'start' ? 'Starting supervision' : 'Start supervision' }}</UiButton>
              <UiButton :disabled="mcp.reading" @click="supervision.refresh">Refresh status</UiButton>
              <UiButton variant="hazard" :disabled="!inTauri() || mcp.command === 'stop' || mcp.stopRequested" @click="confirmStop = true">Stop runtime</UiButton>
            </div>
          </form>
          <p v-if="mcp.commandError" class="copy supervision-warning" role="alert">{{ mcp.commandError }}</p>
          <div v-if="confirmStop" class="confirmation" role="group" aria-label="Confirm terminal runtime shutdown" @keydown.esc.stop.prevent="confirmStop = false">
            <p>Stop the entire desktop runtime, including MCP and feed tasks? Shutdown is terminal: restarting the app is required. This does not close positions or prove cancellation completion.</p>
            <UiButton @click="confirmStop = false">Keep runtime</UiButton>
            <UiButton variant="hazard" @click="stopRuntime">Stop runtime</UiButton>
          </div>
          <p v-if="mcp.stopRequested" class="copy supervision-warning" role="status">Runtime shutdown requested. {{ mcp.runtime?.phase === 'stopped' ? 'Desktop tasks stopped.' : mcp.runtime?.phase === 'stopped_with_error' ? 'Desktop tasks stopped with errors.' : 'Completion is not confirmed here.' }} Restarting the app is required; this panel cannot reopen it.</p>
          <p v-if="mcp.runtime?.detail" class="copy supervision-warning">{{ mcp.runtime.detail }}</p>
        </template>
        <template v-else>
          <p class="copy">Dark theme · Space Mono + Archivo. Bright values carry facts; subdued rules define the housings.</p>
          <label class="motion"><input type="checkbox" :checked="motionPaused" @change="setMotionPaused(($event.target as HTMLInputElement).checked)" /> Pause decorative motion</label>
          <p class="copy">{{ motionReduced ? 'Your system requests reduced motion. Decoration stays still.' : 'Terrain and decorative sampling pause when the window is hidden.' }} Market data, orders and freshness tracking continue.</p>
        </template>
      </PanelHousing>
      <PanelHousing v-if="section === 'Permissions & limits'" inset label="Kill switch · stored state" data-tour="kill">
        <p class="copy">Stored global halt: {{ storedPolicy ? (storedPolicy.kill.global ? 'engaged' : 'not engaged') : 'not read' }}. Agent-specific stored halts: {{ storedPolicy ? Object.keys(storedPolicy.kill.agents).length : 'unknown' }}. A stored state does not prove cancel completion.</p>
        <p class="copy">HALT AGENT targets only the listening runtime's bound TESTNET agent and account. It requests an order pause and resting-order cancellation; positions may remain open. The persistent halt banner reports durability and cancellation separately.</p>
        <p class="copy">Dead-man protection must be verified per container. This interface has no arming or coverage reading.</p>
        <p class="copy">No global halt or resume control is provided here. Stored policy is not proof of the live halt outcome.</p>
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
.settings__content { display: grid; grid-template-columns: minmax(0, 1fr); grid-auto-rows: max-content; align-content: start; gap: var(--panel-gap); min-height: 0; overflow: auto; }
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
.supervision-title { margin-bottom: var(--s-4); font: 700 var(--fs-copy) / 1.5 var(--font-mono); letter-spacing: 0; }
.supervision-form { display: grid; gap: var(--s-3); margin-block: var(--s-4); min-width: 0; }
.supervision-form label { display: grid; gap: var(--s-2); color: var(--body); }
.supervision-form input { box-sizing: border-box; width: 100%; min-width: 0; max-width: 64ch; padding: var(--s-3); border: 1px solid var(--rule-strong); background: var(--void); color: var(--signal); font: inherit; letter-spacing: 0; }
.supervision-actions { display: flex; flex-wrap: wrap; gap: var(--s-3); }
.supervision-warning { color: var(--hazard); overflow-wrap: anywhere; }
</style>
