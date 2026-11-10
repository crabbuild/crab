import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import * as path from "node:path";
import { tmpdir } from "node:os";
import type { StorageConnectionInput } from "../../electron/storage-connection-types";
import type { AgentEvent } from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";

type S3Profile = StorageConnectionInput & { credentials: { provider: "s3"; access_key_id: string; secret_access_key: string } };
interface Fixture {
  url: string;
  local: string;
  profiles: [S3Profile, S3Profile];
  payloads: [{ marker: string; sha256: string; size: number }, { marker: string; sha256: string; size: number }];
  cold_payloads: [{ marker: string; sha256: string; size: number }, { marker: string; sha256: string; size: number }];
}
interface Runtime { repo_id: string; native_session_id: string | null; error?: string }

test("storage settings replace local and SDK reader identities across distinct real stores", async () => {
  const descriptor = process.env.CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE;
  test.skip(!descriptor, "Requires the owned two-store storage identity fixture");
  test.setTimeout(180_000);
  const fixture = JSON.parse(readFileSync(descriptor!, "utf8")) as Fixture;
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-storage-identity-"));
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(root, "gitconfig") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  writeFileSync(path.join(root, "gitconfig"), "");
  const git = (args: string[]) => execFileSync("git", args, { cwd: fixture.local, env });
  const pointer = git(["show", ":lfs.safetensors"]);
  for (const file of ["weights.safetensors", "lfs.safetensors"]) {
    // The owned fixture survives retries; restore only the lazy files this
    // smoke materializes, keeping the index and both storage services intact.
    writeFileSync(path.join(fixture.local, file), git(["show", `:${file}`]));
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
    await show(page, await call(page, "open_local", { repo_id: "local", path: fixture.local }), fixture.local);
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
    expect(sha(readFileSync(path.join(fixture.local, "lfs.safetensors")))).toBe(fixture.payloads[0].sha256);
    expect(git(["show", ":lfs.safetensors"])).toEqual(pointer);
    expect((await stats(fixture.profiles[0])).get).toBeGreaterThan(beforeLocal.get);
    const localSession = epoch("local");
    const localReader = await call(page, "file_open", { repo_id: "local", path: "weights.safetensors", native_session_id: localSession });
    const remote = await call(page, "open_url", { repo_id: "snapshot", url: fixture.url, storage_connection_id: alpha });
    await show(page, remote, fixture.url);
    await files(page, "weights.safetensors");
    await expect(page.getByText(fixture.payloads[0].marker, { exact: false }).first()).toBeVisible();
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
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), AWS_ACCESS_KEY_ID: "invalid-ambient-access", AWS_SECRET_ACCESS_KEY: "invalid-ambient-secret", AWS_SESSION_TOKEN: "invalid-ambient-session", AWS_ENDPOINT_URL: "http://127.0.0.1:1", AWS_ENDPOINT_URL_S3: "http://127.0.0.1:1", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(root, "gitconfig") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  writeFileSync(path.join(root, "gitconfig"), "");
  const pointer = execFileSync("git", ["show", ":cold.safetensors"], { cwd: fixture.local, env });
  writeFileSync(path.join(fixture.local, "cold.safetensors"), pointer);
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
    await call(page, "open_local", { repo_id: "before-restart", path: fixture.local, storage_connection_id: connection.id });
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
    const handle = await call(page, "open_local", { repo_id: "after-restart", path: fixture.local });
    await page.evaluate(({ handle, source }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: source } })), { handle, source: fixture.local });
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="cold.safetensors"]').click();
    await expect(page.getByText(fixture.cold_payloads[0].marker, { exact: false }).first()).toBeVisible();
    const content = await call(page, "read_bytes", { repo_id: "after-restart", path: "cold.safetensors", max_bytes: fixture.cold_payloads[0].size + 1 });
    const digest = createHash("sha256").update(Buffer.from(String(content.bytes_b64), "base64")).digest("hex");
    expect(digest).toBe(fixture.cold_payloads[0].sha256);
    expect(readFileSync(path.join(fixture.local, "cold.safetensors"))).toEqual(pointer);
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
