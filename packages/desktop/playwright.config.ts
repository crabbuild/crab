import { defineConfig } from "@playwright/test";

/**
 * Playwright configuration.
 *
 * Three projects:
 *
 * 1. "a11y" — accessibility (axe-core) tests that run against the Vite dev
 *    server in Chromium. These are fast and run on every PR.
 *
 * 2. "e2e-smoke" — end-to-end tests that launch the packaged Electron binary.
 *    These require a prior `npm run package` step and run on mac + linux CI
 *    and Windows. They are excluded from the
 *    default `playwright test` run unless the E2E_SMOKE env var is set, so
 *    local `npm run test:a11y` stays fast.
 *
 * 3. "agent-requests" — question and approval cards against an isolated
 *    renderer fixture, using the installed Chrome browser and the Vite server.
 */
export default defineConfig({
  testDir: "./tests",
  timeout: 30_000,
  retries: 0,

  projects: [
    {
      name: "theme-system",
      testDir: "./tests/browser",
      testMatch: /theme-system\.pw\.ts$/,
      use: { baseURL: "http://localhost:5173", browserName: "chromium", channel: process.env.CI ? undefined : "chrome" },
    },
    {
      name: "agent-requests",
      testDir: "./tests/browser",
      testMatch: /agent-requests\.pw\.ts$/,
      use: { baseURL: "http://localhost:5173", browserName: "chromium", channel: "chrome" },
    },
    {
      name: "a11y",
      testMatch: /a11y(?:-orchestrator)?\.spec\.ts$/,
      use: {
        baseURL: "http://localhost:5173",
        browserName: "chromium",
      },
    },
    {
      name: "e2e-smoke",
      testDir: "./tests/e2e",
      testMatch: /.*\.spec\.ts$/,
      // No browser — the spec launches Electron directly via child_process.
      use: {},
      // Only run when explicitly requested (CI sets this env var).
      grep: process.env.E2E_SMOKE ? undefined : /^$/, // match nothing by default
    },
  ],

  webServer: process.env.E2E_SMOKE ? undefined : {
    command: "npx vite --host 127.0.0.1",
    url: "http://localhost:5173",
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
