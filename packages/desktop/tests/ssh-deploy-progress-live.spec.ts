import { describe, expect, it } from "vitest";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { chromium, expect as browserExpect, type Browser } from "@playwright/test";
import { createServer } from "vite";
import type { DeployProgress } from "../electron/ssh-deployer";

declare global {
  interface Window {
    sshProgressFixture: {
      ready: boolean;
      progress(value: DeployProgress & { profileId: string }): void;
      failure(value: { profileId: string; reason: string; message: string }): void;
      counts(): number[];
    };
  }
}
describe.skipIf(!process.env.CRAB_AGENT_LIVE_ROOT)("SSH progress ownership in the renderer", () => {
  it("keeps another profile's events from replacing progress or clearing an error", async () => {
    const directory = await mkdtemp(path.join(tmpdir(), "crab-progress-ui-"));
    const vite = await createServer({ cacheDir: directory, optimizeDeps: { entries: ["tests/fixtures/ssh-deploy-progress.html"] }, server: { host: "127.0.0.1", port: 0, strictPort: false }, clearScreen: false });
    let browser: Browser | undefined;
    try {
      await vite.listen(); const address = vite.httpServer?.address(); if (!address || typeof address === "string") throw new Error("Fixture renderer unavailable");
      browser = await chromium.launch({ channel: "chrome" }); const page = await browser.newPage();
      page.on("pageerror", error => { console.error("Progress renderer:", error.message); });
      await page.goto(`http://127.0.0.1:${address.port}/tests/fixtures/ssh-deploy-progress.html`);
      await page.waitForFunction(() => window.sshProgressFixture.ready, undefined, { timeout: 10_000 });
      await page.evaluate(() => window.sshProgressFixture.failure({ profileId: "first", reason: "UPLOAD_FAILED", message: "Owned upload failed" }));
      await browserExpect(page.getByRole("alert")).toContainText("Owned upload failed");
      await page.evaluate(() => window.sshProgressFixture.progress({ profileId: "second", stage: "preparing", percent: 0 }));
      await browserExpect(page.getByRole("alert")).toContainText("Owned upload failed"); await browserExpect(page.getByRole("status")).toHaveCount(0);
      await page.evaluate(() => window.sshProgressFixture.progress({ profileId: "first", stage: "preparing", percent: 0 }));
      await browserExpect(page.getByRole("status")).toContainText("Preparing runtime"); await browserExpect(page.getByRole("alert")).toHaveCount(0);
      await page.getByRole("button", { name: "Second profile" }).click(); await browserExpect(page.getByRole("status")).toHaveCount(0);
      await page.evaluate(() => window.sshProgressFixture.progress({ profileId: "second", stage: "uploading", percent: 42 }));
      await browserExpect(page.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "42");
      expect(await page.evaluate(() => window.sshProgressFixture.counts())).toEqual([1, 1]);
      await page.screenshot({ path: path.join(process.env.CRAB_AGENT_LIVE_ROOT!, "ssh-profile-progress.png") });
    } finally { await browser?.close(); await vite.close(); await rm(directory, { recursive: true, force: true }); }
  }, 30_000);
});
