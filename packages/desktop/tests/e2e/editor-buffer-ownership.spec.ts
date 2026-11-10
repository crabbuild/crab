import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

for (const extension of ["txt", "md"]) test(`failed ${extension} edits survive tab switching, close and reopening`, async () => {
  test.setTimeout(90_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-buffer-owner-"));
  const source = path.join(root, "source"); mkdirSync(source);
  const filename = `pending.${extension}`, file = path.join(source, filename);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", CRAB_CACHE_DIR: path.join(root, "cache") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  writeFileSync(file, "ORIGINAL_ON_DISK\n"); writeFileSync(path.join(source, "other.txt"), "OTHER_FILE_CONTENT\n");
  git(["add", "."]); git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Buffer ownership fixture"]);
  const index = readFileSync(path.join(source, ".git/index"));
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  const value = "RETAIN_THIS_UNACKNOWLEDGED_BUFFER";
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    page.on("pageerror", error => { stderr += `\nRenderer error: ${error.stack}`; });
    page.on("console", message => { if (message.type() === "error") stderr += `\nRenderer console: ${message.text()}`; });
    await application.evaluate(({ dialog }) => {
      const state = globalThis as typeof globalThis & { bufferNativePrompts: string[] };
      state.bufferNativePrompts = [];
      dialog.showMessageBoxSync = () => {
        state.bufferNativePrompts.push("Keep editing");
        return 0;
      };
    });
    // Electron owns the beforeunload decision. Playwright's automatic dialog
    // dismissal races that native handler and obscures the actual test result.
    page.on("dialog", dialog => { if (dialog.type() !== "beforeunload") void dialog.dismiss(); });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "buffer-owner", path: source });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
    const editor = page.locator(".monaco-editor .view-lines");
    await expect(editor).toContainText("ORIGINAL_ON_DISK");
    await application.evaluate((_electron, file) => {
      const fs = process.getBuiltinModule("fs").promises, original = fs.writeFile;
      const state = globalThis as typeof globalThis & { bufferWriteFault: { restore: () => void } };
      state.bufferWriteFault = { restore: () => { fs.writeFile = original; } };
      fs.writeFile = async (...args: Parameters<typeof fs.writeFile>) => {
        if (String(args[0]) === file) throw Object.assign(new Error("Fixture buffer write denied"), { code: "EACCES" });
        return original(...args);
      };
    }, file);
    await editor.click();
    await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
    await page.keyboard.insertText(value);
    await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
    await expect(page.getByRole("button", { name: "Retry saving", exact: true })).toBeVisible();
    await page.locator('[role="treeitem"][data-item-path="other.txt"]').click();
    await expect(editor).toContainText("OTHER_FILE_CONTENT");
    await page.getByRole("button", { name: filename, exact: true }).click();
    await expect(editor).toContainText(value);
    await expect(page.getByRole("button", { name: "Retry saving", exact: true })).toBeVisible();
    await page.getByRole("button", { name: filename, exact: true }).click({ button: "right" });
    await page.getByRole("menuitem", { name: /^Close\s*⌘W$/ }).click();
    await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
    await expect(editor).toContainText(value);
    expect(readFileSync(file, "utf8")).toBe("ORIGINAL_ON_DISK\n");
    const sibling = path.join(root, "sibling"); mkdirSync(sibling);
    execFileSync("git", ["init", "--initial-branch=main"], { cwd: sibling, env });
    writeFileSync(path.join(sibling, "sibling.txt"), "SIBLING_REPOSITORY_CONTENT");
    execFileSync("git", ["add", "sibling.txt"], { cwd: sibling, env });
    execFileSync("git", ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Sibling repository fixture"], { cwd: sibling, env });
    const siblingHandle = await call(page, "open_local", { repo_id: "buffer-sibling", path: sibling });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle: siblingHandle, source: sibling });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="sibling.txt"]').click();
    await expect(editor).toContainText("SIBLING_REPOSITORY_CONTENT");
    await page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "source", exact: true }).click();
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
    await expect(editor).toContainText(value);
    await application.evaluate(({ dialog, app }) => {
      const state = globalThis as typeof globalThis & { bufferQuitPrompts: number };
      state.bufferQuitPrompts = 0;
      dialog.showMessageBoxSync = () => { state.bufferQuitPrompts++; return 0; };
      app.quit();
    });
    await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { bufferQuitPrompts: number }).bufferQuitPrompts)).toBe(1);
    await expect(editor).toContainText(value);
    expect(await call(page, "ping", {})).toBeTruthy();
    await page.screenshot({ path: test.info().outputPath("retained-after-tab-close.png") });
    await application.evaluate(() => (globalThis as typeof globalThis & { bufferWriteFault: { restore: () => void } }).bufferWriteFault.restore());
    await page.getByRole("button", { name: "Retry saving", exact: true }).click();
    await expect.poll(() => readFileSync(file, "utf8")).toBe(value);
    await expect(page.getByLabel("Unsaved changes", { exact: true })).toHaveCount(0);
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
    writeFileSync(test.info().outputPath("buffer-proof.json"), JSON.stringify({ extension, samePendingContentAfterSwitchAndClose: true, realProviderFailure: true, explicitRetryAcknowledged: true, indexUnchanged: true }, null, 2));
  } finally {
    if (application) {
      await application.evaluate(() => (globalThis as typeof globalThis & { bufferWriteFault?: { restore: () => void } }).bufferWriteFault?.restore()).catch(() => {});
      writeFileSync(test.info().outputPath("native-prompts.json"), JSON.stringify(await application.evaluate(() => (globalThis as typeof globalThis & { bufferNativePrompts: string[] }).bufferNativePrompts).catch(() => [])));
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.evaluate(({ BrowserWindow }) => { for (const window of BrowserWindow.getAllWindows()) window.destroy(); }).catch(() => {});
      await application.close();
    }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
