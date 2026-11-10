import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { RepoHandle } from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";

test("saved SSH repositories survive restart, failed restore and deferred selection", async () => {
  test.skip(process.platform === "win32", "The remote-host fixture executes POSIX native binaries");
  test.setTimeout(180_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent/crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-reopen-"));
  const profile = path.join(root, "profile");
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: crab, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const fixture = await sshFixture(root, env);
  const first = path.join(fixture.home, "first-project"), second = path.join(fixture.home, "second-project"), local = path.join(root, "navigation");
  const git = (cwd: string, args: string[]) => execFileSync("git", args, { cwd, env: { ...env, PATH: `${path.dirname(crab)}${path.delimiter}${env.PATH ?? ""}` } });
  for (const directory of [first, second, local]) {
    mkdirSync(directory); git(directory, ["init", "--initial-branch=main"]);
    git(directory, ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "Restore fixture"]);
  }
  mkdirSync(path.join(first, ".crab"));
  writeFileSync(path.join(first, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks"))}\n`);
  git(first, ["config", "filter.crab.process", "crab filter-process"]); git(first, ["config", "filter.crab.required", "true"]);
  const file = "weights.safetensors", marker = "RESTORED_REMOTE_LOGICAL_CONTENT", size = 4 * 1024 ** 2;
  const header = Buffer.from(JSON.stringify({ [marker]: { dtype: "U8", shape: [size], data_offsets: [0, size] } }));
  const prefix = Buffer.alloc(8); prefix.writeBigUInt64LE(BigInt(header.length));
  writeFileSync(path.join(first, ".gitattributes"), "*.safetensors filter=crab -text\n");
  writeFileSync(path.join(first, file), Buffer.concat([prefix, header, Buffer.alloc(size, 0x42)]));
  git(first, ["add", ".gitattributes", file]);
  const pointer = git(first, ["show", `:${file}`]); expect(pointer.length).toBeLessThan(1024);
  writeFileSync(path.join(first, file), pointer);
  writeFileSync(path.join(first, ".gitignore"), ".crab/\n"); git(first, ["add", ".gitignore"]);
  git(first, ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Tracked large file"]);
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  let previews = 0;
  const label = "Saved remote projects";
  async function launch(): Promise<Page> {
    application = await electron.launch({ executablePath, env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), args: [`--user-data-dir=${profile}`] });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-48_000); });
    const page = await application.firstWindow();
    page.on("pageerror", error => { stderr += `\nRenderer error: ${error.stack}`; });
    return page;
  }
  async function close() {
    await application?.close(); application = undefined;
    await expect.poll(() => fixture.commands.size).toBe(0);
  }
  async function showLogicalFile(page: Page) {
    const row = page.locator(`[role="treeitem"][data-item-path="${file}"]`);
    // Clicking the active Files rail button toggles its panel. Preserve a
    // restored visible tree instead of collapsing it before selecting the file.
    if (!await row.isVisible()) {
      const navigation = page.getByRole("navigation", { name: "Primary", exact: true });
      await navigation.getByRole("button", { name: "SSH Connections", exact: true }).click();
      await navigation.getByRole("button", { name: "Files", exact: true }).click();
    }
    await row.click();
    await expect(page.getByText(marker, { exact: false }).first()).toBeVisible({ timeout: 30_000 });
    previews++;
    await expect.poll(() => fixture.requests.filter(request => request.method === "preview_safetensors" && request.path === file).length).toBeGreaterThanOrEqual(previews);
    expect(readFileSync(path.join(first, file))).toEqual(pointer);
    expect(git(first, ["show", `:${file}`])).toEqual(pointer);
  }
  try {
    let page = await launch();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "restore-navigation", path: local }) as unknown as RepoHandle;
    await page.evaluate(({ handle, local }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: local } })), { handle, local });
    const saved = await page.evaluate(value => window.crab!.ssh.saveProfile(value), { label, host: "127.0.0.1", port: fixture.port, username: "fixture", remotePath: first, authMethod: { type: "key" as const, keyPath: fixture.keyPath }, autoDeployAgent: false, autoReconnect: false });
    const firstId = `ssh://${saved.id}${first}`, secondId = `ssh://${saved.id}${second}`;
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "SSH Connections", exact: true }).click();
    await page.getByRole("button", { name: `Connect to ${label}`, exact: true }).click();
    await page.getByRole("dialog", { name: "Verify Host Key", exact: true }).getByRole("button", { name: "Accept", exact: true }).click();
    await page.getByRole("button", { name: `Browse files on ${label}`, exact: true }).click();
    expect(fixture.uploadedBytes).toBe(0);
    await page.getByRole("button", { name: "Open Repo", exact: true }).click();
    await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(firstId);
    await showLogicalFile(page);
    const uploaded = fixture.uploadedBytes;
    expect(uploaded).toBeGreaterThan(0);
    const other = await page.evaluate(({ id, second }) => window.crab!.ssh.openAsRepo(id, second), { id: saved.id, second });
    await page.evaluate(({ handle, path }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path } })), { handle: other, path: secondId });
    await page.getByRole("button", { name: "first-project", exact: true }).click();
    await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(firstId);
    expect(fixture.connectionAttempts).toBe(1);
    await close();

    fixture.setAccepting(false);
    page = await launch();
    const failure = page.getByRole("alert").filter({ hasText: "Could not reopen repository" });
    await expect(failure).toBeVisible();
    expect(await page.evaluate(() => JSON.parse(localStorage.getItem("crab.recentRepos.v1") ?? "[]").map((value: { path: string }) => value.path))).toEqual(expect.arrayContaining([firstId, secondId, local]));
    expect(await page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(firstId);
    await page.screenshot({ path: test.info().outputPath("ssh-restore-failed-retryable.png") });
    fixture.setAccepting(true);
    await failure.getByRole("button", { name: "Retry", exact: true }).click();
    await showLogicalFile(page);
    await expect(failure).not.toBeVisible();
    const attempts = fixture.connectionAttempts;
    await page.getByRole("button", { name: "second-project", exact: true }).click();
    await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(secondId);
    await page.getByRole("button", { name: "first-project", exact: true }).click();
    await showLogicalFile(page);
    expect(fixture.connectionAttempts).toBe(attempts);
    expect(fixture.uploadedBytes).toBe(uploaded);
    await page.screenshot({ path: test.info().outputPath("ssh-restored-two-repositories.png") });
    await close();

    page = await launch();
    await showLogicalFile(page);
    expect(fixture.uploadedBytes).toBe(uploaded);
    await page.evaluate(() => {
      const settings = JSON.parse(localStorage.getItem("crab.settings.v1") ?? "{}");
      localStorage.setItem("crab.settings.v1", JSON.stringify({ ...settings, reopenReposOnStartup: false }));
    });
    await close();
    const beforeDisabled = fixture.connectionAttempts;
    page = await launch();
    await expect(page.getByRole("button", { name: "first-project", exact: true })).toBeVisible();
    expect(fixture.connectionAttempts).toBe(beforeDisabled);
    await page.getByRole("button", { name: "first-project", exact: true }).click();
    await showLogicalFile(page);
    expect(fixture.connectionAttempts).toBe(beforeDisabled + 1);
    expect(fixture.uploadedBytes).toBe(uploaded);
    writeFileSync(test.info().outputPath("ssh-reopen-proof.json"), JSON.stringify({ failedRestorePreservedEntries: true, failedRestorePreservedSelection: true, retryRestoredLogicalPreview: true, deferredRepositoryReusedConnection: true, restartReusedArchive: true, disabledAutoReopenKeptManualAccess: true, pointerUnchanged: true, connectionAttempts: fixture.connectionAttempts, uploadedBytes: uploaded }, null, 2));
  } finally {
    if (application) {
      const page = application.windows()[0];
      if (page) {
        await page.screenshot({ path: test.info().outputPath("ssh-reopen-final-state.png") }).catch(() => {});
        writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      }
      await application.close();
    }
    await fixture.close();
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    writeFileSync(test.info().outputPath("observed-native-requests.json"), JSON.stringify(fixture.requests, null, 2));
    rmSync(root, { recursive: true, force: true });
  }
});
