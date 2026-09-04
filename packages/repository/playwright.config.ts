import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/browser",
  testMatch: "**/*.e2e.ts",
  forbidOnly: true,
  reporter: "list",
  outputDir: "node_modules/.cache/browser-tests",
  use: {
    baseURL: "http://127.0.0.1:5175",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "npm run dev -- --port 5175 --strictPort",
    url: "http://127.0.0.1:5175",
  },
});
