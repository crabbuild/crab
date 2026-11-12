import { defineConfig, mergeConfig } from "vitest/config";
import renderer from "./vite.config";

export default mergeConfig(renderer, defineConfig({
  test: {
    include: ["tests/**/*.spec.ts"],
    exclude: ["tests/e2e/**", "tests/a11y.spec.ts", "tests/a11y-orchestrator.spec.ts"],
  },
}));
