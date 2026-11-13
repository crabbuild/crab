import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { chooseSelect } from "../helpers/select";
import { answerConfirmation, call, packagedExecutable } from "./packaged-app";

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
    const originalMedia = Buffer.alloc(44 + 16 * 192000);
    originalMedia.write("RIFF"); originalMedia.writeUInt32LE(originalMedia.length - 8, 4); originalMedia.write("WAVEfmt ", 8);
    originalMedia.writeUInt32LE(16, 16); originalMedia.writeUInt16LE(1, 20); originalMedia.writeUInt16LE(2, 22);
    originalMedia.writeUInt32LE(48000, 24); originalMedia.writeUInt32LE(192000, 28);
    originalMedia.writeUInt16LE(4, 32); originalMedia.writeUInt16LE(16, 34);
    originalMedia.write("data", 36); originalMedia.writeUInt32LE(originalMedia.length - 44, 40);
    const wrongMedia = Buffer.from(originalMedia);
    wrongMedia[wrongMedia.length - 1] ^= 1;
    writeFileSync(path.join(source, "data.wav"), originalMedia);
    writeFileSync(path.join(source, "replacement.wav"), wrongMedia);
    const originalPayload = Buffer.from("original payload\n");
    for (const [file, payload] of [["pointer.txt", originalPayload], ["replacement-pointer.txt", Buffer.from("replaced payload\n")]] as const) {
      const oid = createHash("sha256").update(payload).digest("hex");
      const object = path.join(source, ".git/lfs/objects", oid.slice(0, 2), oid.slice(2, 4), oid);
      mkdirSync(path.dirname(object), { recursive: true });
      writeFileSync(object, payload);
      writeFileSync(path.join(source, file), `version https://git-lfs.github.com/spec/v1\noid sha256:${oid}\nsize ${payload.length}\n`);
    }
    const originalPointer = readFileSync(path.join(source, "pointer.txt"));
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
    git(["add", "data.txt", "data.sqlite", "pointer.txt", "data.wav"]);
    git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "captured"]);
    git(["tag", "captured"]);
    const index = readFileSync(path.join(source, ".git/index"));
    const originalDatabase = readFileSync(path.join(source, "data.sqlite"));
    expect(originalDatabase.length).toBeGreaterThan(262144);
    expect(readFileSync(path.join(source, "replacement.sqlite")).length).toBe(originalDatabase.length);
    writeFileSync(path.join(source, "replacement.txt"), wrongText);
    const objects = ["data.txt", "data.sqlite", "pointer.txt", "data.wav"].map(file => {
      const oid = git(["rev-parse", `captured:${file}`]).toString().trim();
      const object = path.join(source, ".git/objects", oid.slice(0, 2), oid.slice(2));
      const original = readFileSync(object);
      const replacementFile = file === "pointer.txt" ? "replacement-pointer.txt" : file.replace("data.", "replacement.");
      const replacement = git(["hash-object", "--no-filters", "-w", replacementFile]);
      return { file, oid, object, original, replacement: replacement.toString().trim() };
    });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "git-integrity", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await chooseSelect(page.getByRole("combobox").filter({ hasText: "HEAD (default)" }), { label: "captured" });
    const open = (file: string) => page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
    await expect(page.locator('[role="treeitem"][data-item-path="data.txt"]')).toBeVisible();
    // Listing already authenticates small pointer candidates. Replace objects
    // afterward to exercise each fresh preview/query capture independently.
    for (const object of objects) {
      const replacement = path.join(source, ".git/objects", object.replacement.slice(0, 2), object.replacement.slice(2));
      chmodSync(object.object, 0o600);
      writeFileSync(object.object, readFileSync(replacement));
    }
    await page.getByRole("button", { name: "Refresh file tree", exact: true }).click();
    await expect(page.getByText(/Git blob checksum differs/).first()).toBeVisible();
    await expect(page.locator('[role="treeitem"][data-item-path="data.txt"]')).toBeVisible();
    await open("data.txt");
    await expect(page.getByText(/Failed to read data.txt:.*checksum differs/)).toBeVisible();
    await expect(page.getByText("replaced incorrect row", { exact: true })).toHaveCount(0);
    await expect(call(page, "read_bytes", { repo_id: "git-integrity", path: "data.txt", rev: "captured", max_bytes: 1024 * 1024 })).rejects.toThrow(/checksum differs/);
    const captured = await call(page, "file_open", { repo_id: "git-integrity", path: "data.txt", rev: "captured", native_session_id: handle.native_session_id });
    try {
      await expect(call(page, "read_range", { repo_id: "git-integrity", file_id: captured.file_id, native_session_id: handle.native_session_id, offset: 0, length: 32 })).rejects.toThrow(/checksum differs/);
    } finally {
      await call(page, "file_close", { repo_id: "git-integrity", file_id: captured.file_id, native_session_id: handle.native_session_id });
    }
    await page.screenshot({ path: test.info().outputPath("git-text-checksum-error.png") });
    await open("data.sqlite");
    await expect(page.getByRole("alert").filter({ hasText: "checksum differs" })).toBeVisible();
    await expect(call(page, "file_query", { repo_id: "git-integrity", path: "data.sqlite", rev: "captured", sql: "SELECT value FROM readings" })).rejects.toThrow(/checksum differs/);
    const snapshot = path.join(cache, "desktop-content/database-query/crab-query-snapshot");
    expect([snapshot, snapshot + "-wal", snapshot + "-shm"].some(existsSync)).toBe(false);
    await page.screenshot({ path: test.info().outputPath("git-sqlite-checksum-error.png") });
    await open("pointer.txt");
    await expect(page.getByRole("region", { name: "File details", exact: true }).getByRole("alert").filter({ hasText: "checksum differs" })).toBeVisible();
    await expect(call(page, "read_bytes", { repo_id: "git-integrity", path: "pointer.txt", rev: "captured", max_bytes: 1024 })).rejects.toThrow(/checksum differs/);
    await page.screenshot({ path: test.info().outputPath("git-pointer-checksum-error.png") });
    await open("data.wav");
    const mediaPreview = page.getByRole("region", { name: "Audio preview", exact: true });
    await expect(mediaPreview.getByRole("alert").filter({ hasText: "checksum differs" })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("git-media-checksum-error.png") });
    for (const object of objects) writeFileSync(object.object, object.original);
    await page.getByRole("button", { name: "Refresh file tree", exact: true }).click();
    await expect(page.getByLabel("File list refresh failed", { exact: true })).toHaveCount(0);
    const close = (file: string) => page.getByRole("button", { name: file, exact: true }).locator("..").getByRole("button", { name: "Close tab", exact: true }).click();
    await close("data.wav");
    await close("pointer.txt");
    await close("data.sqlite");
    await close("data.txt");
    await open("data.txt");
    const restored = await call(page, "read_bytes", { repo_id: "git-integrity", path: "data.txt", rev: "captured", max_bytes: 1024 * 1024 });
    expect(Buffer.from(String(restored.bytes_b64), "base64").toString()).toBe(originalText);
    await expect(page.getByText("captured original body", { exact: true }).first()).toBeVisible();
    await open("pointer.txt");
    await expect(page.getByText(createHash("sha256").update(originalPayload).digest("hex"), { exact: true })).toBeVisible();
    const restoredPointer = await call(page, "read_bytes", { repo_id: "git-integrity", path: "pointer.txt", rev: "captured", max_bytes: 1024 });
    expect(Buffer.from(String(restoredPointer.bytes_b64), "base64")).toEqual(originalPayload);
    await open("data.wav");
    const audio = mediaPreview.locator("audio");
    await expect.poll(() => audio.evaluate((node: HTMLMediaElement) => node.duration)).toBe(16);
    await audio.evaluate((node: HTMLMediaElement) => { node.muted = true; node.currentTime = 3.2; });
    await expect.poll(() => audio.evaluate((node: HTMLMediaElement) => !node.seeking && Math.abs(node.currentTime - 3.2) < 0.1)).toBe(true);
    await audio.evaluate((node: HTMLMediaElement) => node.play());
    await expect.poll(() => audio.evaluate((node: HTMLMediaElement) => node.currentTime > 3.3)).toBe(true);
    await audio.evaluate((node: HTMLMediaElement) => node.pause());
    await page.screenshot({ path: test.info().outputPath("git-media-repaired.png") });
    await open("data.sqlite");
    await page.getByRole("button", { name: /^readings/ }).click();
    await expect(page.getByRole("cell", { name: "17", exact: true })).toBeVisible();
    const result = await call(page, "file_query", { repo_id: "git-integrity", path: "data.sqlite", rev: "captured", sql: "SELECT value FROM readings" });
    expect(result.rows).toEqual([{ value: 17 }]);
    expect([snapshot, snapshot + "-wal", snapshot + "-shm"].some(existsSync)).toBe(false);
    expect(readFileSync(path.join(source, ".git/index"))).toEqual(index);
    expect(readFileSync(path.join(source, "data.txt"), "utf8")).toBe(originalText);
    expect(readFileSync(path.join(source, "data.sqlite"))).toEqual(originalDatabase);
    expect(readFileSync(path.join(source, "pointer.txt"))).toEqual(originalPointer);
    expect(readFileSync(path.join(source, "data.wav"))).toEqual(originalMedia);
    await page.screenshot({ path: test.info().outputPath("git-content-repaired.png") });

    await chooseSelect(page.getByRole("combobox").filter({ hasText: "captured" }), { label: "HEAD (default)" });
    writeFileSync(path.join(source, "data.txt"), "unstaged edit before confirmed restoration\n");
    const restoredRow = page.locator('[role="treeitem"][data-item-path="data.txt"]');
    await expect(restoredRow.getByText("M", { exact: true })).toBeVisible();
    // Revision selection returns focus to a shared control. The global palette
    // shortcut must still reach confirmation without moving focus elsewhere.
    await expect(page.getByRole("combobox").filter({ hasText: "HEAD (default)" })).toBeFocused();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+Shift+P" : "Control+Shift+P");
    const palette = page.getByRole("dialog", { name: "Command palette", exact: true });
    await palette.getByRole("combobox").fill("Git: Discard File Changes");
    await palette.getByRole("option").filter({ hasText: "Git: Discard File Changes" }).click();
    await palette.getByRole("combobox").fill("data.txt");
    await palette.getByRole("combobox").press("Enter");
    await answerConfirmation(page, /^Restore /);
    await expect(page.getByText("Restored data.txt from the index", { exact: true })).toBeVisible();
    expect(readFileSync(path.join(source, "data.txt"), "utf8")).toBe(originalText);
    expect(readFileSync(path.join(source, ".git/index"))).toEqual(index);
    await expect(restoredRow.getByText("M", { exact: true })).toHaveCount(0);
    await open("data.txt");
    await expect(page.getByText("captured original body", { exact: true }).first()).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("git-index-restored.png") });
    writeFileSync(test.info().outputPath("git-integrity-proof.json"), JSON.stringify({
      textBytes: Buffer.byteLength(originalText), databaseBytes: originalDatabase.length, mediaBytes: originalMedia.length,
      rejected: objects.map(({ file, oid }) => ({ file, oid })), repairedValue: 17, snapshotRemoved: true,
      indexPreserved: true, listingRecovered: true, restoredThroughUi: true, dirtyStatusCleared: true,
    }, null, 2));
  } catch (error) {
    const page = await application?.firstWindow();
    await page?.screenshot({ path: test.info().outputPath("git-integrity-failure.png") });
    throw error;
  } finally {
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
