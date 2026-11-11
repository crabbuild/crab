import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type {} from "../../src/ipc";
import { packagedExecutable, prepareFixtureSsh } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";

test("physical documents and editor saves cannot cross an unobserved SSH repository replacement", async () => {
  test.skip(process.platform === "win32", "The SSH fixture executes POSIX native binaries");
  test.setTimeout(120_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-file-session-"));
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: path.join(resources, "agent/crab"), CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const fixture = await sshFixture(root, env);
  const source = path.join(fixture.home, "session-project"), file = path.join(source, "note.md"), settingsFile = path.join(source, ".crab/desktop/settings.json");
  mkdirSync(path.dirname(settingsFile), { recursive: true });
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  writeFileSync(file, "ORIGINAL_REMOTE\n", { mode: 0o751 });
  writeFileSync(settingsFile, JSON.stringify({ marker: "OLD_DOCUMENT", theme: "dark" }) + "\n");
  git(["add", "."]); git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Physical session fixture"]);
  const index = readFileSync(path.join(source, ".git/index"));
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  try {
    application = await electron.launch({ executablePath, args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    page.on("dialog", dialog => { if (dialog.type() !== "beforeunload") void dialog.dismiss(); });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const label = "Physical session ownership";
    const saved = await page.evaluate(value => window.crab.ssh.saveProfile(value), { label, host: "127.0.0.1", port: fixture.port, username: "fixture", remotePath: source, authMethod: { type: "key" as const, keyPath: fixture.keyPath }, autoDeployAgent: true, autoReconnect: true, operationTimeoutMs: 30_000 });
    await page.getByRole("button", { name: "SSH Connections", exact: true }).click();
    await prepareFixtureSsh(application!, fixture);
    await page.getByRole("button", { name: `Connect to ${label}`, exact: true }).click();
    await page.getByRole("button", { name: `Browse files on ${label}`, exact: true }).click();
    await page.getByRole("button", { name: "Open Repo", exact: true }).click();
    await expect(page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "session-project", exact: true })).toBeVisible({ timeout: 60_000 });
    const original = await page.evaluate(({ id, source }) => window.crab.ssh.openAsRepo(id, source), { id: saved.id, source });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="note.md"]').click();
    const editor = page.locator(".monaco-editor .view-lines");
    await expect(editor).toContainText("ORIGINAL_REMOTE");
    await application.evaluate(({ BrowserWindow }, { repoId, settingsFile }) => {
      const require = process.getBuiltinModule("module").createRequire(`${process.resourcesPath}/app.asar/package.json`);
      const { RemoteProvider } = require("./dist-electron/remote-provider.cjs") as typeof import("../../electron/remote-provider");
      const providerRead = RemoteProvider.prototype.fsRead, contents = BrowserWindow.getAllWindows()[0].webContents, send = contents.send;
      const events: Parameters<typeof send>[] = [];
      let release!: () => void;
      const gate = new Promise<void>(resolve => { release = resolve; });
      const state = globalThis as typeof globalThis & { physicalSession: { reads: number; flush: () => number; release: () => void; restore: () => void } };
      state.physicalSession = {
        reads: 0, release,
        flush: () => { contents.send = send; for (const args of events) send.apply(contents, args); return events.length; },
        restore: () => { contents.send = send; RemoteProvider.prototype.fsRead = providerRead; release(); },
      };
      // Hold notification delivery, not the main/native owner transition.
      // This exercises a caller that has not yet observed replacement.
      contents.send = ((...args: Parameters<typeof send>) => {
        const event = args[1] as { method?: string; params?: { repo_id?: string } } | undefined;
        if (args[0] === "crab:event" && event?.method === "repo:capabilities-changed" && event.params?.repo_id === repoId) events.push(args);
        else send.apply(contents, args);
      }) as typeof send;
      RemoteProvider.prototype.fsRead = async function (...args: Parameters<typeof providerRead>) {
        const result = await providerRead.apply(this, args);
        if (args[0] === repoId && args[1] === settingsFile && ++state.physicalSession.reads === 1) await gate;
        return result;
      };
    }, { repoId: original.repo_id, settingsFile });
    await page.getByRole("button", { name: /^Task list:/ }).click();
    const tasks = page.getByRole("dialog", { name: "Task list", exact: true });
    await tasks.getByRole("textbox", { name: "New task title", exact: true }).fill("RETAIN_TASK_AFTER_REPLACEMENT");
    await tasks.getByRole("button", { name: "Add task", exact: true }).click();
    await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { physicalSession: { reads: number } }).physicalSession.reads)).toBe(1);
    await page.evaluate(repo_id => window.crab.rpc("close", { repo_id }), original.repo_id);
    const replacement = await page.evaluate(({ id, source }) => window.crab.ssh.openAsRepo(id, source), { id: saved.id, source });
    expect(replacement.native_session_id).not.toBe(original.native_session_id);
    const replacementDocument = { marker: "REPLACEMENT_DOCUMENT", theme: "dark" };
    writeFileSync(settingsFile, JSON.stringify(replacementDocument) + "\n");
    const beforeWrite = fixture.requests.filter(request => request.method === "write_text").length;
    await application.evaluate(() => (globalThis as typeof globalThis & { physicalSession: { release: () => void } }).physicalSession.release());
    await expect(tasks.getByRole("alert").or(tasks.getByText("RETAIN_TASK_AFTER_REPLACEMENT", { exact: true }))).toBeVisible();
    writeFileSync(test.info().outputPath("document-handoff-observed.json"), JSON.stringify({ document: JSON.parse(readFileSync(settingsFile, "utf8")), writes: fixture.requests.filter(request => request.method === "write_text").length - beforeWrite }, null, 2));
    await expect(tasks.getByRole("alert")).toContainText("native session ended");
    await expect(tasks.getByRole("textbox", { name: "New task title", exact: true })).toHaveValue("RETAIN_TASK_AFTER_REPLACEMENT");
    expect(JSON.parse(readFileSync(settingsFile, "utf8"))).toEqual(replacementDocument);
    expect(fixture.requests.filter(request => request.method === "write_text").length).toBe(beforeWrite);
    await page.screenshot({ path: test.info().outputPath("stale-document-blocked.png") });
    await tasks.getByRole("button", { name: "Close task list", exact: true }).click();
    const pending = "RETAIN_PENDING_EDITOR_🦀_café";
    await editor.click(); await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
    await page.keyboard.insertText(pending); await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
    await expect(page.getByRole("button", { name: "Retry saving", exact: true })).toBeVisible();
    expect(readFileSync(file, "utf8")).toBe("ORIGINAL_REMOTE\n");
    expect(fixture.requests.filter(request => request.method === "write_text").length).toBe(beforeWrite);
    const staleRead = await page.evaluate(async ({ repoId, file, session }) => {
      try { await window.crab.fsReadFile(repoId, file, 512, session); return null; }
      catch (error) { return error instanceof Error ? error.message : String(error); }
    }, { repoId: original.repo_id, file, session: original.native_session_id });
    expect(staleRead).toContain("native session ended");
    await page.screenshot({ path: test.info().outputPath("stale-buffer-retained.png") });
    expect(await application.evaluate(() => (globalThis as typeof globalThis & { physicalSession: { flush: () => number } }).physicalSession.flush())).toBeGreaterThan(0);
    await expect.poll(() => readFileSync(file, "utf8")).toBe(pending);
    await expect(page.getByLabel("Unsaved changes", { exact: true })).toHaveCount(0);
    await page.getByRole("button", { name: /^Task list:/ }).click();
    await tasks.getByRole("textbox", { name: "New task title", exact: true }).fill("RETAIN_TASK_AFTER_REPLACEMENT");
    await tasks.getByRole("button", { name: "Add task", exact: true }).click();
    await expect(tasks.getByText("RETAIN_TASK_AFTER_REPLACEMENT", { exact: true })).toBeVisible();
    expect(JSON.parse(readFileSync(settingsFile, "utf8"))).toMatchObject({ ...replacementDocument, tasks: [{ title: "RETAIN_TASK_AFTER_REPLACEMENT" }] });
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
    expect(statSync(file).mode & 0o7777).toBe(0o751);
    expect(readdirSync(source).filter(name => name.startsWith(".crab-write-"))).toEqual([]);
    writeFileSync(test.info().outputPath("physical-session-proof.json"), JSON.stringify({ realSshNativeOwnerReplaced: true, oldReadCannotOverwriteReplacement: true, staleBufferSaveRejectedBeforeWire: true, stalePhysicalReadRejected: true, taskInputRetained: true, latestBufferAcknowledgedAfterRuntimeDelivery: true, taskRetryPreservesReplacementDocument: true, indexUnchanged: true, modePreserved: true }, null, 2));
  } finally {
    if (application) {
      await application.evaluate(() => (globalThis as typeof globalThis & { physicalSession?: { restore: () => void } }).physicalSession?.restore()).catch(() => {});
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.evaluate(({ BrowserWindow }) => { for (const window of BrowserWindow.getAllWindows()) window.destroy(); }).catch(() => {});
      await application.close();
    }
    await fixture.close();
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
