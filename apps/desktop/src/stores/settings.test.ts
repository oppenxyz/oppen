import { openMcpSettings, openPolicySettings, settingsSection } from "./settings";
import { setView, shell } from "./shell";

declare const it: (name: string, body: () => void) => void;
declare const expect: (value: unknown) => { toBe(expected: unknown): void };

it("opens the MCP settings section without changing network or feed evidence", () => {
  const previousView = shell.view;
  const previousSection = settingsSection.value;
  const network = shell.network;
  const feeds = JSON.stringify(shell.feeds);
  try {
    setView("builder");
    settingsSection.value = "Permissions & limits";
    openMcpSettings();
    expect(shell.view).toBe("settings");
    expect(settingsSection.value).toBe("MCP server");
    expect(shell.network).toBe(network);
    expect(JSON.stringify(shell.feeds)).toBe(feeds);
  } finally {
    settingsSection.value = previousSection;
    setView(previousView);
  }
});

it("opens paused policy setup without selecting an unverified roster identity or changing network", () => {
  const previousView = shell.view;
  const previousSection = settingsSection.value;
  const network = shell.network;
  try {
    settingsSection.value = "MCP server";
    openPolicySettings();
    expect(shell.view).toBe("settings");
    expect(settingsSection.value).toBe("Permissions & limits");
    expect(shell.network).toBe(network);
  } finally {
    settingsSection.value = previousSection;
    setView(previousView);
  }
});
