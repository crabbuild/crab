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
    dedupe: ["vega", "vega-lite", "vega-interpreter"],
    alias: {
      "@": path.resolve(__dirname, "src"),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "chrome120",
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [{ name: "monaco", test: /[\\/]node_modules[\\/]monaco-editor[\\/]/ }],
        },
      },
    },
  },
  server: {
    fs: {allow: [__dirname, path.resolve(__dirname, "../workflow-results"), path.resolve(__dirname, "../../crates/crab-workflow/assets")]},
    port: 5173,
    strictPort: true,
  },
});
