import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, realpathSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { RepoHandle } from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";

test("desktop SSH connection opens and reads a real logical large file through the deployed runtime", async () => {
  test.skip(process.platform === "win32", "The local remote-host fixture executes POSIX native binaries");
  test.setTimeout(180_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent/crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-ui-"));
  const profile = path.join(root, "profile");
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: "/not-the-bundled-crab", CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const fixture = await sshFixture(root, env);
  const source = path.join(fixture.home, "remote-project"); mkdirSync(source);
  const setupEnv = { ...env, CRAB_BIN: crab, PATH: `${path.dirname(crab)}${path.delimiter}${env.PATH ?? ""}` };
  const git = (args: string[]) => execFileSync("git", args, { cwd: source, env: setupEnv });
  git(["init", "--initial-branch=main"]); mkdirSync(path.join(source, ".crab"));
  writeFileSync(path.join(source, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, "cache/chunks"))}\n`);
  git(["config", "filter.crab.process", "crab filter-process"]); git(["config", "filter.crab.required", "true"]);
  const file = "remote.safetensors";
  writeFileSync(path.join(source, ".gitattributes"), `${file} filter=crab -text\n`);
  writeFileSync(path.join(source, ".gitignore"), ".crab/\n");
  const marker = "REMOTE_SSH_LOGICAL_CONTENT_VERIFIED";
  const size = 4 * 1024 ** 2;
  const header = Buffer.from(JSON.stringify({ [marker]: { dtype: "U8", shape: [size], data_offsets: [0, size] } }));
  const prefix = Buffer.alloc(8); prefix.writeBigUInt64LE(BigInt(header.length));
  const content = Buffer.concat([prefix, header, Buffer.alloc(size, 0x42)]);
  writeFileSync(path.join(source, file), content); git(["add", ".gitattributes", ".gitignore", file]);
  const pointer = git(["show", `:${file}`]); expect(pointer.length).toBeLessThan(1024); writeFileSync(path.join(source, file), pointer);
  git(["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Large SSH file"]);
  const local = path.join(root, "local"); mkdirSync(local);
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: local, env: setupEnv });
  execFileSync("git", ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "Navigation fixture"], { cwd: local, env: setupEnv });
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  const label = "Native runtime verification";
  try {
    const electronEnv = Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string"));
    application = await electron.launch({ executablePath, env: electronEnv, args: [`--user-data-dir=${profile}`] });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-48_000); });
    const page = await application.firstWindow();
    page.on("pageerror", error => { stderr += `\nRenderer error: ${error.stack}`; });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const handle = await call(page, "open_local", { repo_id: "ssh-navigation", path: local }) as unknown as RepoHandle;
    await page.evaluate(({ handle, local }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: local } })), { handle, local });
    const savedProfile = await page.evaluate(async value => {
      if (!window.crab) throw new Error("Missing desktop bridge");
      return window.crab.ssh.saveProfile(value);
    }, { label, host: "127.0.0.1", port: fixture.port, username: "fixture", remotePath: source, authMethod: { type: "key" as const, keyPath: fixture.keyPath }, autoDeployAgent: true, autoReconnect: false });
    const id = savedProfile.id;
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "SSH Connections", exact: true }).click();
    await page.getByRole("button", { name: `Connect to ${label}`, exact: true }).click();
    const trust = page.getByRole("dialog", { name: "Verify Host Key", exact: true });
    await expect(trust).toBeVisible();
    await trust.getByRole("button", { name: "Accept", exact: true }).click();
    await expect(page.getByRole("button", { name: `Browse files on ${label}`, exact: true })).toBeVisible({ timeout: 60_000 });
    await page.screenshot({ path: test.info().outputPath("ssh-runtime-connected.png") });
    await page.getByRole("button", { name: `Browse files on ${label}`, exact: true }).click();
    await page.getByRole("button", { name: "Open Repo", exact: true }).click();
    await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(`ssh://${id}${source}`);
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator(`[role="treeitem"][data-item-path="${file}"]`).click();
    await expect(page.getByText(marker, { exact: false }).first()).toBeVisible({ timeout: 30_000 });
    expect(fixture.requests.some(request => request.repo_id === `ssh://${id}${source}` && request.path === file && request.method === "preview_safetensors")).toBe(true);
    expect(readFileSync(path.join(source, file))).toEqual(pointer);
    await page.screenshot({ path: test.info().outputPath("ssh-large-file-visible.png") });
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
    await expect(page.getByText(/materialized on disk/)).toBeVisible();
    expect(readFileSync(path.join(source, file))).toEqual(content);
    expect(git(["show", `:${file}`])).toEqual(pointer);
    await page.screenshot({ path: test.info().outputPath("ssh-large-file-materialized.png") });
    const paths = await application.evaluate(({ app }) => ({ userData: app.getPath("userData") }));
    expect(realpathSync(paths.userData)).toBe(realpathSync(profile));
    const saved = JSON.parse(readFileSync(path.join(profile, "ssh-profiles.json"), "utf8")) as Array<{ id: string }>;
    expect(saved.map(value => value.id)).toEqual([id]);
    const result = { bytes: content.length, deployedBytes: fixture.uploadedBytes, previewPreservedPointer: true, materializedExactly: true, indexPreservedPointer: true, profile: paths.userData, observedNativeRequests: fixture.requests.filter(request => request.repo_id?.startsWith("ssh://")), authMethod: "key_file" };
    writeFileSync(test.info().outputPath("ssh-ui-proof.json"), JSON.stringify(result, null, 2));
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "SSH Connections", exact: true }).click();
    await page.getByRole("button", { name: `Disconnect from ${label}`, exact: true }).click();
    await expect.poll(() => fixture.commands.size).toBe(0);
  } finally {
    if (application) {
      const page = application.windows()[0];
      if (page) {
        await page.screenshot({ path: test.info().outputPath("ssh-ui-final-state.png") }).catch(() => {});
        writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      }
      await application.close();
    }
    await fixture.close();
    writeFileSync(test.info().outputPath("observed-native-requests.json"), JSON.stringify(fixture.requests, null, 2));
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
