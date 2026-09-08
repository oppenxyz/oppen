import { ref } from "vue";
import { setView } from "./shell";

export const SETTINGS_SECTIONS = ["Permissions & limits", "Pilot consent", "Keys & venues", "Models & API keys", "Local data", "MCP server", "Display"] as const;
export const settingsSection = ref<(typeof SETTINGS_SECTIONS)[number]>("Permissions & limits");

export function openMcpSettings(): void {
  settingsSection.value = "MCP server";
  setView("settings");
}

export function openPolicySettings(): void {
  settingsSection.value = "Permissions & limits";
  setView("settings");
}
export function openPilotConsentSettings(): void {
  settingsSection.value = "Pilot consent";
  setView("settings");
}
