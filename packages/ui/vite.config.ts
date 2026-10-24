import { defineConfig } from "vite";
import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
export default defineConfig({
  plugins: [tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  // Worker entry points are outside Vite's initial dependency scan. Pre-bundle
  // their JavaScript runtimes so first use cannot trigger a page reload.
  optimizeDeps: { include: ["sql.js", "web-tree-sitter"] },
  // Preserve the browser Host and Origin: authenticated development uses Vite
  // as Crab's canonical public origin, including the OIDC callback.
  server: {
    proxy: {
      "/api": { target: "http://127.0.0.1:8788", changeOrigin: false },
      "/auth": { target: "http://127.0.0.1:8788", changeOrigin: false },
    },
  },
  worker: { format: "es" },
});
