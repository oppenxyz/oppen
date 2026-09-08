<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
import { checkUpdate, downloadUpdate, installUpdate, inTauri, type UpdateInfo } from "../../lib/bridge";

const dialog = ref<HTMLDialogElement | null>(null);
const info = ref<UpdateInfo | null>(null);
const phase = ref<"idle" | "checking" | "downloading" | "ready" | "installing" | "error">("idle");
const error = ref("");
const confirming = ref(false);
const keepRunning = ref<HTMLButtonElement | null>(null);
const restartChoice = ref<HTMLButtonElement | null>(null);
async function confirmRestart(): Promise<void> { confirming.value = true; await nextTick(); keepRunning.value?.focus(); }
async function cancelRestart(): Promise<void> { confirming.value = false; await nextTick(); restartChoice.value?.focus(); }
let timer: ReturnType<typeof setInterval> | undefined;
const busy = computed(() => ["checking", "downloading", "installing"].includes(phase.value));
const label = computed(() => ({ idle: "Updates", checking: "Checking update…", downloading: "Downloading update…",
  ready: "Update ready", installing: "Installing update…", error: "Updates · check failed" })[phase.value]);
function message(value: unknown): string {
  if (value && typeof value === "object" && "kind" in value) {
    const failure = value as { kind: string; detail?: string };
    if (failure.kind === "unavailable" && typeof failure.detail === "string") return failure.detail;
    if (failure.kind === "busy") return "An update operation is already running. Try again shortly.";
    if (failure.kind === "install_failed") return "Installation failed. Keep this session open and retry, or reinstall the latest release.";
    if (failure.kind === "invalid_release") return "The release metadata could not be validated. This update was not installed.";
  }
  return "The update could not be downloaded or its signature verified. Nothing was installed. Try checking again.";
}
async function check(): Promise<void> {
  if (!inTauri() || busy.value || phase.value === "ready") return;
  phase.value = "checking"; error.value = "";
  try {
    info.value = await checkUpdate();
    if (info.value.available_version && !info.value.ready) {
      phase.value = "downloading";
      info.value = await downloadUpdate();
    }
    phase.value = info.value.ready ? "ready" : "idle";
  } catch (value) { phase.value = "error"; error.value = message(value); }
}
function open(): void { confirming.value = false; dialog.value?.showModal(); }
async function install(): Promise<void> {
  if (phase.value !== "ready" || !confirming.value) return;
  phase.value = "installing"; error.value = "";
  try { await installUpdate(); }
  catch (value) { phase.value = "ready"; error.value = message(value); confirming.value = false; }
}
onMounted(() => {
  void check();
  timer = setInterval(() => { void check(); }, 60 * 60 * 1000);
});
onUnmounted(() => clearInterval(timer));
</script>
<template>
  <button class="update-entry" :class="{ ready: phase === 'ready' }" @click="open">{{ label }}</button>
  <dialog ref="dialog" class="updates" aria-labelledby="updates-title" @cancel="confirming = false">
    <h2 id="updates-title">App updates</h2>
    <p v-if="info">Installed version {{ info.current_version }}</p>
    <p v-if="phase === 'ready'">Version {{ info?.available_version }} is downloaded and its signature is verified.</p>
    <p v-else-if="phase === 'idle' && info">You have the latest published build.</p>
    <p v-else-if="busy" role="status">{{ label }}</p>
    <p v-if="error" role="alert">{{ error }}</p>
    <p v-if="!inTauri()">Updates are available in the installed app.</p>
    <p>Updates download automatically from the official Oppen release channel using your GitHub login. Installation waits for your confirmation.</p>
    <div v-if="confirming" class="confirm" role="group" aria-label="Confirm restart">
      <p>Restarting interrupts this console and its market feeds. It does not cancel orders or close positions. Confirm when you are ready to restart.</p>
      <button ref="keepRunning" :disabled="phase === 'installing'" @click="cancelRestart">Keep running</button>
      <button :disabled="phase !== 'ready'" @click="install">Install and restart</button>
    </div>
    <div v-else class="actions">
      <button v-if="phase === 'ready'" ref="restartChoice" @click="confirmRestart">Restart to update</button>
      <button v-else :disabled="busy || !inTauri()" @click="check">Check for updates</button>
      <button @click="dialog?.close()">Close</button>
    </div>
  </dialog>
</template>
<style scoped>
.update-entry { flex-shrink: 0; color: var(--body); white-space: nowrap; }
.update-entry.ready { color: var(--uranium); }
.updates { position: fixed; margin: auto; width: min(560px, 90vw); max-height: 85vh; overflow: auto; padding: var(--s-6); border: 1px solid var(--bracket); background: var(--plate); color: var(--signal); font: var(--fs-body)/1.6 var(--font-mono); text-transform: none; letter-spacing: normal; white-space: normal; }
.updates::backdrop { background: rgb(0 0 0 / 70%); }
.updates h2 { font-size: var(--fs-body); color: var(--signal); }
.updates p { margin-block: var(--s-3); }
.updates button { padding: var(--s-2) var(--s-3); border: 1px solid var(--bracket); }
.updates button:disabled { opacity: .5; }
.actions, .confirm { display: flex; flex-wrap: wrap; gap: var(--s-3); }
.confirm p { flex-basis: 100%; }
</style>
