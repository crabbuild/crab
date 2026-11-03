import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// Electron loads the built renderer via `file://`, so we need relative
// asset URLs. The `base: "./"` below wires that up; anything else (an
// absolute `/assets/...`) breaks once the app is packaged.
export default defineConfig({
  base: "./",
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src"),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "chrome120",
    rollupOptions: {
      output: {
        manualChunks: {
          monaco: ["monaco-editor"],
        },
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
