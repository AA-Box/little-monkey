import { defineConfig } from "vitest/config";

// Kept separate from vite.config.ts (which is tailored to `tauri dev`) so
// test runs never interact with the Tauri dev-server expectations there.
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
