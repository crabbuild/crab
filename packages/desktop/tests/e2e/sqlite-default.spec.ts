import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

test("SQLite SQL waits for table metadata and preserves an early explicit query", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-sqlite-default-"));
  const source = path.join(root, "source");
  mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env = { ...Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env });
    await application.evaluate(({ app }, directory) => {
      const modules = process.getBuiltinModule("node:module") as typeof import("node:module");
      const require = modules.createRequire(`${app.getAppPath()}/package.json`);
      const Database = require("better-sqlite3") as typeof import("better-sqlite3");
      const { LocalProvider } = require(`${app.getAppPath()}/dist-electron/local-provider.cjs`) as typeof import("../../electron/local-provider");
      type Gate = { entered: boolean; queries: number; wait: Promise<void>; release: () => void };
      const gates = new Map<string, Gate>();
      for (const name of ["default.sqlite", "explicit.sqlite"]) {
        const db = new Database(`${directory}/${name}`);
        try { db.exec("CREATE TABLE data (value INTEGER); INSERT INTO data VALUES (81);"); }
        finally { db.close(); }
        let release!: () => void;
        const wait = new Promise<void>(resolve => { release = resolve; });
        gates.set(name, { entered: false, queries: 0, wait, release });
      }
      const original = LocalProvider.prototype.request;
      LocalProvider.prototype.request = async function(method, params) {
        const gate = typeof params.path === "string" ? gates.get(params.path) : undefined;
        if (method === "file_query" && gate) gate.queries++;
        const result = await original.call(this, method, params);
        if (method === "preview_sqlite" && gate) { gate.entered = true; await gate.wait; }
        return result;
      };
      (globalThis as typeof globalThis & { sqliteDefaultGates?: Map<string, Gate> }).sqliteDefaultGates = gates;
    }, source);
    git(["add", "."]);
    git(["-c", "user.name=SQLite fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "SQLite defaults"]);
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "sqlite-default", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    for (const name of ["default.sqlite", "explicit.sqlite"]) {
      await page.locator(`[role="treeitem"][data-item-path="${name}"]`).click();
      await expect.poll(() => application!.evaluate((_electron, name) => {
        const state = globalThis as typeof globalThis & { sqliteDefaultGates?: Map<string, { entered: boolean }> };
        return state.sqliteDefaultGates?.get(name)?.entered;
      }, name)).toBe(true);
      await page.getByRole("button", { name: "SQL", exact: true }).click();
      const editor = page.getByRole("textbox", { name: "SQL query", exact: true });
      const run = page.getByRole("button", { name: "Run", exact: true });
      await expect(editor).toBeVisible();
      await expect(run).toBeDisabled();
      await expect(page.getByRole("status").filter({ hasText: "Reading table metadata" })).toBeVisible();
      if (name === "explicit.sqlite") {
        await editor.fill("SELECT 7 AS chosen;");
        await run.click();
        await expect(page.getByRole("cell", { name: "7", exact: true })).toBeVisible();
      }
      await application.evaluate((_electron, name) => {
        const state = globalThis as typeof globalThis & { sqliteDefaultGates?: Map<string, { release: () => void }> };
        state.sqliteDefaultGates?.get(name)?.release();
      }, name);
      if (name === "default.sqlite") {
        await expect(page.getByRole("cell", { name: "81", exact: true })).toBeVisible();
        await expect(editor).toHaveValue('SELECT * FROM "data" LIMIT 100;');
      } else {
        await expect(editor).toHaveValue("SELECT 7 AS chosen;");
        await expect(page.getByRole("cell", { name: "7", exact: true })).toBeVisible();
      }
      expect(await application.evaluate((_electron, name) => {
        const state = globalThis as typeof globalThis & { sqliteDefaultGates?: Map<string, { queries: number }> };
        return state.sqliteDefaultGates?.get(name)?.queries;
      }, name)).toBe(1);
      await page.screenshot({ animations: "disabled", path: test.info().outputPath(name + ".png") });
    }
  } finally {
    await application?.evaluate(() => {
      const state = globalThis as typeof globalThis & { sqliteDefaultGates?: Map<string, { release: () => void }> };
      for (const gate of state.sqliteDefaultGates?.values() ?? []) gate.release();
    }).catch(() => {});
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
