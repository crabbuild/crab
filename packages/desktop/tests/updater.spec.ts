import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { parse } from "yaml";
import type { BrowserWindow } from "electron";

const { updater, listeners, handlers, send } = vi.hoisted(() => {
  const listeners = new Map<string, (payload?: unknown) => void>();
  const handlers = new Map<string, () => Promise<unknown>>();
  return {
    listeners, handlers, send: vi.fn(),
    updater: {
      on: vi.fn((event: string, listener: (payload?: unknown) => void) => { listeners.set(event, listener); }),
      checkForUpdates: vi.fn(), downloadUpdate: vi.fn(), quitAndInstall: vi.fn(),
      autoDownload: true, autoInstallOnAppQuit: false, allowDowngrade: true,
    },
  };
});

vi.mock("electron-updater", () => ({ autoUpdater: updater }));
vi.mock("electron-log", () => ({}));
vi.mock("electron", () => ({
  ipcMain: { handle: (channel: string, handler: () => Promise<unknown>) => handlers.set(channel, handler) },
  app: { getVersion: () => "2.0.0" },
}));

import { initAutoUpdater, stopAutoUpdater } from "../electron/updater";

describe("desktop updates", () => {
  beforeEach(() => {
    vi.useFakeTimers(); vi.clearAllMocks(); listeners.clear(); handlers.clear();
    updater.checkForUpdates.mockResolvedValue({ updateInfo: { version: "2.0.1" } });
    updater.downloadUpdate.mockResolvedValue([]);
    initAutoUpdater({ isDestroyed: () => false, webContents: { send } } as unknown as BrowserWindow);
  });
  afterEach(() => { stopAutoUpdater(); vi.clearAllTimers(); vi.useRealTimers(); });

  it("uses the open-source package repository for update publishing", () => {
    const config = parse(readFileSync(new URL("../electron-builder.yml", import.meta.url), "utf8"));
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
    const repository = new URL(pkg.repository.url);
    expect(`${config.publish.owner}/${config.publish.repo}`).toBe(repository.pathname.slice(1).replace(/\.git$/, ""));
  });

  it.each(["ERR_UPDATER_CHANNEL_FILE_NOT_FOUND", "ERR_UPDATER_NO_PUBLISHED_VERSIONS"])(
    "reports %s as unpublished desktop updates, without leaking provider diagnostics", code => {
      listeners.get("error")?.(Object.assign(new Error("HTTP 404: response headers and internal stack"), { code }));
      expect(send).toHaveBeenLastCalledWith("updater:unavailable", {
        message: "Automatic desktop updates are not published for this platform yet.",
      });
    },
  );

  it("keeps network and malformed metadata failures visible with concise copy", () => {
    listeners.get("error")?.(new Error("HTTP 503: response headers and internal stack"));
    expect(send).toHaveBeenLastCalledWith("updater:error", { message: "Could not update Crab. Check your connection and try again." });
  });

  it.each(["updater:check", "updater:download"])("sanitizes failures returned by %s", async channel => {
    const error = Object.assign(new Error("HTTP 404: internal provider stack"), { code: "ERR_UPDATER_CHANNEL_FILE_NOT_FOUND" });
    const operation = channel === "updater:check" ? updater.checkForUpdates : updater.downloadUpdate;
    operation.mockImplementationOnce(async () => { listeners.get("error")?.(error); throw error; });
    await expect(handlers.get(channel)?.()).resolves.toEqual({ success: false, error: "Automatic desktop updates are not published for this platform yet." });
  });

  it("forwards a genuine desktop update without downloading or allowing a downgrade", () => {
    listeners.get("update-available")?.({ version: "2.0.1", releaseDate: "2026-11-02", releaseNotes: "Desktop update" });
    expect(send).toHaveBeenLastCalledWith("updater:available", { version: "2.0.1", releaseDate: "2026-11-02", releaseNotes: "Desktop update" });
    expect(updater.autoDownload).toBe(false);
    expect(updater.allowDowngrade).toBe(false);
    expect(updater.downloadUpdate).not.toHaveBeenCalled();
  });

  it("cancels the launch check as well as periodic checks on shutdown", async () => {
    stopAutoUpdater();
    await vi.advanceTimersByTimeAsync(4 * 60 * 60 * 1000);
    expect(updater.checkForUpdates).not.toHaveBeenCalled();
  });
});
