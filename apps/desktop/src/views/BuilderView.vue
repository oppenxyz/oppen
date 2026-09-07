<script setup lang="ts">
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import StatBlock from "../components/housing/StatBlock.vue";
import UiButton from "../components/ui/UiButton.vue";
import { shell, setView } from "../stores/shell";

interface Source {
  name: string;
  desc: string;
  /** v1 ships one source. The rest are labelled with the release that brings them. */
  planned?: string;
}

const SOURCES: readonly Source[] = [
  { name: "External via MCP", desc: "Claude Code or any MCP client. Agent lives elsewhere, keys stay here." },
  { name: "oppen template", desc: "Prompt templates describe responsibilities; they do not prescribe trading thresholds.", planned: "planned" },
  { name: "Bring-your-own model", desc: "Hosted loop via API key. Planned.", planned: "v1.5" },
  { name: "Script strategy", desc: "Sandboxed Python/TS runtime. Planned.", planned: "v2" },
];
</script>

<template>
  <div class="builder">
    <PanelHousing label="01 · Source" data-tour="builder">
      <ul class="sources">
        <li
          v-for="(source, index) in SOURCES"
          :key="source.name"
          class="source"
          :class="{ 'source--selected': index === 0, 'source--planned': source.planned }"
        >
          <div class="source__head">
            <span class="source__name">{{ source.name }}</span>
            <span v-if="source.planned" class="source__tag">{{ source.planned }}</span>
          </div>
          <div class="source__desc">{{ source.desc }}</div>
        </li>
      </ul>
      <template #footer>02 · Policy → 03 · Testnet → 04 · Arm</template>
    </PanelHousing>

    <div class="builder__center">
      <PanelHousing inset>
        <div class="namebar">
          <span class="label">Name</span>
          <span class="namebar__name">External MCP setup</span>
          <span class="namebar__spacer" />
          <span class="namebar__client">Client · <span class="namebar__v">Your MCP client</span></span>
        </div>
      </PanelHousing>

      <div class="builder__pair">
        <PanelHousing label="02 · Connect the gateway" :brackets="['tl']">
          <div class="guide">
            <p>This development build runs the gateway separately. The console cannot create pairings or edit its active risk policy yet.</p>
            <ol>
              <li>Configure the testnet account in <code>OPPEN_TESTNET_USER</code>. Keep account-owner keys in your wallet.</li>
              <li>From the repository, run:<pre>cargo run -p oppen-mcp --example serve</pre></li>
              <li>The gateway prints a one-time pairing command for your MCP client. Use that command locally.</li>
              <li>Ask the connected agent to read state and run preflight. Inspect any refusal before considering execution.</li>
            </ol>
            <p>This path binds <code>agent-alpha</code> to the configured testnet account. The gateway's default allowlist is empty; registration is not trading permission.</p>
          </div>
          <template #footer>Policy is checked before every order. The model cannot change it.</template>
        </PanelHousing>
        <PanelHousing label="Instructions — what the model sees">
          <div class="guide">
            <p>Start with a read-only briefing:</p>
            <blockquote>Read the account state, open orders and feed freshness. Identify the account/container and network. Explain missing data and typed refusals. Do not place or cancel orders.</blockquote>
            <p>Agent explanations are claims, not verified facts. The execution engine checks the active policy before signing.</p>
          </div>
          <template #footer>
            <span class="split">
              <span>Tools · state, features, preflight, place, cancel, journal</span>
              <span>Operator briefing</span>
            </span>
          </template>
        </PanelHousing>
      </div>
    </div>

    <div class="builder__right">
      <PanelHousing label="03 · Testnet run" :meta="shell.network">
        <EmptyState matrix mode="sweep" line="Run history is not connected to this console." />
        <div class="run">
          <StatBlock label="Trades" size="md" />
          <StatBlock label="PnL" size="md" />
          <StatBlock label="Refused" size="md" />
        </div>
      </PanelHousing>

      <PanelHousing inset label="04 · Verify readiness">
        <p class="copy copy--sm">
          Pairing, policy editing and arming are not available in this console yet. Follow the testnet gateway setup, then verify the account and active limits in your MCP client.
        </p>
        <div class="actions">
          <UiButton block @click="setView('onboarding')">Check setup</UiButton>
        </div>
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
.guide { overflow: auto; padding: var(--s-4); font-family: var(--font-sans); font-size: var(--fs-copy); line-height: 1.65; color: var(--body); }
.guide p + p, .guide ol, .guide blockquote { margin-top: var(--s-3); }
.guide ol { padding-left: var(--s-5); }
.guide li + li { margin-top: var(--s-2); }
.guide pre { white-space: pre-wrap; overflow-wrap: anywhere; padding: var(--s-2); margin-block: var(--s-2); border: 1px solid var(--rule); color: var(--signal); font: 11px/1.5 var(--font-mono); }
.guide blockquote { padding-left: var(--s-3); border-left: 1px solid var(--bracket); color: var(--signal-dim); }

.builder {
  display: grid;
  flex: 1;
  grid-template-columns: 260px minmax(0, 1fr) 340px;
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.builder__center {
  display: grid;
  grid-template-rows: auto 1fr;
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
}

.builder__pair {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  grid-template-rows: minmax(0, 1.2fr) minmax(0, 1fr);
  gap: var(--panel-gap);
  min-height: 0;
}

.builder__right {
  display: grid;
  grid-template-rows: 1fr auto;
  gap: var(--panel-gap);
  min-height: 0;
}

.source {
  padding: var(--s-3);
  border-bottom: 1px solid var(--rule);
  border-left: 2px solid transparent;
}

.source--selected {
  border-left-color: var(--signal);
}

.source__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--s-2);
}

.source__name {
  font-weight: 700;
  color: var(--body);
}

.source--selected .source__name {
  color: var(--signal);
}

.source--planned .source__name {
  color: var(--bracket);
}

.source__tag {
  padding: 2px 6px;
  border: 1px solid var(--rule);
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  color: var(--bracket);
}

.source__desc {
  margin-top: var(--s-1);
  font-size: var(--fs-body-sm);
  color: var(--bracket);
}

.source--planned .source__desc {
  color: var(--bracket);
}

.namebar {
  display: flex;
  align-items: center;
  gap: var(--s-4);
}

.namebar__name {
  font-size: var(--fs-display-sm);
  font-weight: 700;
  letter-spacing: var(--ls-display);
  color: var(--bracket);
}

.namebar__spacer {
  flex: 1;
}

.namebar__client {
  font-size: var(--fs-body-sm);
  letter-spacing: 0.14em;
  text-transform: uppercase;
  color: var(--bracket);
}

.namebar__v {
  color: var(--body);
}

.split {
  display: flex;
  justify-content: space-between;
  gap: var(--s-3);
}

.run {
  display: grid;
  flex: none;
  grid-template-columns: repeat(3, 1fr);
  gap: var(--s-3);
  padding: var(--s-3);
  border-top: 1px solid var(--rule);
}

.actions {
  display: flex;
  gap: var(--s-2);
  margin-top: var(--s-3);
}
</style>
