import { test, expect, _electron as electron, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir, userInfo } from "node:os";
import * as path from "node:path";
import { isNativeAgentCommand, call, packagedExecutable } from "./packaged-app";

for (const fault of ["blocked shell", "log directory"] as const) test(`device settings survive ${fault} before actor construction and repair live`, async () => {
  test.skip(process.platform !== "darwin" || !userInfo().shell?.endsWith("/zsh"), "Requires macOS secure storage and a real zsh startup gate");
  test.setTimeout(150_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-actor-settings-"));
  const profile = path.join(root, "profile"), shellConfig = path.join(root, "shell"), repository = path.join(root, "repository");
  mkdirSync(shellConfig); mkdirSync(repository);
  const gate = path.join(root, "hold-shell"), marker = path.join(root, "shell-entered");
  const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
  writeFileSync(path.join(shellConfig, ".zshenv"), `printf '%s\\n' "$$" > ${quote(marker)}\nwhile [[ -e ${quote(gate)} ]]; do /bin/sleep 0.05; done\n`);
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, ZDOTDIR: shellConfig, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", CRAB_CACHE_DIR: path.join(root, "cache") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  execFileSync("git", ["init", "--initial-branch=main"], { cwd: repository, env });
  const tensor = "ACTOR_REPAIR_REAL_PREVIEW";
  const header = Buffer.from(JSON.stringify({ [tensor]: { dtype: "U8", shape: [16], data_offsets: [0, 16] } })), prefix = Buffer.alloc(8);
  prefix.writeBigUInt64LE(BigInt(header.length));
  writeFileSync(path.join(repository, "weights.safetensors"), Buffer.concat([prefix, header, Buffer.alloc(16, 0x41)]));
  const secret = `fixture-private-${randomUUID()}`, access = `fixture-access-${randomUUID()}`;
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let stderr = "";
  let launches = 0;
  let admitted = "", cancelled = "";
  const release = () => rmSync(gate, { force: true });
  const nativeChildren = () => execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).filter(row => row && Number(row[2]) === application?.process().pid && isNativeAgentCommand(row[3])).map(row => Number(row![1]));
  async function launch() {
    writeFileSync(gate, "held"); rmSync(marker, { force: true });
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${profile}`], env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")) });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-32_000); });
    const page = await application.firstWindow();
    await expect.poll(() => existsSync(marker)).toBe(true);
    if (fault === "log directory") {
      await application.evaluate(() => {
        const fs = process.getBuiltinModule("fs"), os = process.getBuiltinModule("os"), path = process.getBuiltinModule("path");
        const original = fs.mkdirSync, descriptor = Object.getOwnPropertyDescriptor(fs, "mkdirSync");
        if (!descriptor) throw new Error("Missing filesystem descriptor");
        const target = path.join(os.homedir(), ".crab", "desktop", "logs");
        const state = globalThis as typeof globalThis & { actorLogFault: { hits: number; restore: () => void } };
        state.actorLogFault = { hits: 0, restore: () => { Object.defineProperty(fs, "mkdirSync", descriptor); } };
        // Fail the production mkdir without touching the user's real directory.
        Object.defineProperty(fs, "mkdirSync", { ...descriptor, value: (...args: unknown[]) => {
          if (String(args[0]) === target) { state.actorLogFault.hits++; throw Object.assign(new Error("Fixture log directory denied"), { code: "EACCES" }); }
          return Reflect.apply(original, fs, args);
        } });
      });
      release();
      await expect.poll(() => application!.evaluate(() => (globalThis as typeof globalThis & { actorLogFault: { hits: number } }).actorLogFault.hits)).toBeGreaterThan(0);
    }
    expect(nativeChildren()).toHaveLength(0);
    if (fault === "blocked shell") {
      admitted = path.join(root, `admitted-${++launches}`); cancelled = path.join(root, `cancelled-${launches}`);
      await application.evaluate(async ({ app }, fixture) => {
        const require = process.getBuiltinModule("module").createRequire(app.getAppPath() + "/package.json");
        const { GenericAdapter } = require("./dist-electron/orchestrator-adapters.cjs") as typeof import("../../electron/orchestrator-adapters");
        const source = "require('node:fs').writeFileSync(process.argv[1], 'started')";
        const revoked = new GenericAdapter(fixture.node, "Environment fixture", ["-e", source]);
        const pending = revoked.spawn(fixture.cancelled, fixture.cwd);
        revoked.cancel(); await pending;
        const active = new GenericAdapter(fixture.node, "Environment fixture", ["-e", source]);
        void active.spawn(fixture.admitted, fixture.cwd);
      }, { node: process.execPath, admitted, cancelled, cwd: repository });
      expect(existsSync(admitted)).toBe(false);
      expect(existsSync(cancelled)).toBe(false);
    }
    return page;
  }
  async function settings(page: Page) {
    await expect(page.getByRole("button", { name: "More actions", exact: true })).toBeVisible();
    await page.keyboard.press("Meta+,");
    await page.getByRole("button", { name: "Storage & Cache", exact: true }).click();
    return page.getByRole("region", { name: "Storage connections", exact: true });
  }
  async function fill(page: Page, name: string) {
    const section = page.getByRole("region", { name: "Storage connections", exact: true });
    await section.getByLabel("Connection name", { exact: true }).fill(name);
    await section.getByLabel("Bucket", { exact: true }).fill("actor-fixture");
    await section.getByLabel("Signing region", { exact: true }).fill("us-east-1");
    await section.getByLabel("Endpoint URL (optional)", { exact: true }).fill("http://127.0.0.1:1");
    await section.getByLabel("Access key ID", { exact: true }).fill(access);
    await section.getByLabel("Secret access key", { exact: true }).fill(secret);
  }
  async function releaseAndVerify() {
    release();
    if (fault === "blocked shell" && admitted) {
      await expect.poll(() => existsSync(admitted)).toBe(true);
      expect(existsSync(cancelled)).toBe(false);
    }
  }
  async function close() {
    await releaseAndVerify();
    if (application) {
      if (fault === "blocked shell") await call(application.windows()[0], "ping", {}).catch(() => {});
      await application.close(); application = undefined;
    }
  }
  try {
    let page = await launch();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    let section = await settings(page);
    await section.getByRole("button", { name: "Add connection", exact: true }).click();
    await fill(page, "Before construction");
    await section.getByRole("button", { name: "Save connection", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Before construction", exact: true })).toBeVisible();
    await section.getByRole("button", { name: "Edit Before construction", exact: true }).click();
    await fill(page, "Saved without actor");
    await section.getByRole("button", { name: "Save changes", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Saved without actor", exact: true })).toBeVisible();
    await section.getByRole("button", { name: "Add connection", exact: true }).click();
    await fill(page, "Disposable");
    await section.getByRole("button", { name: "Save connection", exact: true }).click();
    await section.getByRole("button", { name: "Forget Disposable", exact: true }).click();
    await expect(section.getByRole("button", { name: "Edit Disposable", exact: true })).toHaveCount(0);
    expect(nativeChildren()).toHaveLength(0);
    const encrypted = readFileSync(path.join(profile, "storage-connections.enc"));
    expect(encrypted.includes(Buffer.from(secret))).toBe(false);
    await close();
    page = await launch(); section = await settings(page);
    await expect(section.getByRole("button", { name: "Edit Saved without actor", exact: true })).toBeVisible();
    const saved = await call(page, "storage_connections_list", {});
    expect(saved.connections).toEqual([expect.objectContaining({ name: "Saved without actor" })]);
    expect(JSON.stringify(saved).includes(secret)).toBe(false);
    await page.screenshot({ path: test.info().outputPath("settings-before-native-construction.png") });
    if (fault === "log directory") {
      await section.getByRole("button", { name: "Test Saved without actor", exact: true }).click();
      await expect(section.getByRole("alert")).toContainText("Fixture log directory denied");
      await application!.evaluate(() => (globalThis as typeof globalThis & { actorLogFault: { restore: () => void } }).actorLogFault.restore());
    }
    await releaseAndVerify();
    await expect.poll(async () => { try { return (await call(page, "ping", {})).pong; } catch { return false; } }).toBeTruthy();
    expect(nativeChildren()).toHaveLength(1);
    await section.getByRole("button", { name: "Test Saved without actor", exact: true }).click();
    await expect(section.getByRole("alert")).toBeVisible({ timeout: 30_000 });
    await expect(section.getByRole("alert")).not.toContainText("Fixture log directory denied");
    await expect(section.getByRole("alert")).not.toContainText(secret);
    await application!.evaluate(({ dialog }, directory) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [directory] }); }, repository);
    await page.getByRole("button", { name: "Open Repository", exact: true }).click();
    await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="weights.safetensors"]').click();
    await expect(page.getByText(tensor, { exact: false }).first()).toBeVisible();
    await expect(page.getByText("Desktop services unavailable", { exact: true })).toHaveCount(0);
    await page.screenshot({ path: test.info().outputPath("constructor-repaired-preview.png") });
    writeFileSync(test.info().outputPath("actor-settings-proof.json"), JSON.stringify({ fault, metadataBeforeActor: true, encryptedCrud: true, coldRestoreUnderFault: true, liveConstructorRepair: true, oneNativeChild: true, actualConnectionTest: true, realFilePreview: true }, null, 2));
  } finally {
    await close();
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    rmSync(root, { recursive: true, force: true });
  }
});
