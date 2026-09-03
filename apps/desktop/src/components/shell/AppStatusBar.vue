<script setup lang="ts">
import { computed } from "vue";
import { feedLabel, shell } from "../../stores/shell";

const feeds = computed(() => ({
  market: feedLabel(shell.feeds.wsMarket),
  user: feedLabel(shell.feeds.wsUser),
  rest: feedLabel(shell.feeds.rest),
}));
</script>

<template>
  <footer class="sb">
    <span class="sb__k">Last decision</span>
    <template v-if="shell.lastDecision">
      <span>{{ shell.lastDecision.time }}</span>
      <span class="sb__k">{{ shell.lastDecision.agent }}</span>
      <!-- Agent-authored text. Plain text only. -->
      <span class="sb__text">{{ shell.lastDecision.text }}</span>
    </template>
    <span v-else>—</span>

    <span class="sb__spacer" />

    <span>
      Feeds ·
      <span class="sb__k">WS·MKT {{ feeds.market }} · WS·USER {{ feeds.user }} · REST {{ feeds.rest }}</span>
    </span>
    <span>{{ shell.decisionsToday }} decisions today · {{ shell.refusedToday }} refused</span>
  </footer>
</template>

<style scoped>
.sb {
  display: flex;
  flex: none;
  align-items: center;
  gap: var(--s-6);
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
</style>
