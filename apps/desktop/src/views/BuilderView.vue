<script setup lang="ts">
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import StatBlock from "../components/housing/StatBlock.vue";
import UiButton from "../components/ui/UiButton.vue";
import { shell } from "../stores/shell";

interface Source {
  name: string;
  desc: string;
  /** v1 ships one source. The rest are labelled with the release that brings them. */
  planned?: string;
}

const SOURCES: readonly Source[] = [
  { name: "External via MCP", desc: "Claude Code or any MCP client. Agent lives elsewhere, keys stay here." },
  { name: "oppen template", desc: "Policy + prompt file for your agent. basis-carry · funding-arb · momentum" },
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
          <span class="namebar__name">—</span>
          <span class="namebar__spacer" />
          <span class="namebar__client">Client · <span class="namebar__v">—</span></span>
        </div>
      </PanelHousing>

      <div class="builder__pair">
        <PanelHousing label="02 · Policy — hard limits, enforced by runtime" :brackets="['tl']">
          <EmptyState line="No draft policy." />
          <template #footer>Policy is checked before every order. The model cannot change it.</template>
        </PanelHousing>
        <PanelHousing label="Instructions — what the model sees">
          <EmptyState line="No instructions." />
          <template #footer>
            <span class="split">
              <span>Tools · state, features, preflight, place, cancel, journal</span>
              <span>— tokens</span>
            </span>
          </template>
        </PanelHousing>
      </div>
    </div>

    <div class="builder__right">
      <PanelHousing label="03 · Testnet run" :meta="shell.network">
        <EmptyState matrix mode="sweep" line="No testnet run." />
        <div class="run">
          <StatBlock label="Trades" size="md" />
          <StatBlock label="PnL" size="md" />
          <StatBlock label="Refused" size="md" />
        </div>
      </PanelHousing>

      <PanelHousing inset label="04 · Arm">
        <p class="copy copy--sm">
          Arming pairs the agent on testnet first. Promote to mainnet from the roster once it has earned it — approval
          mode stays on until you turn it off.
        </p>
        <div class="actions">
          <UiButton block disabled title="Drafts arrive with the agent registry">Save draft</UiButton>
          <UiButton block variant="primary" disabled title="Pairing arrives with the MCP gateway">Arm agent</UiButton>
        </div>
      </PanelHousing>
    </div>
  </div>
</template>

<style scoped>
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
  grid-template-columns: 1fr 1fr;
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
  color: var(--rule-strong);
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
