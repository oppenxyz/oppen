<script setup lang="ts">
import EmptyState from "../components/housing/EmptyState.vue";
import PanelHousing from "../components/housing/PanelHousing.vue";
import ReadoutRows, { type ReadoutRow } from "../components/housing/ReadoutRows.vue";
import UiButton from "../components/ui/UiButton.vue";
import { setView } from "../stores/shell";

const RUNTIME: readonly ReadoutRow[] = [
  { k: "PROCESS", v: "local" },
  { k: "KEYS", v: "never leave device" },
  { k: "MODEL CALLS", v: "—" },
];
</script>

<template>
  <div class="agents">
    <PanelHousing label="Agents · 0 · local runtime">
      <template #meta>
        <UiButton size="sm" @click="setView('builder')">+ New</UiButton>
      </template>
      <EmptyState line="No agents paired." />
      <template #footer>
        New agents start in approval mode with tight caps.<br />
        Autonomy is granted per policy — revoked in one click.
      </template>
    </PanelHousing>

    <div class="agents__main">
      <PanelHousing inset :brackets="['tl']">
        <EmptyState line="No agent selected." />
      </PanelHousing>

      <div class="agents__lower">
        <PanelHousing label="Decision log" meta="Every decision, including holds">
          <EmptyState matrix size="md" line="Pair an agent to see decisions." />
        </PanelHousing>

        <div class="agents__side">
          <PanelHousing inset label="Policy">
            <EmptyState line="No policy loaded." />
          </PanelHousing>
          <PanelHousing inset label="Approvals" meta="0 pending" :brackets="['br']">
            <EmptyState line="No proposals queued." />
            <p class="agents__note">Re-priced at approval · expires unsent</p>
          </PanelHousing>
          <PanelHousing inset label="Runtime">
            <ReadoutRows :rows="RUNTIME" />
          </PanelHousing>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.agents {
  display: grid;
  flex: 1;
  grid-template-columns: 380px minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
  padding: var(--panel-gap);
}

.agents__main {
  display: grid;
  grid-template-rows: auto 1fr;
  grid-template-columns: minmax(0, 1fr);
  gap: var(--panel-gap);
  min-width: 0;
  min-height: 0;
}

.agents__lower {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 320px;
  gap: var(--panel-gap);
  min-height: 0;
}

.agents__side {
  display: grid;
  grid-template-rows: auto auto 1fr;
  gap: var(--panel-gap);
  min-height: 0;
}

.agents__note {
  margin-top: var(--s-2);
  font-size: var(--fs-label);
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--bracket);
}
</style>
