import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const host = process.env.TAURI_DEV_HOST;
const fullProductE2eBuild = process.env.VITE_COMPUTER_USE_FULL_PRODUCT_E2E === "1";

const productionBuild = {
  manifest: true,
  license: true,
  chunkSizeWarningLimit: 900,
  rollupOptions: {
    output: {
      manualChunks(id: string) {
        if (!id.includes("node_modules")) return undefined;
        if (/[\\/]node_modules[\\/](react|react-dom|scheduler|zustand)[\\/]/.test(id)) return "react-vendor";
        if (/[\\/]node_modules[\\/]@tauri-apps[\\/]/.test(id)) return "tauri-vendor";
        if (/[\\/]node_modules[\\/](react-markdown|remark-|rehype-|unified|micromark|mdast-|hast-|unist-)/.test(id)) return "markdown-vendor";
        // Syntax highlighting is only used by the lazy ArtifactPane. Let
        // Rollup keep it with that async surface; forcing it into a manual
        // vendor chunk makes the entry import it to resolve a chunk cycle.
        // Mermaid intentionally keeps its own renderer chunks. Combining the
        // whole graph stack into one manual chunk creates a multi-megabyte
        // download even when only one diagram type is used.
        if (/[\\/]node_modules[\\/]sql.js[\\/]/.test(id)) return "sql-vendor";
        return undefined;
      },
    },
  },
};

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react(), tailwindcss()],
  // The full-product acceptance loads the built bundle through Tauri's local
  // asset server; relative URLs also work when that server is not rooted at
  // the repository's web origin.
  base: fullProductE2eBuild ? "./" : "/",

  // Vite 7 resolves build settings per environment. Keep the top-level copy
  // for legacy/Tauri callers and the explicit client copy for the environment
  // builder so neither path silently drops the release budgets.
  build: productionBuild,
  environments: { client: { build: productionBuild } },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  // (PORT env override lets a second instance run for browser preview)
  server: {
    port: Number(process.env.PORT) || 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
