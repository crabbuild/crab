import { describe, expect, it } from "vitest";
import { _electron as electron, expect as browserExpect, type ElectronApplication } from "@playwright/test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { createServer } from "vite";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;

describe.skipIf(!liveRoot)("Workflow autosave lifecycle", () => {
  it.each(["rename", "delete"])("does not recreate a dirty workflow after %s", async operation => {
    const directory = await mkdtemp(path.join(liveRoot!, "workflow-autosave-"));
    const vite = await createServer({ cacheDir: path.join(directory, "vite-cache"),
      optimizeDeps: { entries: ["tests/fixtures/workflow-autosave.html"] },
      server: { host: "127.0.0.1", port: 0 }, clearScreen: false });
    let application: ElectronApplication | undefined;
    try {
      await vite.listen();
      const address = vite.httpServer?.address();
      if (!address || typeof address === "string") throw new Error("Workflow fixture renderer unavailable");
      const entry = path.join(directory, "main.cjs");
      await writeFile(entry, `
        const { app, BrowserWindow } = require("electron");
        const [profile, url] = process.argv.slice(-2);
        app.setPath("userData", profile);
        app.whenReady().then(() => new BrowserWindow({ show: false,
          webPreferences: { sandbox: true, contextIsolation: true }
        }).loadURL(url));
        app.on("window-all-closed", () => app.quit());
      `);
      const env = Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined));
      delete env.ELECTRON_RUN_AS_NODE;
      application = await electron.launch({ args: [entry, path.join(directory, "profile"),
        `http://127.0.0.1:${address.port}/tests/fixtures/workflow-autosave.html`], env });
      const page = await application.firstWindow();
      await page.getByRole("button", { name: operation, exact: true }).click();
      await browserExpect(page.locator("output")).not.toBeEmpty();
      expect(JSON.parse(await page.locator("output").innerText())).toEqual({ success: true,
        files: operation === "rename" ? [expect.objectContaining({ path: "report.workflow.yaml",
          stages: [expect.objectContaining({ cmd: "edited" })] })] : [] });
    } finally {
      await application?.close();
      await vite.close();
      await rm(directory, { recursive: true, force: true });
    }
  }, 30_000);
});
