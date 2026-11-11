import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync, renameSync, existsSync } from "node:fs";
import { open, type FileHandle } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { RepoHandle } from "../../src/ipc";
import { packagedExecutable, prepareFixtureSsh } from "./packaged-app";
import { sshFixture } from "./ssh-fixture";

type ReplayWindow = Window & { replayResults: Record<string, { status: "pending" | "resolved" | "rejected"; code?: number }> };

test("queued SSH metadata and Changes recover independently of a blocked sibling", async () => {
  test.skip(process.platform === "win32", "The remote-host fixture executes POSIX native binaries");
  test.setTimeout(150_000);
  const executablePath = packagedExecutable();
  const resources = process.platform === "darwin" ? path.resolve(path.dirname(executablePath), "../Resources") : path.join(path.dirname(executablePath), "resources");
  const crab = path.join(resources, "agent/crab");
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-ssh-metadata-"));
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, CRAB_BIN: crab, CRAB_CACHE_DIR: path.join(root, "cache"), GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1" };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  const fixture = await sshFixture(root, env);
  const healthy = path.join(fixture.home, "healthy-project"), blocked = path.join(fixture.home, "blocked-project");
  const config = path.join(blocked, ".git/config");
  const git = (cwd: string, args: string[]) => execFileSync("git", args, { cwd, env });
  for (const directory of [healthy, blocked]) {
    mkdirSync(directory); git(directory, ["init", "--initial-branch=main"]);
    writeFileSync(path.join(directory, "seed.txt"), "Reconnect fixture\n");
    git(directory, ["add", "seed.txt"]);
    git(directory, ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Metadata recovery fixture"]);
  }
  const marker = "HEALTHY_RECONNECTED_TENSOR", size = 4 * 1024 ** 2;
  const header = Buffer.from(JSON.stringify({ [marker]: { dtype: "U8", shape: [size], data_offsets: [0, size] } }));
  const prefix = Buffer.alloc(8); prefix.writeBigUInt64LE(BigInt(header.length));
  const content = Buffer.concat([prefix, header, Buffer.alloc(size, 0x42)]);
  writeFileSync(path.join(healthy, "weights.safetensors"), content);
  git(healthy, ["add", "weights.safetensors"]);
  git(healthy, ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Large preview fixture"]);
  const index = readFileSync(path.join(healthy, ".git/index"));
  writeFileSync(path.join(healthy, "seed.txt"), "A pending edit before reconnect\n");
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let blockedConfig: FileHandle | undefined;
  let stderr = "";
  async function release() {
    // Restore the path before closing the writer: EOF can start the next Git
    // child before the async close callback returns, stranding it on the FIFO.
    if (existsSync(`${config}.saved`)) renameSync(`${config}.saved`, config);
    await blockedConfig?.close(); blockedConfig = undefined;
  }
  try {
    application = await electron.launch({ executablePath, env: Object.fromEntries(Object.entries(env).filter((entry): entry is [string, string] => typeof entry[1] === "string")), args: [`--user-data-dir=${path.join(root, "profile")}`] });
    application.process().stderr?.on("data", bytes => { stderr = (stderr + bytes.toString()).slice(-48_000); });
    const page = await application.firstWindow();
    page.on("pageerror", error => { stderr += `\nRenderer error: ${error.stack}`; });
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    const label = "Independent metadata recovery";
    const saved = await page.evaluate(value => window.crab!.ssh.saveProfile(value), { label, host: "127.0.0.1", port: fixture.port, username: "fixture", remotePath: healthy, authMethod: { type: "key" as const, keyPath: fixture.keyPath }, autoDeployAgent: true, autoReconnect: true, operationTimeoutMs: 60_000 });
    const healthyId = `ssh://${saved.id}${healthy}`, blockedId = `ssh://${saved.id}${blocked}`;
    await page.getByRole("button", { name: "SSH Connections", exact: true }).click();
    await prepareFixtureSsh(application!, fixture);
    await page.getByRole("button", { name: `Connect to ${label}`, exact: true }).click();
    await page.getByRole("button", { name: `Browse files on ${label}`, exact: true }).click();
    await page.getByRole("button", { name: "Open Repo", exact: true }).click();
    await expect(page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "healthy-project", exact: true })).toBeVisible({ timeout: 60_000 });
    const other: RepoHandle = await page.evaluate(({ id, directory }) => window.crab!.ssh.openAsRepo(id, directory), { id: saved.id, directory: blocked });
    await page.evaluate(({ handle, path }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path } })), { handle: other, path: blockedId });
    await page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "healthy-project", exact: true }).click();
    const navigation = page.getByRole("navigation", { name: "Primary", exact: true });
    await navigation.getByRole("button", { name: "Git", exact: true }).click();
    await expect(page.getByRole("textbox", { name: "Commit subject", exact: true })).toBeVisible();
    await page.getByRole("textbox", { name: "Commit subject", exact: true }).fill("Preserve my reconnect draft");

    fixture.setAccepting(false); fixture.dropConnections();
    await expect.poll(() => page.evaluate(id => window.crab!.ssh.getStatus(id), saved.id)).toBe("reconnecting");
    await expect.poll(() => fixture.commands.size).toBe(0);
    renameSync(config, `${config}.saved`); execFileSync("mkfifo", [config]); blockedConfig = await open(config, "r+");
    writeFileSync(path.join(healthy, "after-reconnect.txt"), "Visible after independent metadata replay\n");
    await page.evaluate(({ healthyId, blockedId }) => {
      const target = window as unknown as ReplayWindow;
      target.replayResults = {};
      for (const [op_id, repo_id] of [["queued-slow", blockedId], ["cancelled-unsent", blockedId], ["queued-fast", healthyId]]) {
        target.replayResults[op_id] = { status: "pending" };
        void window.crab!.rpc("repo_summary", { repo_id, op_id }).then(
          () => { target.replayResults[op_id] = { status: "resolved" }; },
          (error: { code?: number }) => { target.replayResults[op_id] = { status: "rejected", code: error.code }; },
        );
      }
    }, { healthyId, blockedId });
    fixture.setAccepting(true);
    await expect.poll(() => page.evaluate(() => (window as unknown as ReplayWindow).replayResults["queued-fast"].status), { timeout: 30_000 }).toBe("resolved");
    expect(await page.evaluate(() => (window as unknown as ReplayWindow).replayResults["queued-slow"].status)).toBe("pending");
    expect(fixture.requests.some(request => request.op_id === "queued-fast")).toBe(true);
    expect(fixture.requests.some(request => request.op_id === "queued-slow")).toBe(false);
    await expect(page.getByText("after-reconnect.txt", { exact: true }).and(page.locator(":visible")).first()).toBeVisible();
    await expect(page.getByRole("textbox", { name: "Commit subject", exact: true })).toHaveValue("Preserve my reconnect draft");
    await page.evaluate(repo_id => window.crab!.rpc("rpc/cancel", { repo_id, op_id: "cancelled-unsent" }), blockedId);
    await expect.poll(() => page.evaluate(() => (window as unknown as ReplayWindow).replayResults["cancelled-unsent"])).toEqual({ status: "rejected", code: 1010 });
    expect(fixture.requests.some(request => request.op_id === "cancelled-unsent")).toBe(false);
    await page.screenshot({ path: test.info().outputPath("ssh-changes-recovered-while-sibling-blocked.png") });
    await navigation.getByRole("button", { name: "Files", exact: true }).click();
    await page.locator('[role="treeitem"][data-item-path="weights.safetensors"]').click();
    await expect(page.getByText(marker, { exact: false }).first()).toBeVisible();
    expect(readFileSync(path.join(healthy, "weights.safetensors"))).toEqual(content);
    expect(readFileSync(path.join(healthy, ".git/index"))).toEqual(index);
    await page.screenshot({ path: test.info().outputPath("ssh-preview-during-blocked-sibling.png") });
    await release();
    await expect.poll(() => page.evaluate(() => (window as unknown as ReplayWindow).replayResults["queued-slow"].status), { timeout: 20_000 }).toBe("resolved");
    await page.getByLabel("Repository list", { exact: true }).getByRole("button", { name: "blocked-project", exact: true }).click();
    await navigation.getByRole("button", { name: "Files", exact: true }).click();
    await expect(page.locator('[role="treeitem"][data-item-path="seed.txt"]')).toBeVisible();
    expect(fixture.requests.filter(request => request.op_id === "queued-fast")).toHaveLength(1);
    expect(fixture.requests.filter(request => request.op_id === "queued-slow")).toHaveLength(1);
    expect(fixture.requests.some(request => request.op_id === "cancelled-unsent")).toBe(false);
    writeFileSync(test.info().outputPath("metadata-replay-proof.json"), JSON.stringify({ healthyQueuedMetadataRecoveredBeforeBlockedOpen: true, changesRecoveredWithoutRemount: true, largePreviewUsableWhileSiblingBlocked: true, cancelledUnsentMetadataNeverDispatched: true, blockedMetadataRecoveredAfterRelease: true, exactContentAndIndexPreserved: true }, null, 2));
  } finally {
    await release();
    if (application) {
      const page = application.windows()[0];
      if (page) writeFileSync(test.info().outputPath("renderer-final-state.txt"), await page.locator("body").innerText().catch(() => "Renderer closed"));
      await application.close();
    }
    await fixture.close();
    writeFileSync(test.info().outputPath("application-stderr.log"), stderr);
    writeFileSync(test.info().outputPath("observed-native-requests.json"), JSON.stringify(fixture.requests, null, 2));
    rmSync(root, { recursive: true, force: true });
  }
});
