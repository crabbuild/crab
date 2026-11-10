import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

test("local publication preserves the source on failure and retries without native services", async () => {
  test.skip(process.platform === "win32", "Requires POSIX modes and process enumeration");
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-local-publication-"));
  const source = path.join(root, "source"), file = path.join(source, "note.txt");
  const metadata = path.join(source, ".git"), offline = path.join(source, ".git-offline");
  const settingsDirectory = path.join(source, ".crab/desktop"), settingsFile = path.join(settingsDirectory, "settings.json");
  mkdirSync(settingsDirectory, { recursive: true });
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", CRAB_CACHE_DIR: path.join(root, "cache") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  writeFileSync(file, "ORIGINAL_ON_DISK\n"); chmodSync(file, 0o751);
  writeFileSync(settingsFile, JSON.stringify({ marker: "PRESERVE_OTHER_SETTINGS" }) + "\n"); chmodSync(settingsFile, 0o640);
  writeFileSync(path.join(source, "unrelated.txt"), "UNRELATED_INDEX_CONTENT\n");
  git(["add", "."]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Local publication fixture"]);
  const index = readFileSync(path.join(metadata, "index"));
  const userTemporaryName = ".crab-write-user-file";
  writeFileSync(path.join(source, userTemporaryName), "USER_OWNED_FILE");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    page.on("dialog", dialog => { if (dialog.type() !== "beforeunload") void dialog.dismiss(); });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "local-publication", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });

    // Settings and editor saves now both use repository-scoped publication;
    // the selected scope must also reach the app's actual theme consumer.
    await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
    await page.getByRole("tab", { name: "Workspace", exact: true }).click();
    await page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox").selectOption("light");
    await expect(page.locator("html")).toHaveClass(/theme-light/);
    expect(JSON.parse(readFileSync(settingsFile, "utf8"))).toMatchObject({ marker: "PRESERVE_OTHER_SETTINGS", theme: expect.any(String) });
    expect(statSync(settingsFile).mode & 0o7777).toBe(0o640);
    expect(readdirSync(settingsDirectory).filter(name => name.startsWith(".crab-write-"))).toEqual([]);

    await application.evaluate(async (_electron, settingsFile) => {
      const fs = process.getBuiltinModule("fs").promises, original = fs.rename;
      const physical = await fs.realpath(settingsFile);
      let release!: () => void;
      const gate = new Promise<void>(resolve => { release = resolve; });
      const state = globalThis as typeof globalThis & { heldSettings: { hits: number; release: () => void } };
      state.heldSettings = { hits: 0, release: () => { fs.rename = original; release(); } };
      fs.rename = async (...args: Parameters<typeof fs.rename>) => {
        if (String(args[1]) === physical && ++state.heldSettings.hits === 1) await gate;
        return original(...args);
      };
    }, settingsFile);
    await page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox").selectOption("dark");
    await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { heldSettings: { hits: number } }).heldSettings.hits)).toBe(1);
    await page.getByRole("group", { name: "Font size", exact: true }).getByRole("spinbutton").fill("18");
    await application.evaluate(({ dialog, app }) => {
      const state = globalThis as typeof globalThis & { settingsQuitPrompts: number };
      state.settingsQuitPrompts = 0;
      dialog.showMessageBoxSync = () => { state.settingsQuitPrompts++; return 0; };
      app.quit();
    });
    await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { settingsQuitPrompts: number }).settingsQuitPrompts)).toBe(1);
    expect(await call(page, "ping", {})).toBeTruthy();
    await application.evaluate(() => (globalThis as typeof globalThis & { heldSettings: { release: () => void } }).heldSettings.release());
    await expect.poll(() => JSON.parse(readFileSync(settingsFile, "utf8"))).toMatchObject({ marker: "PRESERVE_OTHER_SETTINGS", theme: "dark", editorFontSize: 18 });
    await expect(page.locator("html")).toHaveClass(/theme-dark/);
    await page.getByRole("tab", { name: "User", exact: true }).click();
    await page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox").selectOption("light");
    await expect(page.locator("html")).toHaveClass(/theme-dark/);
    expect(JSON.parse(readFileSync(settingsFile, "utf8")).theme).toBe("dark");
    await page.getByRole("tab", { name: "Workspace", exact: true }).click();
    await page.getByRole("button", { name: "Use User value for Theme", exact: true }).click();
    await expect(page.locator("html")).toHaveClass(/theme-light/);
    await expect.poll(() => JSON.parse(readFileSync(settingsFile, "utf8")).theme).toBeUndefined();

    await application.evaluate(async (_electron, settingsFile) => {
      const fs = process.getBuiltinModule("fs").promises, original = fs.open;
      const physical = await fs.realpath(settingsFile);
      const state = globalThis as typeof globalThis & { settingsReadFault: { restore: () => void } };
      state.settingsReadFault = { restore: () => { fs.open = original; } };
      fs.open = async (...args: Parameters<typeof fs.open>) => {
        if (args[1] === "r" && [settingsFile, physical].includes(String(args[0]))) throw Object.assign(new Error("Fixture settings read unavailable"), { code: "EIO" });
        return original(...args);
      };
    }, settingsFile);
    const beforeTask = readFileSync(settingsFile);
    await page.getByRole("button", { name: /^Task list:/ }).click();
    const tasks = page.getByRole("dialog", { name: "Task list", exact: true });
    await tasks.getByRole("textbox", { name: "New task title", exact: true }).fill("RETAIN_TASK_ON_FAILURE");
    await tasks.getByRole("button", { name: "Add task", exact: true }).click();
    await expect(tasks.getByRole("alert")).toContainText("Fixture settings read unavailable");
    await expect(tasks.getByRole("textbox", { name: "New task title", exact: true })).toHaveValue("RETAIN_TASK_ON_FAILURE");
    expect(readFileSync(settingsFile)).toEqual(beforeTask);
    await application.evaluate(() => (globalThis as typeof globalThis & { settingsReadFault: { restore: () => void } }).settingsReadFault.restore());
    await tasks.getByRole("button", { name: "Add task", exact: true }).click();
    await expect(tasks.getByText("RETAIN_TASK_ON_FAILURE", { exact: true })).toBeVisible();
    await tasks.getByRole("button", { name: 'Mark "RETAIN_TASK_ON_FAILURE" as done', exact: true }).click();
    await expect.poll(() => JSON.parse(readFileSync(settingsFile, "utf8")).tasks[0].done).toBe(true);
    await tasks.getByRole("button", { name: 'Delete task "RETAIN_TASK_ON_FAILURE"', exact: true }).click();
    await expect.poll(() => JSON.parse(readFileSync(settingsFile, "utf8")).tasks).toEqual([]);
    await tasks.getByRole("button", { name: "Close task list", exact: true }).click();

    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="note.txt"]').click();
    const editor = page.locator(".monaco-editor .view-lines");
    await expect(editor).toContainText("ORIGINAL_ON_DISK");
    await expect(editor).toHaveCSS("font-size", "18px");
    await application.evaluate(async (_electron, file) => {
      const fs = process.getBuiltinModule("fs").promises, original = fs.rename;
      const physical = await fs.realpath(file);
      let release!: () => void;
      const gate = new Promise<void>(resolve => { release = resolve; });
      const state = globalThis as typeof globalThis & { publicationFault: { hits: number; restore: () => void; release: () => void } };
      state.publicationFault = { hits: 0, release, restore: () => { fs.rename = original; release(); } };
      fs.rename = async (...args: Parameters<typeof fs.rename>) => {
        if (String(args[1]) === physical) {
          state.publicationFault.hits++;
          await gate;
          throw Object.assign(new Error("Fixture publication failed"), { code: "EIO" });
        }
        return original(...args);
      };
    }, file);
    const value = "RETRY_COMPLETE_UTF8_🦀_café";
    await editor.click();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
    await page.keyboard.insertText(value);
    await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
    await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { publicationFault: { hits: number } }).publicationFault.hits)).toBeGreaterThan(0);
    const temporary = readdirSync(source).filter(name => name.startsWith(".crab-write-") && name !== userTemporaryName);
    expect(temporary).toHaveLength(1);
    const status = await call(page, "git_status_porcelain", { repo_id: handle.repo_id });
    const statusPaths = (status.entries as Array<{ path: string }>).map(entry => entry.path);
    expect(statusPaths).not.toContain(temporary[0]);
    expect(statusPaths).toContain(userTemporaryName);
    await application.evaluate(() => (globalThis as typeof globalThis & { publicationFault: { release: () => void } }).publicationFault.release());
    await expect(page.getByRole("button", { name: "Retry saving", exact: true })).toBeVisible();
    expect(readFileSync(file, "utf8")).toBe("ORIGINAL_ON_DISK\n");
    expect(statSync(file).mode & 0o7777).toBe(0o751);
    expect(readdirSync(source).filter(name => name.startsWith(".crab-write-") && name !== userTemporaryName)).toEqual([]);

    renameSync(metadata, offline);
    const row = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).find(row => row && Number(row[2]) === application!.process().pid && row[3].includes("/agent/crab-desktop-agent "));
    if (!row) throw new Error("Owned native agent not found");
    process.kill(Number(row[1]), "SIGKILL");
    await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
    await expect(editor).toContainText(value);
    await application.evaluate(() => (globalThis as typeof globalThis & { publicationFault: { restore: () => void } }).publicationFault.restore());
    await page.getByRole("button", { name: "Retry saving", exact: true }).click();
    await expect.poll(() => readFileSync(file, "utf8")).toBe(value);
    await expect(page.getByLabel("Unsaved changes", { exact: true })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
    expect(statSync(file).mode & 0o7777).toBe(0o751);
    expect(readdirSync(source).filter(name => name.startsWith(".crab-write-") && name !== userTemporaryName)).toEqual([]);
    expect(readFileSync(path.join(offline, "index")).equals(index)).toBe(true);
    await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
    await page.getByRole("tab", { name: "Workspace", exact: true }).click();
    await page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox").selectOption("dark");
    await expect(page.locator("html")).toHaveClass(/theme-dark/);
    expect(JSON.parse(readFileSync(settingsFile, "utf8"))).toMatchObject({ marker: "PRESERVE_OTHER_SETTINGS", theme: "dark" });
    await expect(page.getByRole("button", { name: "Retry reopening repository", exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("saved-with-native-unavailable.png") });
    writeFileSync(test.info().outputPath("local-publication-proof.json"), JSON.stringify({ ownedTemporaryHiddenDuringStatusRead: true, userFilenamePrefixRetained: true, renameFailurePreservedSource: true, exactUtf8RetryWithoutNative: true, modesPreserved: true, indexUnchanged: true, noOwnedTemporaries: true, workspaceThemePreservedOtherSettings: true, serializedSettingsAcknowledged: true, pendingSettingsQuitCancelled: true, userAndWorkspaceScopesApplied: true, failedTaskReadPreservedInputAndDocument: true, taskAddToggleDelete: true, settingsSavedWithoutNative: true }, null, 2));
  } finally {
    if (existsSync(offline)) renameSync(offline, metadata);
    if (application) {
      await application.evaluate(() => {
        const state = globalThis as typeof globalThis & { publicationFault?: { restore: () => void }; settingsReadFault?: { restore: () => void }; heldSettings?: { release: () => void } };
        state.publicationFault?.restore(); state.settingsReadFault?.restore(); state.heldSettings?.release();
      }).catch(() => {});
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.evaluate(({ BrowserWindow }) => { for (const window of BrowserWindow.getAllWindows()) window.destroy(); }).catch(() => {});
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
