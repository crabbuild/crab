import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { _electron as electron, expect as browserExpect, type ElectronApplication } from "@playwright/test";
import { createServer, type ViteDevServer } from "vite";

type Change = "workspace" | "unmount" | "filter" | "quit" | "refresh" | "discovery";
declare global {
  interface Window {
    sidebarScrollFixture: { action(mode: Change): { position: number; saved: number } };
  }
}

describe.skipIf(!process.env.CRAB_AGENT_LIVE_ROOT)("Repository sidebar scroll persistence", () => {
  let directory: string;
  let vite: ViteDevServer;
  let entry: string;
  let url: string;
  beforeAll(async () => {
    directory = await mkdtemp(path.join(tmpdir(), "crab-sidebar-scroll-"));
    vite = await createServer({ cacheDir: directory,
      optimizeDeps: { entries: ["tests/fixtures/sidebar-scroll.html"] },
      server: { host: "127.0.0.1", port: 0, strictPort: false }, clearScreen: false });
    await vite.listen();
    const address = vite.httpServer?.address();
    if (!address || typeof address === "string") throw new Error("Sidebar fixture renderer unavailable");
    url = `http://127.0.0.1:${address.port}/tests/fixtures/sidebar-scroll.html`;
    entry = path.join(directory, "main.cjs");
    await writeFile(entry, `
      const { app, BrowserWindow } = require("electron");
      const [profile, url] = process.argv.slice(-2);
      app.setPath("userData", profile);
      app.whenReady().then(() => new BrowserWindow({ show: false,
        webPreferences: { sandbox: true, contextIsolation: true }
      }).loadURL(url));
      app.on("window-all-closed", () => app.quit());
    `);
  }, 30_000);
  afterAll(async () => {
    await vite?.close();
    if (directory) await rm(directory, { recursive: true, force: true });
  });
  it.each<Change>(["workspace", "unmount", "filter", "quit", "refresh", "discovery"])("retains scroll across %s", async mode => {
    const env = Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined));
    delete env.ELECTRON_RUN_AS_NODE;
    let application: ElectronApplication | undefined;
    try {
      // The script fixture needs Playwright's readiness barrier before window creation.
      application = await electron.launch({ args: [entry, path.join(directory, mode), url], env, timeout: 15_000 });
      const page = await application.firstWindow();
      page.on("pageerror", error => console.error("Sidebar fixture:", error.message));
      await page.getByRole("button", { name: "Pin repo-49", exact: true }).first().waitFor();
      const result = await page.evaluate(mode => window.sidebarScrollFixture.action(mode), mode);
      expect(result).toEqual({ position: 600, saved: 600 });
      await page.goto(url);
      await page.getByRole("button", { name: "Pin repo-49", exact: true }).first().waitFor();
      await browserExpect.poll(() => page.getByLabel("Repository list", { exact: true }).evaluate(list => list.scrollTop)).toBe(600);
    } finally {
      await application?.close();
    }
  }, 60_000);
});
