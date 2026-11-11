import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { readFileSync, writeFileSync, mkdtempSync, rmSync, cpSync, mkdirSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import * as path from "node:path";
import { tmpdir } from "node:os";
import type { StorageConnectionInput } from "../../electron/storage-connection-types";
import type { AgentEvent } from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";
import type { RepoHandle } from "../../src/ipc";

type S3Profile = StorageConnectionInput & { credentials: { provider: "s3"; access_key_id: string; secret_access_key: string } };
interface Fixture {
  url: string;
  repositories: [string, string];
  profiles: [S3Profile, S3Profile];
  payloads: [{ marker: string; sha256: string; size: number }, { marker: string; sha256: string; size: number }];
  cold_payloads: [{ marker: string; sha256: string; size: number }, { marker: string; sha256: string; size: number }];
}
interface Runtime { repo_id: string; native_session_id: string | null; error?: string }

function isolatedEnvironment(root: string): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(root, "gitconfig") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  writeFileSync(path.join(root, "gitconfig"), "");
  return env;
}

const countParquetRows = "SELECT count(*) AS selected_rows FROM data";

async function queryColdParquet(page: Page, connection: S3Profile, cache: string, expectedRows: number) {
  const snapshot = path.join(cache, "desktop-content", "database-query", "crab-query-snapshot");
  const transport = async (action: "pause_lfs" | "release" | "stats") => {
    const response = await fetch(`${connection.endpoint}/__fixture_${action}`);
    expect(response.ok).toBe(true);
    return await response.json() as { get: number; blocked: number; paused: boolean; writes: number };
  };
  const before = await transport("pause_lfs");
  try {
    await page.locator('[role="treeitem"][data-item-path="query.parquet"]').click();
    await page.getByRole("button", { name: "SQL", exact: true }).click();
    await expect.poll(async () => (await transport("stats")).blocked > 0 && existsSync(snapshot)).toBe(true);
    await page.getByRole("button", { name: "Cancel query", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "Query cancelled" })).toBeVisible();
    expect(existsSync(snapshot)).toBe(false);
    expect((await transport("stats")).paused).toBe(true);
  } finally {
    await transport("release");
  }
  await page.getByRole("textbox", { name: "SQL query", exact: true }).fill(countParquetRows);
  await page.getByRole("button", { name: "Run", exact: true }).click();
  await expect(page.getByRole("cell", { name: String(expectedRows), exact: true })).toBeVisible();
  expect(existsSync(snapshot)).toBe(false);
  const after = await transport("stats");
  expect(after.get).toBeGreaterThan(before.get);
  expect(after.writes).toBe(0);
}

async function rerunReboundParquet(page: Page, expectedRows: number) {
  await page.locator('[role="treeitem"][data-item-path="query.parquet"]').click();
  await expect(page.getByRole("textbox", { name: "SQL query", exact: true })).toHaveValue(countParquetRows);
  await expect(page.getByRole("cell", { name: "3", exact: true })).toHaveCount(0);
  await expect(page.getByRole("alert").filter({ hasText: "Repository storage changed" })).toBeVisible();
  await page.getByRole("button", { name: "Run", exact: true }).click();
  await expect(page.getByRole("cell", { name: String(expectedRows), exact: true })).toBeVisible();
}

test("device storage identities reach SSH cold reads, replacement and saved restart", async () => {
  const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
  test.skip(!descriptor || process.platform === "win32", "Requires owned storage services and a POSIX SSH fixture");
  test.setTimeout(240_000);
  const source = JSON.parse(readFileSync(descriptor!, "utf8")) as Fixture;
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-storage-"));
  const env = isolatedEnvironment(root);
  const remote = await sshFixture(root, { ...env, CRAB_CACHE_DIR: path.join(root, "remote-cache") });
  const directories = ["alpha-project", "beta-project"].map(name => path.join(remote.home, name));
  const git = (directory: string, args: string[]) => execFileSync("git", args, { cwd: directory, env });
  for (const [index, directory] of directories.entries()) {
    cpSync(source.repositories[index], directory, { recursive: true });
    mkdirSync(path.join(directory, ".crab"), { recursive: true });
    writeFileSync(path.join(directory, ".crab/config.toml"), `[cache]\nchunk_cache_dir = ${JSON.stringify(path.join(root, `remote-chunks-${index}`))}\n`);
    for (const file of ["weights.safetensors", "lfs.safetensors", "cold.safetensors", "query.parquet", "archive.sqlite", "archive.sqlite-wal"]) {
      writeFileSync(path.join(directory, file), git(directory, ["show", `:${file}`]));
    }
  }
  const profile = path.join(root, "profile");
  const events: Runtime[] = [];
  const epoch = (repo: string) => events.filter(event => event.repo_id === repo).at(-1)?.native_session_id;
  const sha = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
  const stats = async (index: number) => await (await fetch(source.profiles[index].endpoint + "/__fixture_stats")).json() as { get: number; head: number; writes: number };
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  async function launch() {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-48_000); });
    const page = await application.firstWindow();
    await page.exposeFunction("recordSshStorageRuntime", (value: Runtime) => events.push(value));
    await page.evaluate(() => {
      const target = window as typeof window & { recordSshStorageRuntime(value: unknown): Promise<void> };
      window.crab.onEvent((event: AgentEvent) => { if (event.method === "repo:capabilities-changed") void target.recordSshStorageRuntime(event.params); });
    });
    return page;
  }
  async function close() {
    await application?.close(); application = undefined;
    await expect.poll(() => remote.commands.size).toBe(0);
  }
  async function settings(page: Page) {
    await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    return page.getByRole("region", { name: "Storage connections", exact: true });
  }
  async function fill(page: Page, connection: S3Profile) {
    const section = page.getByRole("region", { name: "Storage connections", exact: true });
    for (const [label, value] of [["Connection name", connection.name], ["Bucket", connection.bucket], ["Signing region", connection.region], ["Endpoint URL (optional)", connection.endpoint], ["Access key ID", connection.credentials.access_key_id], ["Secret access key", connection.credentials.secret_access_key]]) {
      await section.getByLabel(label, { exact: true }).fill(value);
    }
  }
  async function showFile(page: Page, file: string, marker: string) {
    const row = page.locator(`[role="treeitem"][data-item-path="${file}"]`);
    if (!await row.isVisible()) {
      const nav = page.getByRole("navigation", { name: "Primary", exact: true });
      await nav.getByRole("button", { name: "Settings", exact: true }).click();
      await nav.getByRole("button", { name: "Files", exact: true }).click();
    }
    await row.click();
    await expect(page.getByText(marker, { exact: false }).first()).toBeVisible();
  }
  try {
    let page = await launch();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const label = "Cloud-backed SSH";
    const saved = await page.evaluate(value => window.crab.ssh.saveProfile(value), { label, host: "127.0.0.1", port: remote.port, username: "fixture", remotePath: directories[0], authMethod: { type: "key" as const, keyPath: remote.keyPath }, autoDeployAgent: false, autoReconnect: false });
    const [alphaId, betaId] = directories.map(directory => `ssh://${saved.id}${directory}`);
    await page.getByRole("button", { name: "SSH Connections", exact: true }).click();
    await page.getByRole("button", { name: `Connect to ${label}`, exact: true }).click();
    await page.getByRole("dialog", { name: "Verify Host Key", exact: true }).getByRole("button", { name: "Accept", exact: true }).click();
    await page.getByRole("button", { name: `Browse files on ${label}`, exact: true }).click();
    await page.getByRole("button", { name: "Open Repo", exact: true }).click();
    await expect(page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "alpha-project", exact: true })).toBeVisible({ timeout: 60_000 });
    let section = await settings(page);
    for (const connection of source.profiles) {
      await section.getByRole("button", { name: "Add connection", exact: true }).click();
      await fill(page, connection);
      await section.getByRole("button", { name: "Test connection", exact: true }).click();
      await expect(section.getByRole("status").filter({ hasText: "Bucket read/list access verified" })).toBeVisible();
      await section.getByRole("button", { name: "Save connection", exact: true }).click();
      await expect(section.getByRole("button", { name: `Edit ${connection.name}`, exact: true })).toBeVisible();
    }
    const row = (name: string) => section.getByRole("button", { name: `Edit ${name}`, exact: true }).locator("..");
    const initialAlphaEpoch = epoch(alphaId);
    await row("Alpha").getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(section.getByRole("status")).toContainText("Connection selected");
    await expect.poll(() => Boolean(epoch(alphaId) && epoch(alphaId) !== initialAlphaEpoch)).toBe(true);
    const selected = await call(page, "storage_connections_list", { repo_id: alphaId });
    const alphaConnection = String(selected.selected_id);
    const beforeAlpha = await stats(0);
    await showFile(page, "lfs.safetensors", source.payloads[0].marker);
    await queryColdParquet(page, source.profiles[0], path.join(root, "remote-cache"), 3);
    expect(readFileSync(path.join(directories[0], "query.parquet"))).toEqual(git(directories[0], ["show", ":query.parquet"]));
    expect(remote.requests.some(request => request.method === "file_query" && request.repo_id === alphaId)).toBe(true);
    await page.screenshot({ path: test.info().outputPath("ssh-cold-parquet-sql.png") });
    await showFile(page, "archive.sqlite", "81");
    await page.getByRole("button", { name: "SQL", exact: true }).click();
    await expect(page.getByRole("cell", { name: "81", exact: true })).toBeVisible();
    for (const file of ["archive.sqlite", "archive.sqlite-wal"]) {
      expect(readFileSync(path.join(directories[0], file))).toEqual(git(directories[0], ["show", `:${file}`]));
    }
    await showFile(page, "lfs.safetensors", source.payloads[0].marker);
    const alphaEpoch = epoch(alphaId);
    expect(typeof alphaEpoch).toBe("string");
    const oldFile = await call(page, "file_open", { repo_id: alphaId, path: "lfs.safetensors", native_session_id: alphaEpoch });
    const oldMedia = await call(page, "media_open", { repo_id: alphaId, path: "sample.wav", native_session_id: alphaEpoch });
    expect(await page.evaluate(async url => { const response = await fetch(url); await response.arrayBuffer(); return response.status; }, String(oldMedia.url))).toBe(200);
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
    await expect(page.getByText(/materialized on disk/)).toBeVisible();
    expect(sha(readFileSync(path.join(directories[0], "lfs.safetensors")))).toBe(source.payloads[0].sha256);
    expect(git(directories[0], ["show", ":lfs.safetensors"])).toEqual(git(source.repositories[0], ["show", ":lfs.safetensors"]));
    expect((await stats(0)).get).toBeGreaterThan(beforeAlpha.get);
    const bulkPaths = ["lfs.safetensors", "weights.safetensors"];
    for (const file of bulkPaths) writeFileSync(path.join(directories[0], file), git(directories[0], ["show", `:${file}`]));
    const batchIndex = git(directories[0], ["ls-files", "--stage", "-z"]);
    await page.getByRole("button", { name: "Refresh hydration status", exact: true }).click();
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Large Files", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate", exact: true }).last().click();
    await page.getByRole("tab", { name: "Custom", exact: true }).click();
    for (const file of bulkPaths) await page.getByRole("checkbox", { name: new RegExp(file) }).check();
    await page.getByRole("button", { name: /^Hydrate 2 files/ }).click();
    await expect(page.getByRole("main").getByText("Hydrated weights.safetensors", { exact: true })).toBeVisible();
    for (const file of bulkPaths) expect(sha(readFileSync(path.join(directories[0], file)))).toBe(source.payloads[0].sha256);
    expect(git(directories[0], ["ls-files", "--stage", "-z"])).toEqual(batchIndex);
    expect(remote.requests.some(request => request.method === "files_materialize" && request.repo_id === alphaId && request.storage_connection_id === alphaConnection)).toBe(true);
    await page.screenshot({ path: test.info().outputPath("ssh-bulk-hydration.png") });
    // The later credential test must fetch content from storage again.
    writeFileSync(path.join(directories[0], "weights.safetensors"), git(directories[0], ["show", ":weights.safetensors"]));
    const beta: RepoHandle = await page.evaluate(({ id, directory }) => window.crab.ssh.openAsRepo(id, directory), { id: saved.id, directory: directories[1] });
    await page.evaluate(({ handle, path }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path } })), { handle: beta, path: betaId });
    section = await settings(page);
    await row("Beta").getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(section.getByRole("status")).toContainText("Connection selected");
    await expect.poll(() => Boolean(epoch(betaId) && epoch(betaId) !== beta.native_session_id)).toBe(true);
    const betaEpoch = epoch(betaId);
    const beforeBeta = await stats(1);
    await showFile(page, "weights.safetensors", source.payloads[1].marker);
    expect((await stats(1)).get).toBeGreaterThan(beforeBeta.get);
    section = await settings(page);
    await section.getByRole("button", { name: "Edit Alpha", exact: true }).click();
    await fill(page, { ...source.profiles[0], bucket: "other-" + source.profiles[0].bucket });
    await section.getByRole("button", { name: "Save changes", exact: true }).click();
    await expect(section.getByRole("button", { name: "Save changes", exact: true })).toHaveCount(0);
    await section.getByRole("button", { name: "Test Alpha", exact: true }).click();
    await expect(section.getByRole("alert")).toContainText(/bucket|credentials/i);
    await expect.poll(() => Boolean(epoch(alphaId) && epoch(alphaId) !== alphaEpoch)).toBe(true);
    expect(epoch(betaId)).toBe(betaEpoch);
    const unavailable = await page.evaluate(async params => { try { await window.crab.rpc("read_bytes", params); return null; } catch (error) { return error as { message?: string }; } }, { repo_id: alphaId, path: "weights.safetensors", max_bytes: source.payloads[0].size + 1 });
    expect(unavailable).toMatchObject({ message: expect.stringMatching(/bucket|connection/i) });
    const stale = await page.evaluate(async params => { try { await window.crab.rpc("read_range", params); return null; } catch (error) { return error as { code?: string }; } }, { repo_id: alphaId, file_id: oldFile.file_id, native_session_id: alphaEpoch, offset: 0, length: 1 });
    expect(stale).toMatchObject({ code: "NATIVE_SESSION_LOST" });
    await expect.poll(() => page.evaluate(async url => (await fetch(url)).status, String(oldMedia.url))).toBe(410);
    await section.getByRole("button", { name: "Edit Alpha", exact: true }).click();
    await fill(page, source.profiles[0]);
    await section.getByRole("button", { name: "Save changes", exact: true }).click();
    await expect(section.getByRole("button", { name: "Save changes", exact: true })).toHaveCount(0);
    await section.getByRole("button", { name: "Forget Alpha", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Alpha", exact: true })).toHaveCount(0);
    await expect.poll(() => epoch(alphaId)).toBeNull();
    expect(epoch(betaId)).toBe(betaEpoch);
    await page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "alpha-project", exact: true }).click();
    section = await settings(page);
    await section.getByRole("button", { name: "Add connection", exact: true }).click();
    await fill(page, { ...source.profiles[0], name: "Alpha restored" });
    await section.getByRole("button", { name: "Save connection", exact: true }).click();
    await row("Alpha restored").getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(section.getByRole("status")).toContainText("Connection selected");
    await showFile(page, "weights.safetensors", source.payloads[0].marker);
    expect(epoch(alphaId)).not.toBe(alphaEpoch);
    await rerunReboundParquet(page, 3);
    await page.screenshot({ path: test.info().outputPath("ssh-sql-repaired-source.png") });
    expect(remote.requests.some(request => request.method === "file_open" && request.repo_id === alphaId && request.storage_connection_id === alphaConnection)).toBe(true);
    expect(remote.requests.filter(request => request.method === "storage_connection_test")).toHaveLength(3);
    await page.screenshot({ path: test.info().outputPath("ssh-storage-identity-repaired.png") });
    await close();
    const beforeCold = await Promise.all([stats(0), stats(1)]);
    events.length = 0;
    page = await launch();
    await expect(page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "alpha-project", exact: true })).toBeVisible();
    await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(alphaId);
    await expect.poll(() => page.getByRole("button", { name: "Retry reopening repository", exact: true }).count()).toBe(0);
    for (const [index, repoId] of [alphaId, betaId].entries()) {
      await page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: index === 0 ? "alpha-project" : "beta-project", exact: true }).click();
      await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(repoId);
      await expect.poll(() => Boolean(epoch(repoId))).toBe(true);
      const content = await call(page, "read_bytes", { repo_id: repoId, path: "cold.safetensors", max_bytes: source.cold_payloads[index].size + 1 });
      expect(sha(Buffer.from(String(content.bytes_b64), "base64"))).toBe(source.cold_payloads[index].sha256);
      expect(readFileSync(path.join(directories[index], "cold.safetensors"))).toEqual(git(directories[index], ["show", ":cold.safetensors"]));
      await showFile(page, "cold.safetensors", source.cold_payloads[index].marker);
    }
    const afterCold = await Promise.all([stats(0), stats(1)]);
    for (const index of [0, 1]) { expect(afterCold[index].get).toBeGreaterThan(beforeCold[index].get); expect(afterCold[index].writes).toBe(0); }
    await page.screenshot({ path: test.info().outputPath("ssh-storage-cold-restart.png") });
    writeFileSync(test.info().outputPath("ssh-storage-identity-proof.json"), JSON.stringify({ events, beforeCold, afterCold, requestIdentities: remote.requests, nativeCloudReadsWithInvalidAmbientAuth: true, exactHydrationAndIndexPreservation: true, staleFileAndMediaRevoked: true, healthySiblingUnaffected: true, removalAndReselection: true, coldActiveAndDeferredBindings: true }, null, 2));
  } finally {
    try { await close(); } finally { await remote.close(); writeFileSync(test.info().outputPath("application-stderr.log"), stderr); rmSync(root, { recursive: true, force: true }); }
  }
});

test("storage settings replace local and SDK reader identities across distinct real stores", async () => {
  const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
  test.skip(!descriptor, "Requires the owned two-store storage identity fixture");
  test.setTimeout(180_000);
  const fixture = JSON.parse(readFileSync(descriptor!, "utf8")) as Fixture;
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-storage-identity-"));
  const env = isolatedEnvironment(root);
  const git = (args: string[]) => execFileSync("git", args, { cwd: fixture.repositories[0], env });
  const pointer = git(["show", ":lfs.safetensors"]);
  for (const file of ["weights.safetensors", "lfs.safetensors"]) {
    // The owned fixture survives retries; restore only the lazy files this
    // smoke materializes, keeping the index and both storage services intact.
    writeFileSync(path.join(fixture.repositories[0], file), git(["show", `:${file}`]));
  }
  const sha = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
  const events: Runtime[] = [];
  const epoch = (repo: string) => events.filter(event => event.repo_id === repo).at(-1)?.native_session_id;
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  async function settings(page: Page) {
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    return page.getByRole("region", { name: "Storage connections" });
  }
  async function fill(page: Page, profile: S3Profile) {
    const section = page.getByRole("region", { name: "Storage connections" });
    await section.getByLabel("Connection name", { exact: true }).fill(profile.name);
    await section.getByLabel("Bucket", { exact: true }).fill(profile.bucket);
    await section.getByLabel("Signing region", { exact: true }).fill(profile.region);
    await section.getByLabel("Endpoint URL (optional)", { exact: true }).fill(profile.endpoint);
    await section.getByLabel("Access key ID", { exact: true }).fill(profile.credentials.access_key_id);
    await section.getByLabel("Secret access key", { exact: true }).fill(profile.credentials.secret_access_key);
  }
  async function show(page: Page, handle: Record<string, unknown>, source: string) {
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source });
  }
  async function files(page: Page, file: string) {
    const row = page.locator(`[role="treeitem"][data-item-path="${file}"]`);
    if (!await row.isVisible()) {
      const navigation = page.getByRole("navigation", { name: "Primary", exact: true });
      await navigation.getByRole("button", { name: "Settings", exact: true }).click();
      await navigation.getByRole("button", { name: "Files", exact: true }).click();
    }
    await row.click();
  }
  const stats = async (profile: S3Profile) => await (await fetch(profile.endpoint + "/__fixture_stats")).json() as { get: number; head: number; writes: number };
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((value): value is [string, string] => typeof value[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    await page.exposeFunction("recordIdentityRuntime", (event: Runtime) => events.push(event));
    await page.evaluate(() => {
      const target = window as typeof window & { recordIdentityRuntime(value: unknown): Promise<void> };
      window.crab.onEvent((event: AgentEvent) => { if (event.method === "repo:capabilities-changed") void target.recordIdentityRuntime(event.params); });
    });
    await show(page, await call(page, "open_local", { repo_id: "local", path: fixture.repositories[0] }), fixture.repositories[0]);
    let section = await settings(page);
    for (const profile of fixture.profiles) {
      await section.getByRole("button", { name: "Add connection", exact: true }).click();
      await fill(page, profile);
      await section.getByRole("button", { name: "Test connection", exact: true }).click();
      await expect(section.getByRole("status").filter({ hasText: "Bucket read/list access verified" })).toBeVisible();
      await section.getByRole("button", { name: "Save connection", exact: true }).click();
      await expect(section.getByRole("button", { name: `Edit ${profile.name}`, exact: true })).toBeVisible();
    }
    const row = (name: string) => section.getByRole("button", { name: `Edit ${name}`, exact: true }).locator("..");
    await row("Alpha").getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(section.getByRole("status")).toContainText("Connection selected for this repository");
    const saved = await call(page, "storage_connections_list", { repo_id: "local" });
    const alpha = String(saved.selected_id);
    const beta = (saved.connections as Array<{ id: string; name: string }>).find(value => value.name === "Beta")!.id;
    const beforeLocal = await stats(fixture.profiles[0]);
    await files(page, "lfs.safetensors");
    await expect(page.getByText(fixture.payloads[0].marker, { exact: false }).first()).toBeVisible();
    await page.getByRole("button", { name: "File details", exact: true }).click();
    await page.getByRole("button", { name: "Hydrate to disk", exact: true }).click();
    await expect(page.getByText(/materialized on disk/)).toBeVisible();
    expect(sha(readFileSync(path.join(fixture.repositories[0], "lfs.safetensors")))).toBe(fixture.payloads[0].sha256);
    expect(git(["show", ":lfs.safetensors"])).toEqual(pointer);
    expect((await stats(fixture.profiles[0])).get).toBeGreaterThan(beforeLocal.get);
    const localSession = epoch("local");
    const localReader = await call(page, "file_open", { repo_id: "local", path: "weights.safetensors", native_session_id: localSession });
    const remote = await call(page, "open_url", { repo_id: "snapshot", url: fixture.url, storage_connection_id: alpha });
    await show(page, remote, fixture.url);
    await files(page, "weights.safetensors");
    await expect(page.getByText(fixture.payloads[0].marker, { exact: false }).first()).toBeVisible();
    await queryColdParquet(page, fixture.profiles[0], path.join(root, "cache"), 3);
    await page.screenshot({ path: test.info().outputPath("sdk-cold-parquet-sql.png") });
    const remoteSession = String(remote.native_session_id);
    const retained = await call(page, "file_open", { repo_id: "snapshot", path: "weights.safetensors", native_session_id: remoteSession });
    const media = await call(page, "media_open", { repo_id: "snapshot", path: "sample.wav", native_session_id: remoteSession });
    expect(await page.evaluate(async url => { const response = await fetch(url); await response.arrayBuffer(); return response.status; }, String(media.url))).toBe(200);
    expect(await page.evaluate(() => window.crab.getCapabilities("snapshot"))).toMatchObject({ canEdit: false, canMaterialize: false });
    section = await settings(page);
    await section.getByRole("button", { name: "Edit Alpha", exact: true }).click();
    await expect(section.getByLabel("Secret access key", { exact: true })).toHaveValue("");
    await fill(page, { ...fixture.profiles[0], bucket: "other-" + fixture.profiles[0].bucket });
    await section.getByRole("button", { name: "Save changes", exact: true }).click();
    await expect(section.getByRole("alert")).toBeVisible();
    await expect.poll(() => epoch("snapshot")).toBeNull();
    await expect.poll(() => page.evaluate(async url => (await fetch(url)).status, String(media.url))).toBe(410);
    for (const [repo_id, reader, native_session_id] of [["local", localReader, localSession], ["snapshot", retained, remoteSession]] as const) {
      const error = await page.evaluate(async params => { try { await window.crab.rpc("read_range", params); return null; } catch (error) { return error as { code?: string }; } }, { repo_id, file_id: reader.file_id, native_session_id, offset: 0, length: 1 });
      expect(error).toMatchObject({ code: "NATIVE_SESSION_LOST" });
    }
    await section.getByRole("button", { name: "Edit Alpha", exact: true }).click();
    await fill(page, { ...fixture.profiles[1], name: "Alpha" });
    await section.getByRole("button", { name: "Save changes", exact: true }).click();
    await expect(section.getByRole("button", { name: "Save changes", exact: true })).toHaveCount(0);
    await files(page, "weights.safetensors");
    await expect(page.getByText(fixture.payloads[1].marker, { exact: false }).first()).toBeVisible();
    const replacement = epoch("snapshot");
    expect(replacement).not.toBe(remoteSession);
    await rerunReboundParquet(page, 4);
    await page.screenshot({ path: test.info().outputPath("sdk-sql-replaced-source.png") });
    await files(page, "archive.sqlite");
    await expect(page.getByRole("cell", { name: "82", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "SQL", exact: true }).click();
    await expect(page.getByRole("cell", { name: "82", exact: true })).toBeVisible();
    await page.screenshot({ path: test.info().outputPath("sdk-sqlite-wal.png") });
    const content = await call(page, "read_bytes", { repo_id: "snapshot", path: "weights.safetensors", max_bytes: fixture.payloads[1].size + 1 });
    expect(sha(Buffer.from(String(content.bytes_b64), "base64"))).toBe(fixture.payloads[1].sha256);
    await page.screenshot({ path: test.info().outputPath("storage-identity-replaced.png") });
    section = await settings(page);
    await section.getByRole("button", { name: "Forget Alpha", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Alpha", exact: true })).toHaveCount(0);
    await expect.poll(() => epoch("snapshot")).toBeNull();
    await row("Beta").getByRole("button", { name: "Use connection", exact: true }).click();
    await expect(section.getByRole("status")).toContainText("Connection selected for this repository");
    expect((await call(page, "storage_connections_list", { repo_id: "snapshot" })).selected_id).toBe(beta);
    await files(page, "weights.safetensors");
    await expect(page.getByText(fixture.payloads[1].marker, { exact: false }).first()).toBeVisible();
    const transports = await Promise.all(fixture.profiles.map(stats));
    expect(transports.map(value => value.writes)).toEqual([0, 0]);
    expect(transports.every(value => value.get > 0)).toBe(true);
    writeFileSync(test.info().outputPath("storage-identity-proof.json"), JSON.stringify({ localHydratedExactBytes: true, indexPointerUnchanged: true, failedSelectionRevokedLocalAndSdkReaders: true, oldMediaRevoked: true, replacementReadExactOtherStoreBytes: true, removalAndSelectionRecovered: true, snapshotMutationsUnavailable: true, events, transports }, null, 2));
  } finally {
    await application?.close();
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});

test("saved local storage identity survives a cold app restart without ambient credentials", async () => {
  const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
  test.skip(!descriptor, "Requires the owned two-store storage identity fixture");
  test.setTimeout(120_000);
  const fixture = JSON.parse(readFileSync(descriptor!, "utf8")) as Fixture;
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-cold-identity-"));
  const profile = path.join(root, "profile");
  const env = isolatedEnvironment(root);
  const pointer = execFileSync("git", ["show", ":cold.safetensors"], { cwd: fixture.repositories[0], env });
  writeFileSync(path.join(fixture.repositories[0], "cold.safetensors"), pointer);
  const launch = () => electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
  const stats = async () => await (await fetch(fixture.profiles[0].endpoint + "/__fixture_stats")).json() as { get: number; head: number; writes: number };
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  const recordErrors = (app: Awaited<ReturnType<typeof electron.launch>>) => app.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
  try {
    application = await launch();
    recordErrors(application);
    let page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const connection = await call(page, "storage_connections_save", { connection: fixture.profiles[0] });
    await call(page, "open_local", { repo_id: "before-restart", path: fixture.repositories[0], storage_connection_id: connection.id });
    // Do not browse the object before restart: neither a warm vault nor a
    // verified content cache may satisfy the cold native capture below.
    await application.close();
    application = undefined;
    const encrypted = readFileSync(path.join(profile, "storage-connections.enc"));
    expect(encrypted.includes(Buffer.from(fixture.profiles[0].credentials.secret_access_key))).toBe(false);
    const before = await stats();
    application = await launch();
    recordErrors(application);
    page = await application.firstWindow();
    const handle = await call(page, "open_local", { repo_id: "after-restart", path: fixture.repositories[0] });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source: fixture.repositories[0] });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="cold.safetensors"]').click();
    await expect(page.getByText(fixture.cold_payloads[0].marker, { exact: false }).first()).toBeVisible();
    const content = await call(page, "read_bytes", { repo_id: "after-restart", path: "cold.safetensors", max_bytes: fixture.cold_payloads[0].size + 1 });
    const digest = createHash("sha256").update(Buffer.from(String(content.bytes_b64), "base64")).digest("hex");
    expect(digest).toBe(fixture.cold_payloads[0].sha256);
    expect(readFileSync(path.join(fixture.repositories[0], "cold.safetensors"))).toEqual(pointer);
    const after = await stats();
    expect(after.get).toBeGreaterThan(before.get);
    expect(after.writes).toBe(0);
    await page.screenshot({ path: test.info().outputPath("cold-storage-identity.png") });
    writeFileSync(test.info().outputPath("cold-storage-identity-proof.json"), JSON.stringify({ encryptedSecretAbsentFromPlaintext: true, restartedLocalBindingUsedWithoutExplicitSelection: true, uncachedLfsExactBytes: true, workingPointerUnchanged: true, before, after }, null, 2));
  } finally {
    await application?.close();
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});

test("GUI snapshots at the same URL restore separate saved identities and cold content after restart", async () => {
  const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
  test.skip(!descriptor, "Requires the owned two-store storage identity fixture");
  test.setTimeout(150_000);
  const fixture = JSON.parse(readFileSync(descriptor!, "utf8")) as Fixture;
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-saved-snapshots-"));
  const env = isolatedEnvironment(root);
  const profile = path.join(root, "profile");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  async function launch() {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    return application.firstWindow();
  }
  const recent = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem("crab.recentRepos.v1") ?? "[]") as Array<{ repo_id: string; path: string; label: string }>);
  const stats = async () => Promise.all(fixture.profiles.map(async connection => await (await fetch(connection.endpoint + "/__fixture_stats")).json() as { get: number; head: number; writes: number }));
  async function view(page: Page, file: string, marker: string) {
    const row = page.locator(`[role="treeitem"][data-item-path="${file}"]`);
    if (!await row.isVisible()) {
      const nav = page.getByRole("navigation", { name: "Primary", exact: true });
      await nav.getByRole("button", { name: "Settings", exact: true }).click();
      await nav.getByRole("button", { name: "Files", exact: true }).click();
    }
    await row.click();
    await expect(page.getByText(marker, { exact: false }).first()).toBeVisible();
  }
  try {
    let page = await launch();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const selected: string[] = [];
    for (const [index, connection] of fixture.profiles.entries()) {
      selected.push(String((await call(page, "storage_connections_save", { connection })).id));
      if (index === 0) {
        await fetch(connection.endpoint + "/__fixture_pause");
        await page.getByRole("button", { name: "More actions", exact: true }).click();
        await page.getByRole("button", { name: "Open storage snapshot", exact: true }).click();
        const pending = page.getByRole("dialog", { name: "Open storage snapshot", exact: true });
        await pending.getByLabel("Repository URL", { exact: true }).fill(fixture.url);
        await pending.getByRole("button", { name: "Edit Alpha", exact: true }).locator("..").getByRole("button", { name: "Use connection", exact: true }).click();
        await pending.getByRole("button", { name: "Open snapshot", exact: true }).click();
        await expect.poll(async () => (await (await fetch(connection.endpoint + "/__fixture_stats")).json() as { blocked: number }).blocked).toBeGreaterThan(0);
        await pending.getByRole("button", { name: "Cancel", exact: true }).click();
        await expect(pending).toHaveCount(0, { timeout: 10_000 });
        expect((await (await fetch(connection.endpoint + "/__fixture_stats")).json() as { paused: boolean }).paused).toBe(true);
        expect(await recent(page)).toHaveLength(0);
        await fetch(connection.endpoint + "/__fixture_release");
      }
      await page.getByRole("button", { name: "More actions", exact: true }).click();
      await page.getByRole("button", { name: "Open storage snapshot", exact: true }).click();
      const dialog = page.getByRole("dialog", { name: "Open storage snapshot", exact: true });
      await dialog.getByLabel("Repository URL", { exact: true }).fill(fixture.url);
      await dialog.getByLabel("Name (optional)", { exact: true }).fill(connection.name + " snapshot");
      await dialog.getByRole("button", { name: `Edit ${connection.name}`, exact: true }).locator("..").getByRole("button", { name: "Use connection", exact: true }).click();
      await dialog.getByRole("button", { name: "Open snapshot", exact: true }).click();
      await expect(dialog).toHaveCount(0);
      await view(page, "weights.safetensors", fixture.payloads[index].marker);
      await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
      await expect(page.getByRole("tab", { name: "Workspace", exact: true })).toHaveAttribute("aria-disabled", "true");
      await page.getByRole("button", { name: /^Task list:/ }).click();
      const tasks = page.getByRole("dialog", { name: "Task list", exact: true });
      await expect(tasks.getByRole("textbox", { name: "New task title", exact: true })).toBeDisabled();
      await expect(tasks.getByRole("button", { name: "Add task", exact: true })).toBeDisabled();
      await tasks.getByRole("button", { name: "Close task list", exact: true }).click();
      await page.getByRole("group", { name: "Theme", exact: true }).getByRole("combobox").selectOption("light");
      await expect(page.locator("html")).toHaveClass(/theme-light/);
      await view(page, "weights.safetensors", fixture.payloads[index].marker);
    }
    await expect.poll(async () => (await recent(page)).length).toBe(2);
    const bookmarks = await recent(page);
    const ids = fixture.profiles.map(connection => bookmarks.find(bookmark => bookmark.label === connection.name + " snapshot")!.repo_id);
    expect(new Set(ids).size).toBe(2);
    expect(bookmarks.every(bookmark => bookmark.path === fixture.url)).toBe(true);
    await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(ids[1]);
    const beforeRestart = await stats();
    await application!.close(); application = undefined;
    page = await launch();
    // The selected SDK snapshot reopens automatically; its sibling remains
    // deferred until the sidebar action admits that logical identity.
    await expect(page.getByRole("button", { name: "Beta snapshot", exact: true })).toBeVisible();
    await view(page, "cold.safetensors", fixture.cold_payloads[1].marker);
    for (const index of [1, 0]) {
      if (index === 0) {
        await page.getByRole("button", { name: "Alpha snapshot", exact: true }).click();
        await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(ids[0]);
        await view(page, "cold.safetensors", fixture.cold_payloads[0].marker);
      }
      await expect.poll(() => page.evaluate(() => localStorage.getItem("crab.activeRepoPath.v1"))).toBe(ids[index]);
      expect((await call(page, "storage_connections_list", { repo_id: ids[index] })).selected_id).toBe(selected[index]);
      const payload = fixture.cold_payloads[index];
      const content = await call(page, "read_bytes", { repo_id: ids[index], path: "cold.safetensors", max_bytes: payload.size + 1 });
      expect(createHash("sha256").update(Buffer.from(String(content.bytes_b64), "base64")).digest("hex")).toBe(payload.sha256);
      expect(await page.evaluate(repo => window.crab.getCapabilities(repo), ids[index])).toMatchObject({ canEdit: false, canMaterialize: false });
    }
    const afterRestart = await stats();
    for (const index of [0, 1]) {
      expect(afterRestart[index].get).toBeGreaterThan(beforeRestart[index].get);
      expect(afterRestart[index].writes).toBe(0);
    }
    expect((await recent(page)).map(bookmark => bookmark.repo_id).sort()).toEqual([...ids].sort());
    await page.screenshot({ path: test.info().outputPath("saved-sdk-snapshots.png") });
    writeFileSync(test.info().outputPath("saved-sdk-snapshots-proof.json"), JSON.stringify({ blockedSdkCancelledBeforeStorageRelease: true, sameUrlDistinctIdentities: true, guiOpenedBothStores: true, activeSdkRestored: true, deferredSdkRestored: true, coldBytesExact: true, readOnlyCapabilities: true, workspaceSettingsAndTasksDisabled: true, userThemeStillWorks: true, ids, beforeRestart, afterRestart }, null, 2));
  } finally {
    try { await fetch(fixture.profiles[0].endpoint + "/__fixture_release"); }
    finally { await application?.close(); }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
