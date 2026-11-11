import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type {} from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";

test("local metadata monitoring delivers external changes and joins normal quit without main-thread native watchers", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-watch-owner-"));
  const source = path.join(root, "source");
  const env: NodeJS.ProcessEnv = { ...process.env, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", CRAB_CACHE_DIR: path.join(root, "cache") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  writeFileSync(env.GIT_CONFIG_GLOBAL!, "");
  execFileSync("git", ["init", "--initial-branch=main", source], { env });
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  writeFileSync(path.join(source, "original.txt"), "ORIGINAL"); git(["add", "."]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Watcher owner fixture"]);
  const index = readFileSync(path.join(source, ".git/index"));
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    const page = await application.firstWindow();
    await application.evaluate(async (_electron, root) => {
      const fs = process.getBuiltinModule("fs"), physical = await fs.promises.realpath(root), original = fs.watch;
      const state = globalThis as typeof globalThis & { watchOwnerProbe: { calls: number; restore: () => void } };
      state.watchOwnerProbe = { calls: 0, restore: () => { fs.watch = original; } };
      fs.watch = ((...args: Parameters<typeof fs.watch>) => {
        if ([root, physical].includes(String(args[0]))) state.watchOwnerProbe.calls++;
        return original(...args);
      }) as typeof fs.watch;
    }, source);
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "watch-owner", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.evaluate(root => window.crab.fsWatchStart(root), source);
    expect(await application.evaluate(() => (globalThis as typeof globalThis & { watchOwnerProbe: { calls: number } }).watchOwnerProbe.calls)).toBe(0);
    const external = page.locator('[role="treeitem"][data-item-path="external.txt"]');
    writeFileSync(path.join(source, "external.txt"), "EXTERNAL_CREATE");
    await expect(external).toBeVisible({ timeout: 10_000 });
    await page.screenshot({ path: test.info().outputPath("external-file-visible.png") });
    unlinkSync(path.join(source, "external.txt"));
    await expect(external).toHaveCount(0, { timeout: 10_000 });
    await page.evaluate(root => window.crab.fsWatchStop(root), source);
    await page.evaluate(root => window.crab.fsWatchStart(root), source);
    writeFileSync(path.join(source, "restarted.txt"), "AFTER_RESTART");
    await expect(page.locator('[role="treeitem"][data-item-path="restarted.txt"]')).toBeVisible({ timeout: 10_000 });
    const calls = await application.evaluate(() => (globalThis as typeof globalThis & { watchOwnerProbe: { calls: number } }).watchOwnerProbe.calls);
    expect(calls).toBe(0);
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
    await application.evaluate(() => (globalThis as typeof globalThis & { watchOwnerProbe: { restore: () => void } }).watchOwnerProbe.restore());
    await application.close(); application = undefined;
    writeFileSync(test.info().outputPath("watcher-proof.json"), JSON.stringify({ externalCreateVisible: true, externalDeleteVisible: true, restartVisible: true, mainThreadNativeWatchAcquisitions: calls, indexUnchanged: true, normalQuitJoined: true }, null, 2));
  } finally {
    if (application) {
      await application.evaluate(() => (globalThis as typeof globalThis & { watchOwnerProbe?: { restore: () => void } }).watchOwnerProbe?.restore()).catch(() => {});
      await application.evaluate(({ BrowserWindow }) => { for (const window of BrowserWindow.getAllWindows()) window.destroy(); }).catch(() => {});
      await application.close();
    }
    rmSync(root, { recursive: true, force: true });
  }
});
