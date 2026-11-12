import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { chooseSelect } from "../helpers/select";
import { call, packagedExecutable } from "./packaged-app";

test("captured Git text and SQLite reject wrong object bodies and recover after repair", async () => {
  test.setTimeout(60_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-git-integrity-"));
  const source = path.join(root, "source"), cache = path.join(root, "cache");
  mkdirSync(source);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = {
    ...process.env, CRAB_CACHE_DIR: cache, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1",
  };
  delete env.GIT_DIR;
  delete env.GIT_WORK_TREE;
  delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    const originalText = "captured original body\n".repeat(512);
    const wrongText = "replaced incorrect row\n".repeat(512);
    expect(Buffer.byteLength(wrongText)).toBe(Buffer.byteLength(originalText));
    writeFileSync(path.join(source, "data.txt"), originalText);
    application = await electron.launch({
      executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`],
      env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")),
    });
    await application.evaluate(({ app }, source) => {
      const require = process.getBuiltinModule("module").createRequire(app.getAppPath() + "/package.json");
      const Database = require("better-sqlite3") as typeof import("better-sqlite3");
      for (const [name, value] of [["data.sqlite", 17], ["replacement.sqlite", 93]] as const) {
        const db = new Database(`${source}/${name}`);
        try {
          db.exec("CREATE TABLE readings (value INTEGER); CREATE TABLE padding (data BLOB); INSERT INTO padding VALUES (zeroblob(262144));");
          db.prepare("INSERT INTO readings VALUES (?)").run(value);
        } finally { db.close(); }
      }
    }, source);
    git(["add", "data.txt", "data.sqlite"]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "captured"]);
    git(["tag", "captured"]);
    const index = readFileSync(path.join(source, ".git/index"));
    const originalDatabase = readFileSync(path.join(source, "data.sqlite"));
    expect(originalDatabase.length).toBeGreaterThan(262144);
    expect(readFileSync(path.join(source, "replacement.sqlite")).length).toBe(originalDatabase.length);
    writeFileSync(path.join(source, "replacement.txt"), wrongText);
    const objects = ["data.txt", "data.sqlite"].map(file => {
      const oid = git(["rev-parse", `captured:${file}`]).toString().trim();
      const object = path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2));
      const original = readFileSync(object);
      const replacement = git(["hash-object", "--no-filters", "-w", file.replace("data.", "replacement.")]);
      return { file, oid, object, original, replacement: replacement.toString().trim() };
    });
    // Replace compressed objects, preserving valid framing and captured sizes.
    // Git cat-file itself does not authenticate these loose object bodies.
    for (const object of objects) {
      const replacement = path.join(source, ".git/objects", object.replacement.slice(0, 2), object.replacement.slice(2));
      chmodSync(object.object, 0o600);
      writeFileSync(object.object, readFileSync(replacement));
    }
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "git-integrity", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await chooseSelect(page.getByRole("combobox").filter({ hasText: "HEAD (default)" }), { label: "captured" });
    const open = (file: string) => page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
    await open("data.txt");
    await expect(page.getByText(/Failed to read data.txt:.*checksum differs/)).toBeVisible();
    await expect(page.getByText("replaced incorrect row", { exact: true })).toHaveCount(0);
    await expect(call(page, "read_bytes", { repo_id: "git-integrity", path: "data.txt", rev: "captured", max_bytes: 1024 * 1024 })).rejects.toThrow(/checksum differs/);
    await page.screenshot({ path: test.info().outputPath("git-text-checksum-error.png") });
    await open("data.sqlite");
    await expect(page.getByRole("alert").filter({ hasText: "checksum differs" })).toBeVisible();
    await expect(call(page, "file_query", { repo_id: "git-integrity", path: "data.sqlite", rev: "captured", sql: "SELECT value FROM readings" })).rejects.toThrow(/checksum differs/);
    const snapshot = path.join(cache, "desktop-content/database-query/crab-query-snapshot");
    expect([snapshot, snapshot + "-wal", snapshot + "-shm"].some(existsSync)).toBe(false);
    await page.screenshot({ path: test.info().outputPath("git-sqlite-checksum-error.png") });
    for (const object of objects) writeFileSync(object.object, object.original);
    const close = (file: string) => page.getByRole("button", { name: file, exact: true }).locator("..").getByRole("button", { name: "Close tab", exact: true }).click();
    await close("data.sqlite");
    await close("data.txt");
    await open("data.txt");
    const restored = await call(page, "read_bytes", { repo_id: "git-integrity", path: "data.txt", rev: "captured", max_bytes: 1024 * 1024 });
    expect(Buffer.from(String(restored.bytes_b64), "base64").toString()).toBe(originalText);
    await expect(page.locator(".monaco-editor").first()).toBeVisible();
    await open("data.sqlite");
    await expect(page.getByRole("cell", { name: "17", exact: true })).toBeVisible();
    const result = await call(page, "file_query", { repo_id: "git-integrity", path: "data.sqlite", rev: "captured", sql: "SELECT value FROM readings" });
    expect(result.rows).toEqual([{ value: 17 }]);
    expect([snapshot, snapshot + "-wal", snapshot + "-shm"].some(existsSync)).toBe(false);
    expect(readFileSync(path.join(source, ".git/index"))).toEqual(index);
    expect(readFileSync(path.join(source, "data.txt"), "utf8")).toBe(originalText);
    expect(readFileSync(path.join(source, "data.sqlite"))).toEqual(originalDatabase);
    await page.screenshot({ path: test.info().outputPath("git-content-repaired.png") });
    writeFileSync(test.info().outputPath("git-integrity-proof.json"), JSON.stringify({
      textBytes: Buffer.byteLength(originalText), databaseBytes: originalDatabase.length,
      rejected: objects.map(({ file, oid }) => ({ file, oid })), repairedValue: 17, snapshotRemoved: true, indexPreserved: true,
    }, null, 2));
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
