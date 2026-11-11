import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

test("SQL uses selected SQLite, WAL, LFS and Crab sources and joins cancellation", async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-file-query-"));
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
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent", process.platform === "win32" ? "crab.exe" : "crab");
  const offline = path.join(source, ".git-offline"), gitDir = path.join(source, ".git");
  const liveDatabase = path.join(source, "live.sqlite");
  const archiveDatabase = path.join(source, "archive.sqlite");
  const mixedDatabase = path.join(source, "mixed.sqlite");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  try {
    git(["init", "--initial-branch=main"]);
    git(["config", "filter.crab.process", `'${crab.replaceAll("\\", "/").replaceAll("'", "'\\''")}' filter-process`]);
    git(["config", "filter.crab.required", "true"]);
    writeFileSync(path.join(source, ".gitattributes"), "chunks.pq filter=crab -text\n");
    writeFileSync(path.join(source, ".gitignore"), "/live.sqlite-wal\n/live.sqlite-shm\n");
    application = await electron.launch({
      executablePath, args: [`--user-data-dir=${path.join(root, "profile")}`],
      env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")),
    });
    const archiveWal = (value: number) => application!.evaluate(({ app }, { liveDatabase, archiveDatabase, value }) => {
      const require = process.getBuiltinModule("module").createRequire(app.getAppPath() + "/package.json");
      const Database = require("better-sqlite3") as typeof import("better-sqlite3");
      const { copyFileSync } = require("node:fs") as typeof import("node:fs");
      const fixture = globalThis as typeof globalThis & { crabWalFixture?: InstanceType<typeof Database> };
      if (!fixture.crabWalFixture) {
        const writer = new Database(liveDatabase);
        writer.exec("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE data (value INTEGER); INSERT INTO data VALUES (17);");
        writer.exec("PRAGMA wal_checkpoint(TRUNCATE);");
        fixture.crabWalFixture = writer;
      }
      const writer = fixture.crabWalFixture;
      writer.prepare("UPDATE data SET value=?").run(value);
      copyFileSync(liveDatabase, archiveDatabase);
      copyFileSync(liveDatabase + "-wal", archiveDatabase + "-wal");
      if (value === 113) writer.exec("BEGIN; UPDATE data SET value=991;");
    }, { liveDatabase, archiveDatabase, value });
    const writeLfsPointer = (file: string, payload: Buffer) => {
      const oid = createHash("sha256").update(payload).digest("hex");
      const object = path.join(gitDir, "lfs", "objects", oid.slice(0, 2), oid.slice(2, 4), oid);
      mkdirSync(path.dirname(object), { recursive: true });
      writeFileSync(object, payload);
      writeFileSync(file, `version https://git-lfs.github.com/spec/v1\noid sha256:${oid}\nsize ${payload.length}\n`);
    };
    const writeRevision = async (phase: "before" | "after") => {
      const payload = readFileSync(path.join(__dirname, `../fixtures/metadata-${phase}.parquet`));
      writeLfsPointer(path.join(source, "data.parquet"), payload);
      const chunks = path.join(source, "chunks.pq");
      writeFileSync(chunks, payload);
      await application!.evaluate(({ app }, { file, phase }) => {
        const require = process.getBuiltinModule("module").createRequire(app.getAppPath() + "/package.json");
        const Database = require("better-sqlite3") as typeof import("better-sqlite3");
        const db = new Database(file);
        try {
          db.exec("CREATE TABLE IF NOT EXISTS readings (marker TEXT, value INTEGER); DELETE FROM readings;");
          db.prepare("INSERT INTO readings VALUES (?, ?)").run(phase, phase === "before" ? 17 : 91);
        } finally {
          db.close();
        }
      }, { file: path.join(source, "data.sqlite"), phase });
      git(["add", "."]);
      const chunkPointer = git(["show", ":chunks.pq"]);
      expect(chunkPointer).not.toEqual(payload);
      writeFileSync(chunks, chunkPointer);
      git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", phase]);
    };
    await archiveWal(91);
    writeFileSync(path.join(source, "corrupt.sqlite"), readFileSync(archiveDatabase));
    const corruptWal = readFileSync(archiveDatabase + "-wal");
    corruptWal[corruptWal.length - 1] ^= 1;
    writeFileSync(path.join(source, "corrupt.sqlite-wal"), corruptWal);
    await writeRevision("before");
    git(["tag", "query-before"]);
    await archiveWal(113);
    writeFileSync(mixedDatabase, readFileSync(archiveDatabase));
    writeLfsPointer(mixedDatabase + "-wal", readFileSync(archiveDatabase + "-wal"));
    await writeRevision("after");
    const index = readFileSync(path.join(gitDir, "index"));
    const pointer = readFileSync(path.join(source, "data.parquet"));
    const chunkPointer = readFileSync(path.join(source, "chunks.pq"));
    const database = readFileSync(path.join(source, "data.sqlite"));
    const originalWalFiles = [liveDatabase, liveDatabase + "-wal", archiveDatabase, archiveDatabase + "-wal", mixedDatabase, mixedDatabase + "-wal"]
      .map(file => ({ file, bytes: readFileSync(file) }));
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "file-query", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    const revision = page.getByRole("combobox").filter({ has: page.getByRole("option", { name: "HEAD (default)", exact: true }) });
    await revision.selectOption({ label: "query-before" });
    const open = (file: string) => page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
    const editor = page.getByRole("textbox", { name: "SQL query", exact: true });
    const run = page.getByRole("button", { name: "Run", exact: true });
    const sql = page.getByRole("button", { name: "SQL", exact: true });
    const cell = (value: string) => page.getByRole("cell", { name: value, exact: true });
    const childPid = (parent: number, command: string) => {
      const native = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" })
        .split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line))
        .find(row => row && Number(row[2]) === parent && row[3].includes(command));
      return native ? Number(native[1]) : undefined;
    };
    const nativePid = () => childPid(application!.process().pid!, "/agent/crab-desktop-agent ");
    await open("data.sqlite");
    await expect(cell("before")).toBeVisible();
    await sql.click();
    await expect(cell("before")).toBeVisible();
    await expect(cell("17")).toBeVisible();
    const sqliteDraft = "SELECT marker, value FROM readings WHERE value = 17";
    await editor.fill(sqliteDraft);
    await open("data.parquet");
    await sql.click();
    await expect(run).toBeEnabled();
    const historical = "SELECT count(*) AS selected_rows FROM data";
    await editor.fill(historical);
    await run.click();
    await expect(cell("3")).toBeVisible();
    await open("data.sqlite");
    await expect(editor).toHaveValue(sqliteDraft);
    await expect(cell("before")).toBeVisible();
    await open("data.parquet");
    await expect(editor).toHaveValue(historical);
    await expect(cell("3")).toBeVisible();
    if (process.platform !== "win32") {
      renameSync(gitDir, offline);
      const native = nativePid();
      if (!native) throw new Error("Test application's native child was not found");
      process.kill(native, "SIGKILL");
      await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
      renameSync(offline, gitDir);
      await page.getByRole("button", { name: "Retry reopening repository", exact: true }).click();
      await expect(editor).toHaveValue(historical);
      await expect(cell("3")).toBeVisible();
    }
    const longQuery = "SELECT sum(sin(i::DOUBLE)) FROM range(1000000000) t(i)";
    await editor.fill(longQuery);
    await run.click();
    const snapshot = path.join(cache, "desktop-content", "database-query", "crab-query-snapshot");
    await expect.poll(() => existsSync(snapshot)).toBe(true);
    await page.getByRole("button", { name: "Cancel query", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "Query cancelled" })).toBeVisible();
    expect(existsSync(snapshot)).toBe(false);
    if (process.platform !== "win32") {
      await run.click();
      await expect.poll(() => existsSync(snapshot)).toBe(true);
      execFileSync(path.join(resources, "agent", "crab-desktop-agent"), ["--index-dir", path.join(root, "other-index")], { env, input: "", timeout: 10_000 });
      expect(existsSync(snapshot)).toBe(true);
      const native = nativePid();
      if (!native) throw new Error("Test application's native child was not found");
      process.kill(native, "SIGKILL");
      const retry = page.getByRole("button", { name: "Retry reopening repository", exact: true });
      await expect.poll(async () => {
        const current = nativePid();
        return (current !== undefined && current !== native) || await retry.isVisible();
      }).toBe(true);
      if (await retry.isVisible()) await retry.click();
      await expect.poll(() => existsSync(snapshot)).toBe(false);
      await expect(editor).toHaveValue(longQuery);
    }
    await editor.fill("SELECT range AS n FROM range(1001)");
    await run.click();
    await expect(page.getByText("(result truncated)", { exact: true })).toBeVisible();
    await editor.fill("SELECT 9007199254740993::BIGINT AS exact_integer, 12345678901234567890.123456789::DECIMAL(38, 9) AS exact_decimal");
    await run.click();
    await expect(cell("9007199254740993")).toBeVisible();
    await expect(cell("12345678901234567890.123456789")).toBeVisible();
    await open("chunks.pq");
    await sql.click();
    await expect(run).toBeEnabled();
    await editor.fill(historical);
    await run.click();
    await expect(cell("3")).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("captured-parquet-sql.png") });
    await open("archive.sqlite");
    await expect(cell("91")).toBeVisible();
    await sql.click();
    await expect(cell("91")).toBeVisible();
    const staged = await call(page, "file_query", { repo_id: "file-query", path: "archive.sqlite", rev: "", sql: "SELECT value FROM data" });
    expect(staged.rows).toEqual([{ value: 113 }]);
    await open("corrupt.sqlite");
    await expect(page.getByRole("alert").filter({ hasText: "Invalid captured SQLite WAL" })).toBeVisible();
    await revision.selectOption({ label: "HEAD (default)" });
    await open("mixed.sqlite");
    await expect(cell("113")).toBeVisible();
    await sql.click();
    await expect(cell("113")).toBeVisible();
    await open("live.sqlite");
    await expect(cell("113")).toBeVisible();
    await sql.click();
    await expect(cell("113")).toBeVisible();
    await expect(cell("991")).toHaveCount(0);
    await page.screenshot({ path: test.info().outputPath("live-sqlite-wal.png") });
    if (process.platform !== "win32") {
      const native = nativePid();
      if (!native) throw new Error("Test application's native child was not found");
      const sqliteWorker = () => childPid(native, "--sqlite-live-query");
      await editor.fill("WITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n<1000000000) SELECT sum(n) FROM seq");
      await run.click();
      await expect.poll(() => sqliteWorker() !== undefined).toBe(true);
      await page.getByRole("button", { name: "Cancel query", exact: true }).click();
      await expect(page.getByRole("status").filter({ hasText: "Query cancelled" })).toBeVisible();
      expect(sqliteWorker()).toBeUndefined();
      await editor.fill("SELECT value FROM data");
      await run.click();
      await expect(cell("113")).toBeVisible();
    }
    await editor.fill("UPDATE data SET value=200");
    await run.click();
    await expect(page.getByRole("alert").filter({ hasText: /not authorized/i })).toBeVisible();
    for (const { file, bytes } of originalWalFiles) expect(readFileSync(file)).toEqual(bytes);
    expect([snapshot, snapshot + "-wal", snapshot + "-shm"].some(existsSync)).toBe(false);
    expect(readFileSync(path.join(gitDir, "index"))).toEqual(index);
    expect(readFileSync(path.join(source, "data.parquet"))).toEqual(pointer);
    expect(readFileSync(path.join(source, "chunks.pq"))).toEqual(chunkPointer);
    expect(readFileSync(path.join(source, "data.sqlite"))).toEqual(database);
  } finally {
    if (existsSync(offline)) renameSync(offline, gitDir);
    try {
      await application?.evaluate(() => {
        const fixture = globalThis as typeof globalThis & { crabWalFixture?: import("better-sqlite3").Database };
        fixture.crabWalFixture?.close();
      });
    } finally {
      await application?.close();
      rmSync(root, { recursive: true, force: true });
    }
  }
});
