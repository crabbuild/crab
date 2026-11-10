import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

for (const outcome of ["published", "failed"] as const) test(`discard quit drains an admitted local save until ${outcome}`, async () => {
  test.setTimeout(60_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-save-shutdown-"));
  const source = path.join(root, "source"), file = path.join(source, "note.txt"), audit = path.join(root, "quit.json");
  const env: NodeJS.ProcessEnv = { ...process.env, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", CRAB_CACHE_DIR: path.join(root, "cache") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  writeFileSync(env.GIT_CONFIG_GLOBAL!, "");
  execFileSync("git", ["init", "--initial-branch=main", source], { env });
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  writeFileSync(file, "ORIGINAL\n", { mode: 0o751 }); git(["add", "."]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Save shutdown fixture"]);
  const index = readFileSync(path.join(source, ".git/index"));
  const admitted = "ADMITTED_UTF8_🦀_café", discarded = "DISCARD_THIS_LATER_BUFFER";
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    page.on("dialog", dialog => { if (dialog.type() !== "beforeunload") void dialog.dismiss(); });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "save-shutdown", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="note.txt"]').click();
    const editor = page.locator(".monaco-editor .view-lines");
    await expect(editor).toContainText("ORIGINAL");
    await application.evaluate(async ({ app, dialog }, { file, audit, outcome }) => {
      const fs = process.getBuiltinModule("fs"), physical = await fs.promises.realpath(file), rename = fs.promises.rename, unlink = fs.promises.unlink;
      let release!: () => void, prompts = 0, temporary: string | undefined;
      const gate = new Promise<void>(resolve => { release = resolve; });
      const state = globalThis as typeof globalThis & { saveShutdown: { admitted: number; release: () => void } };
      state.saveShutdown = { admitted: 0, release: () => { fs.promises.rename = rename; fs.promises.unlink = unlink; release(); } };
      fs.promises.rename = async (...args: Parameters<typeof rename>) => {
        if (String(args[1]) === physical) {
          state.saveShutdown.admitted++;
          temporary = String(args[0]);
          if (outcome === "failed") throw Object.assign(new Error("Fixture publication failed during Quit"), { code: "EIO" });
          await gate;
        }
        return rename(...args);
      };
      fs.promises.unlink = async (...args: Parameters<typeof unlink>) => {
        if (String(args[0]) === temporary) await gate;
        return unlink(...args);
      };
      dialog.showMessageBoxSync = () => { prompts++; return 1; };
      app.once("will-quit", () => {
        fs.writeFileSync(audit, JSON.stringify({ prompts, released: false }));
        // Model a filesystem operation that finishes after teardown begins.
        // app.exit bypasses live timers, so an unjoined save loses this gate.
        setTimeout(() => { fs.writeFileSync(audit, JSON.stringify({ prompts, released: true })); release(); }, 1000);
      });
    }, { file, audit, outcome });
    await editor.click();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
    await page.keyboard.insertText(admitted);
    await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
    await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { saveShutdown: { admitted: number } }).saveShutdown.admitted)).toBe(1);
    expect(readdirSync(source).filter(name => name.startsWith(".crab-write-"))).toHaveLength(1);
    expect(readFileSync(file, "utf8")).toBe("ORIGINAL\n");
    await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
    await page.keyboard.insertText(discarded);
    await expect(editor).toContainText(discarded);
    await page.screenshot({ path: test.info().outputPath("pending-save-before-discard-quit.png") });
    const child = application.process();
    await application.close(); application = undefined;
    writeFileSync(test.info().outputPath("observed-shutdown.json"), JSON.stringify({ outcome, ...JSON.parse(readFileSync(audit, "utf8")), exitCode: child.exitCode, content: readFileSync(file, "utf8"), temporaries: readdirSync(source).filter(name => name.startsWith(".crab-write-")), indexUnchanged: readFileSync(path.join(source, ".git/index")).equals(index) }, null, 2));
    expect(JSON.parse(readFileSync(audit, "utf8"))).toEqual({ prompts: 1, released: true });
    expect(child.exitCode).toBe(0);
    expect(readFileSync(file, "utf8")).toBe(outcome === "published" ? admitted : "ORIGINAL\n");
    if (process.platform !== "win32") expect(statSync(file).mode & 0o7777).toBe(0o751);
    expect(readdirSync(source).filter(name => name.startsWith(".crab-write-"))).toEqual([]);
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
    writeFileSync(test.info().outputPath("save-shutdown-proof.json"), JSON.stringify({ outcome, discardPrompt: true, admittedSaveDrained: true, laterBufferDiscarded: true, ownedTemporaryRemoved: true, indexUnchanged: true }, null, 2));
  } finally {
    if (application) {
      await application.evaluate(() => (globalThis as typeof globalThis & { saveShutdown?: { release: () => void } }).saveShutdown?.release()).catch(() => {});
      await application.evaluate(({ BrowserWindow }) => { for (const window of BrowserWindow.getAllWindows()) window.destroy(); }).catch(() => {});
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
