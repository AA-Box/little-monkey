import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const host = process.env.TAURI_DEV_HOST;

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
        if (/[\\/]node_modules[\\/](react-syntax-highlighter|highlight.js|refractor|prismjs)[\\/]/.test(id)) return "highlight-vendor";
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
