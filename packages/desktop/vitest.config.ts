import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/**/*.spec.ts"],
    exclude: ["tests/e2e/**","tests/a11y.spec.ts", "tests/a11y-orchestrator.spec.ts"],
  },
});
