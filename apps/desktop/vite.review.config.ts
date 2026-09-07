import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Spec #31/#34 UI verification. This config can serve fixtures, never publish them.
export default defineConfig(({ command }) => {
  if (command !== "serve") throw new Error("UI fixtures are development-only.");
  return {
    plugins: [vue()],
    resolve: { alias: [{ find: /^(\.\.\/)+lib\/bridge$/, replacement: new URL("./qa/bridge.ts", import.meta.url).pathname }] },
    server: { host: "127.0.0.1", port: 1431, strictPort: true },
  };
});
