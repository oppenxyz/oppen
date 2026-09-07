<script setup lang="ts">
import { shell, setView, refreshAccount } from "../stores/shell";
import UiButton from "./ui/UiButton.vue";
</script>

<template>
  <div v-if="!shell.account || shell.accountError || shell.account.feed !== 'live'" class="account-notice" role="status">
    <div>
      <strong>{{ shell.account ? 'Last account reading' : 'Account data unavailable' }}</strong>
      <span v-if="shell.account"> · {{ new Date(shell.account.as_of_ms).toLocaleString() }}</span>
      <p>{{ shell.accountError ?? (shell.account ? 'Account feed has not confirmed a live connection.' : 'Waiting for a successful account read. Positions and balances are unknown.') }}</p>
    </div>
    <UiButton v-if="!shell.account" @click="setView('onboarding')">Setup</UiButton>
    <UiButton @click="refreshAccount">Retry read</UiButton>
  </div>
</template>

<style scoped>
.account-notice { display: flex; align-items: center; gap: var(--s-3); padding: var(--s-3); border-bottom: 1px solid var(--rule-strong); font-size: var(--fs-body); color: var(--body); }
.account-notice > div { flex: 1; min-width: 0; }
strong { color: var(--signal); font-weight: 400; }
p { margin-top: var(--s-1); overflow-wrap: anywhere; }
</style>
