<script setup lang="ts">
import { shell, setView, refreshAccount, currentAccountFailure, accountObservationLabel } from "../stores/shell";
import UiButton from "./ui/UiButton.vue";
</script>

<template>
  <div v-if="!shell.account || shell.accountError || shell.account.feed !== 'live' || shell.account.feed_age_ms === null || currentAccountFailure" class="account-notice" role="status">
    <div>
      <strong>Account observation · {{ accountObservationLabel() }}</strong>
      <details v-if="shell.accountError || currentAccountFailure">
        <summary>Details</summary>
        <p v-if="currentAccountFailure">{{ currentAccountFailure.detail }}</p>
        <p v-if="shell.accountError">{{ shell.accountError }}</p>
      </details>
    </div>
    <UiButton v-if="!shell.account" @click="setView('onboarding')">Setup</UiButton>
    <UiButton @click="refreshAccount">Retry read</UiButton>
  </div>
</template>

<style scoped>
.account-notice { display: flex; align-items: flex-start; flex: none; gap: var(--s-3); padding: var(--s-2); border-bottom: 1px solid var(--rule-strong); font-size: var(--fs-body-sm); color: var(--body); box-sizing: border-box; max-height: 64px; overflow: auto; }
.account-notice > div { flex: 1; min-width: 0; }
strong { color: var(--signal); font-weight: 400; }
p { margin-top: var(--s-1); overflow-wrap: anywhere; }
summary { cursor: pointer; color: var(--bracket); }
</style>
