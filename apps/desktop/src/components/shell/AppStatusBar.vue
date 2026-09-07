<script setup lang="ts">
import { computed, ref } from "vue";
import { feedLabel, shell } from "../../stores/shell";

const detailsOpen = ref(false);

const feeds = computed(() => ({
  market: feedLabel(shell.feeds.wsMarket),
  user: feedLabel(shell.feeds.wsUser),
  rest: feedLabel(shell.feeds.rest),
}));
</script>

<template>
  <footer class="sb" data-tour="statusbar">
    <span class="sb__k">Last decision</span>
    <template v-if="shell.lastDecision">
      <span>{{ shell.lastDecision.time }}</span>
      <span class="sb__k">{{ shell.lastDecision.agent }}</span>
      <!-- Agent-authored text. Plain text only. -->
      <span class="sb__text">{{ shell.lastDecision.text }}</span>
    </template>
    <span v-else>—</span>

    <span class="sb__spacer" />

    <!--
      Spec item 34. A failed refresh is stated, not hidden: a blank panel and a
      stale one look identical to an operator and only one of them is safe.
    -->
    <button v-if="shell.accountError" class="sb__alert" :aria-expanded="detailsOpen" @click="detailsOpen = !detailsOpen">Account read failed · details</button>
    <div v-if="detailsOpen && shell.accountError" class="sb__details" role="status">
      <p>{{ shell.accountError }}</p><button @click="detailsOpen = false">Close details</button>
    </div>

    <span>
      Feeds ·
      <span class="sb__k" data-tour="feeds">WS·MKT {{ feeds.market }} · WS·USER {{ feeds.user }} · REST {{ feeds.rest }}</span>
    </span>
    <span>Decision log · not connected</span>
  </footer>
</template>

<style scoped>
.sb__details { position: fixed; bottom: calc(var(--statusbar-h) + 8px); right: 8px; width: min(520px, 80vw); padding: var(--s-4); border: 1px solid var(--rule-strong); background: var(--plate); white-space: normal; text-transform: none; font-size: var(--fs-body); line-height: 1.6; z-index: 20; color: var(--signal); }
.sb__details button { margin-top: var(--s-3); text-decoration: underline; }

.sb {
  display: flex;
  flex: none;
  align-items: center;
  gap: var(--s-3);
  height: var(--statusbar-h);
  padding: 0 var(--s-4);
  border-top: 1px solid var(--rule);
  overflow: hidden;
  font-size: var(--fs-label);
  letter-spacing: var(--ls-label-tight);
  text-transform: uppercase;
  white-space: nowrap;
  color: var(--bracket);
}

.sb__k {
  color: var(--body);
}

.sb__text {
  overflow: hidden;
  text-overflow: ellipsis;
  text-transform: none;
  letter-spacing: 0.04em;
  color: var(--signal);
}

.sb__spacer {
  flex: 1;
}

.sb__alert {
  overflow: hidden;
  max-width: 46ch;
  text-overflow: ellipsis;
  color: var(--hazard, #ff4d2e);
}
</style>
