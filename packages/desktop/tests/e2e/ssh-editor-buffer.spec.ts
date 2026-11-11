import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type {} from "../../src/ipc";
import { packagedExecutable, prepareFixtureSsh } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";

test("SSH editor retains failed edits across tabs and writes the latest buffer after reconnect", async () => {
  test.skip(process.platform === "win32", "The remote-host fixture executes POSIX native binaries");
  test.setTimeout(120_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-buffer-"));
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: path.join(resources, "agent/crab"), CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const fixture = await sshFixture(root, env);
  const source = path.join(fixture.home, "buffer-project"); mkdirSync(source);
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env });
  git(["init", "--initial-branch=main"]);
  writeFileSync(path.join(source, "pending.md"), "ORIGINAL_REMOTE\n");
  chmodSync(path.join(source, "pending.md"), 0o751);
  writeFileSync(path.join(source, "pending.crab-tmp"), "UNRELATED_USER_FILE\n");
  writeFileSync(path.join(source, "other.txt"), "OTHER_REMOTE_FILE\n");
  git(["add", "."]);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "SSH editor fixture"]);
  const index = readFileSync(path.join(source, ".git/index"));
  const userTemporaryName = ".crab-write-user-file";
  writeFileSync(path.join(source, userTemporaryName), "USER_OWNED_FILE");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  try {
    application = await electron.launch({ executablePath, args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    // Electron owns the beforeunload decision. Playwright's automatic dialog
    // dismissal races that native handler and obscures the actual test result.
    page.on("dialog", dialog => { if (dialog.type() !== "beforeunload") void dialog.dismiss(); });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const label = "Remote buffer recovery";
    const saved = await page.evaluate(value => window.crab!.ssh.saveProfile(value), { label, host: "127.0.0.1", port: fixture.port, username: "fixture", remotePath: source, authMethod: { type: "key" as const, keyPath: fixture.keyPath }, autoDeployAgent: true, autoReconnect: true, operationTimeoutMs: 30_000 });
    const repoId = `ssh://${saved.id}${source}`;
    await page.getByRole("button", { name: "SSH Connections", exact: true }).click();
    await prepareFixtureSsh(application!, fixture);
    await page.getByRole("button", { name: `Connect to ${label}`, exact: true }).click();
    await page.getByRole("button", { name: `Browse files on ${label}`, exact: true }).click();
    await page.getByRole("button", { name: "Open Repo", exact: true }).click();
    await expect(page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "buffer-project", exact: true })).toBeVisible({ timeout: 60_000 });
    const settingsFile = path.join(source, ".crab/desktop/settings.json");
    expect(existsSync(settingsFile)).toBe(false);
    await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
    await page.getByRole("tab", { name: "Workspace", exact: true }).click();
    await page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox").selectOption("light");
    await expect(page.locator("html")).toHaveClass(/theme-light/);
    await expect.poll(() => JSON.parse(readFileSync(settingsFile, "utf8")).theme).toBe("light");
    const initialSettings = JSON.parse(readFileSync(settingsFile, "utf8"));
    writeFileSync(settingsFile, JSON.stringify({ ...initialSettings, marker: "REMOTE_SETTINGS_🦀" }));
    await page.getByRole("button", { name: /^Task list:/ }).click();
    const tasks = page.getByRole("dialog", { name: "Task list", exact: true });
    await tasks.getByRole("textbox", { name: "New task title", exact: true }).fill("REMOTE_TASK_🦀");
    await tasks.getByRole("button", { name: "Add task", exact: true }).click();
    await expect(tasks.getByText("REMOTE_TASK_🦀", { exact: true })).toBeVisible();
    expect(JSON.parse(readFileSync(settingsFile, "utf8"))).toMatchObject({ theme: "light", marker: "REMOTE_SETTINGS_🦀", tasks: [{ title: "REMOTE_TASK_🦀" }] });
    await tasks.getByRole("button", { name: "Close task list", exact: true }).click();
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="pending.md"]').click();
    const editor = page.locator(".monaco-editor .view-lines");
    await expect(editor).toContainText("ORIGINAL_REMOTE");
    const replace = async (value: string) => {
      await editor.click(); await page.keyboard.press(process.platform === "darwin" ? "Meta+A" : "Control+A");
      await page.keyboard.insertText(value); await page.keyboard.press(process.platform === "darwin" ? "Meta+S" : "Control+S");
    };
    const acknowledged = "SAVED_🦀_café", pending = "LATEST_UNACKNOWLEDGED_REMOTE_BUFFER";
    await replace(acknowledged);
    await expect.poll(() => readFileSync(path.join(source, "pending.md"), "utf8")).toBe(acknowledged);
    await expect(page.getByLabel("Unsaved changes", { exact: true })).toHaveCount(0);
    expect(statSync(path.join(source, "pending.md")).mode & 0o777).toBe(0o751);
    expect(readFileSync(path.join(source, "pending.crab-tmp"), "utf8")).toBe("UNRELATED_USER_FILE\n");
    chmodSync(source, 0o500);
    await replace(pending);
    await expect(page.getByRole("button", { name: "Retry saving", exact: true })).toBeVisible();
    await page.locator('[role="treeitem"][data-item-path="other.txt"]').click();
    await expect(editor).toContainText("OTHER_REMOTE_FILE");
    await page.getByRole("button", { name: "pending.md", exact: true }).click();
    await expect(editor).toContainText(pending);
    await page.getByRole("button", { name: "pending.md", exact: true }).click({ button: "right" });
    await page.getByRole("menuitem", { name: /^Close\s*⌘W$/ }).click();
    await page.locator('[role="treeitem"][data-item-path="pending.md"]').click();
    await expect(editor).toContainText(pending);
    fixture.setAccepting(false); fixture.dropConnections();
    await expect.poll(() => page.evaluate(id => window.crab!.ssh.getStatus(id), saved.id)).toBe("reconnecting");
    await expect(page.getByText("Save pending — waiting for connection", { exact: true })).toBeVisible();
    await expect(editor).toContainText(pending);
    expect(readFileSync(path.join(source, "pending.md"), "utf8")).toBe(acknowledged);
    await page.screenshot({ path: test.info().outputPath("remote-pending-buffer.png") });
    chmodSync(source, 0o700); fixture.setAccepting(true);
    await expect.poll(() => readFileSync(path.join(source, "pending.md"), "utf8"), { timeout: 45_000 }).toBe(pending);
    await expect(page.getByLabel("Unsaved changes", { exact: true })).toHaveCount(0);
    expect(fixture.requests.filter(request => request.method === "write_text").every(request => request.repo_id === repoId && ["pending.md", ".crab/desktop/settings.json"].includes(String(request.path)))).toBe(true);
    expect(fixture.requests.filter(request => request.method === "write_text" && request.path === "pending.md").length).toBeGreaterThanOrEqual(3);
    await page.getByRole("button", { name: /^Task list:/ }).click();
    await expect(tasks.getByText("REMOTE_TASK_🦀", { exact: true })).toBeVisible();
    await tasks.getByRole("textbox", { name: "New task title", exact: true }).fill("AFTER_RECONNECT");
    await tasks.getByRole("button", { name: "Add task", exact: true }).click();
    await expect(tasks.getByText("AFTER_RECONNECT", { exact: true })).toBeVisible();
    expect(JSON.parse(readFileSync(settingsFile, "utf8"))).toMatchObject({ marker: "REMOTE_SETTINGS_🦀", theme: "light", tasks: [{ title: "REMOTE_TASK_🦀" }, { title: "AFTER_RECONNECT" }] });
    const documentReads = fixture.requests.filter(request => request.method === "read_text" && request.path === ".crab/desktop/settings.json");
    expect(documentReads.length).toBeGreaterThanOrEqual(3);
    expect(documentReads.every(request => request.repo_id === repoId)).toBe(true);
    await tasks.getByRole("button", { name: "Close task list", exact: true }).click();
    await expect(editor).toContainText(pending);
    const publications = await page.evaluate(async repo_id => {
      return Promise.all(Array.from({ length: 12 }, async (_, index) => {
        const content = String.fromCharCode(65 + index).repeat(128 * 1024);
        const path = `report.part${index}`;
        const result = await window.crab!.rpc(index % 2 === 0 ? "write_text" : "write_bytes", {
          repo_id, path, ...(index % 2 === 0 ? { content } : { bytes_b64: btoa(content) }),
        });
        return { path, content, result };
      }));
    }, repoId);
    for (const publication of publications) {
      expect(readFileSync(path.join(source, publication.path), "utf8")).toBe(publication.content);
      expect(publication.result).toEqual({ written: true, size: publication.content.length });
    }
    expect(statSync(path.join(source, "pending.md")).mode & 0o777).toBe(0o751);
    expect(readFileSync(path.join(source, "pending.crab-tmp"), "utf8")).toBe("UNRELATED_USER_FILE\n");
    expect(readdirSync(source).filter(name => name.startsWith(".crab-write-") && name !== userTemporaryName)).toEqual([]);
    expect(readFileSync(path.join(source, userTemporaryName), "utf8")).toBe("USER_OWNED_FILE");
    await page.getByRole("button", { name: "Refresh file tree", exact: true }).click();
    await expect(page.locator(`[role="treeitem"][data-item-path="${userTemporaryName}"]`)).toBeVisible();
    for (const [filename, marker] of [["report.part0", "A"], ["report.part1", "B"]]) {
      await page.locator(`[role="treeitem"][data-item-path="${filename}"]`).click();
      await expect(editor).toContainText(marker.repeat(64));
    }
    expect(readFileSync(path.join(source, ".git/index")).equals(index)).toBe(true);
    await page.screenshot({ path: test.info().outputPath("native-file-publication.png") });
    writeFileSync(test.info().outputPath("buffer-proof.json"), JSON.stringify({ remoteSettingsCreatedFromTypedAbsence: true, remoteSettingsPreservedUnknownKeys: true, remoteTasksAfterReconnect: true, actualUtf8Write: true, realPermissionFailure: true, retainedAfterTabClose: true, nativeReconnectAcknowledged: true, repositoryScopedWrites: true, indexUnchanged: true, executableModePreserved: true, unrelatedSameStemPreserved: true, competingTextAndBinaryPublications: publications.length, noTemporaryResidue: true }, null, 2));
  } finally {
    chmodSync(source, 0o700); fixture.setAccepting(true);
    if (application) {
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
