import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { chmodSync, lstatSync, mkdtempSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { call, packagedExecutable } from "./packaged-app";

for (const fault of ["missing agent", "missing CLI", "denied execution"] as const) test(`device storage settings survive ${fault} and native repair`, async () => {
  test.skip(process.platform === "linux", "Secure-vault GUI qualification needs a configured OS keyring");
  test.skip(fault === "denied execution" && process.platform === "win32", "POSIX execute permissions do not apply on Windows");
  test.setTimeout(120_000);
  const executable = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executable), "../Resources") : path.join(path.dirname(executable), "resources");
  const suffix = process.platform === "win32" ? ".exe" : "";
  const binary = path.join(resources, "agent", (fault === "missing CLI" ? "crab" : "crab-desktop-agent") + suffix);
  const held = binary + ".settings-fixture-held";
  const mode = lstatSync(binary).mode & 0o777;
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-settings-offline-"));
  const profile = path.join(root, "profile");
  const repo = path.join(root, "repository");
  mkdirSync(repo);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(root, "gitconfig") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo, env });
  const marker = "RESTORED_NATIVE_SETTINGS_SOURCE";
  const header = Buffer.from(JSON.stringify({ [marker]: { dtype: "U8", shape: [16], data_offsets: [0, 16] } }));
  const size = Buffer.alloc(8); size.writeBigUInt64LE(BigInt(header.length));
  writeFileSync(path.join(repo, "weights.safetensors"), Buffer.concat([size, header, Buffer.alloc(16, 0x41)]));
  const secret = "fixture-private-" + randomUUID();
  const access = "fixture-access-" + randomUUID();
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  let damaged = false;
  function repair() {
    if (!damaged) return;
    if (fault === "denied execution") chmodSync(binary, mode);
    else renameSync(held, binary);
    damaged = false;
  }
  async function launch() {
    application = await electron.launch({ executablePath: executable, args: [`--user-data-dir=${profile}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    return application.firstWindow();
  }
  async function settings(page: Page, keyboard = false) {
    // firstWindow resolves before React installs the global shortcut listener.
    await expect(page.getByRole("button", { name: "More actions", exact: true })).toBeVisible();
    if (keyboard) await page.keyboard.press(process.platform === "darwin" ? "Meta+," : "Control+,");
    else {
      await page.getByRole("button", { name: "More actions", exact: true }).click();
      await page.getByRole("button", { name: "Settings", exact: true }).click();
    }
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    return page.getByRole("region", { name: "Storage connections", exact: true });
  }
  async function fill(page: Page, name: string) {
    const section = page.getByRole("region", { name: "Storage connections", exact: true });
    await section.getByLabel("Connection name", { exact: true }).fill(name);
    await section.getByLabel("Bucket", { exact: true }).fill("settings-fixture");
    await section.getByLabel("Signing region", { exact: true }).fill("us-east-1");
    await section.getByLabel("Endpoint URL (optional)", { exact: true }).fill("http://127.0.0.1:1");
    await section.getByLabel("Access key ID", { exact: true }).fill(access);
    await section.getByLabel("Secret access key", { exact: true }).fill(secret);
  }
  try {
    if (fault === "denied execution") chmodSync(binary, 0);
    else renameSync(binary, held);
    damaged = true;
    let page = await launch();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    let section = await settings(page);
    await section.getByRole("button", { name: "Add connection", exact: true }).click();
    await fill(page, "Offline store");
    await section.getByRole("button", { name: "Save connection", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Offline store", exact: true })).toBeVisible();
    const encrypted = readFileSync(path.join(profile, "storage-connections.enc"));
    expect(encrypted.includes(Buffer.from(secret))).toBe(false);
    await section.getByRole("button", { name: "Edit Offline store", exact: true }).click();
    await expect(section.getByLabel("Secret access key", { exact: true })).toHaveValue("");
    await fill(page, "Saved while unavailable");
    await section.getByRole("button", { name: "Save changes", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Saved while unavailable", exact: true })).toBeVisible();
    await section.getByRole("button", { name: "Test Saved while unavailable", exact: true }).click();
    await expect(section.getByRole("alert")).toContainText(fault === "denied execution" ? /EACCES|denied/i : "Missing Crab v2 runtime");
    await section.getByRole("button", { name: "Add connection", exact: true }).click();
    await fill(page, "Disposable store");
    await section.getByRole("button", { name: "Save connection", exact: true }).click();
    await section.getByRole("button", { name: "Forget Disposable store", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Disposable store", exact: true })).toHaveCount(0);
    await application!.close(); application = undefined;
    page = await launch();
    section = await settings(page, true);
    await expect(section.getByRole("button", { name: "Edit Saved while unavailable", exact: true })).toBeVisible();
    const saved = await call(page, "storage_connections_list", {});
    expect(JSON.stringify(saved).includes(secret)).toBe(false);
    expect(saved.connections).toEqual([expect.objectContaining({ name: "Saved while unavailable" })]);
    await page.screenshot({ path: test.info().outputPath("device-settings-without-native.png") });
    repair();
    // A user selects a directory through the OS picker. Keep that interaction
    // deterministic while opening a real repository through the production IPC.
    await application!.evaluate(({ dialog }, directory) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [directory] }); }, repo);
    await page.getByRole("button", { name: "Open Repository", exact: true }).click();
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="weights.safetensors"]').click();
    await expect(page.getByText(marker, { exact: false }).first()).toBeVisible();
    await expect(page.getByText("Desktop services unavailable", { exact: true })).toHaveCount(0);
    await page.screenshot({ path: test.info().outputPath("native-repaired-preview.png") });
    await settings(page, true);
    await expect(section.getByRole("button", { name: "Edit Saved while unavailable", exact: true })).toBeVisible();
    writeFileSync(test.info().outputPath("native-independent-settings-proof.json"), JSON.stringify({ fault, noRepositoryRequired: true, menuAndShortcutAccess: true, encryptedSaveEditRemove: true, nativeTestFailedVisibly: true, coldMetadataRestored: true, nativeRepairOpenedRealPreview: true }, null, 2));
  } finally {
    try { await application?.close(); }
    finally { repair(); rmSync(root, { recursive: true, force: true }); }
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
  }
});
