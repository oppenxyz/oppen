import { createApp } from "vue";
import App from "../src/App.vue";
import { refreshAccount, setView } from "../src/stores/shell";
import { failNextReads } from "./bridge";
import "../src/styles/tokens.css";
if (new URLSearchParams(location.search).get("large") === "true") {
  for (const [name, value] of Object.entries({ label: 15, "label-lg": 16.5, "body-sm": 15, body: 18, "copy-sm": 18, copy: 19.5 })) {
    document.documentElement.style.setProperty(`--fs-${name}`, `${value}px`);
  }
}
window.addEventListener("message", event => {
  if (event.origin === location.origin && event.source === parent && event.data === "fail-read") {
    failNextReads(); void refreshAccount();
  }
});
if (new URLSearchParams(location.search).get("state") === "loading") setView("agents");
if (new URLSearchParams(location.search).has("policy")) setView("settings");
if (new URLSearchParams(location.search).has("approvals")) setView("agents");
createApp(App).mount("#app");
