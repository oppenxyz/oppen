<script setup lang="ts">
import { computed, onMounted, onUnmounted, type Component } from "vue";
import AppHeader from "./components/shell/AppHeader.vue";
import AppStatusBar from "./components/shell/AppStatusBar.vue";
import PilotStatusBanner from "./components/shell/PilotStatusBanner.vue";
import TourSpotlight from "./components/tour/TourSpotlight.vue";
import {
  refreshKeychain,
  shell,
  startAccountPolling,
  stopAccountPolling,
  type View,
} from "./stores/shell";
import { startMarketFeed, stopMarketFeed } from "./stores/market";
import { startPilotPolling, stopPilotPolling } from "./stores/pilot";
import AgentsView from "./views/AgentsView.vue";
import BuilderView from "./views/BuilderView.vue";
import OnboardingView from "./views/OnboardingView.vue";
import PortfolioView from "./views/PortfolioView.vue";
import SettingsView from "./views/SettingsView.vue";
import TradeView from "./views/TradeView.vue";

const VIEWS: Record<View, Component> = {
  trade: TradeView,
  agents: AgentsView,
  builder: BuilderView,
  portfolio: PortfolioView,
  settings: SettingsView,
  onboarding: OnboardingView,
};

const current = computed(() => VIEWS[shell.view]);

onMounted(() => {
  startAccountPolling();
  startPilotPolling();
  // The socket, the rail poll and the staleness clock (items 31, 34). Started
  // here rather than in TradeView: the status bar reads the feed from every
  // view, so it must not stop when the operator opens Agents.
  void startMarketFeed();
  // Asked once, not polled: see `refreshKeychain`.
  void refreshKeychain();
});
onUnmounted(() => {
  stopAccountPolling();
  stopPilotPolling();
  stopMarketFeed();
});
</script>

<template>
  <div class="app">
    <AppHeader />
    <PilotStatusBanner />
    <main class="app__main">
      <component :is="current" />
    </main>
    <AppStatusBar />
    <TourSpotlight />
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-width: 1280px;
  background: var(--void);
  color: var(--signal);
}

.app__main {
  display: flex;
  flex: 1;
  min-height: 0;
}
</style>
