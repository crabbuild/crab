import { defineConfig } from "vite";
export default defineConfig({
  // Worker entry points are outside Vite's initial dependency scan. Pre-bundle
  // their JavaScript runtimes so first use cannot trigger a page reload.
  optimizeDeps: { include: ["sql.js", "web-tree-sitter"] },
  server: { proxy: { "/api": "http://127.0.0.1:8788" } },
  worker: { format: "es" },
});
