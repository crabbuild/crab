import { defineConfig } from "vitest/config";
import renderer from "./vite.config";

export default defineConfig({
  resolve: renderer.resolve,
  test: {
    include: ["tests/**/*.spec.ts"],
    exclude: ["tests/e2e/**", "tests/a11y.spec.ts", "tests/a11y-orchestrator.spec.ts"],
  },
});
