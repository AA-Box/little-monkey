import { defineConfig } from "vitest/config";

// Kept separate from vite.config.ts (which is tailored to `tauri dev`) so
// test runs never interact with the Tauri dev-server expectations there.
export default defineConfig({
  test: {
    environment: "node",
    // `sdk/**` covers the checked-in-template client SDKs (TypeScript
    // request-building tests only — no real network calls, no Python/shell).
    include: ["src/**/*.test.ts", "sdk/**/*.test.ts"],
  },
});
