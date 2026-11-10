import { test, expect, _electron as electron } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { open, type FileHandle } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type {} from "../../src/ipc";
import { call, packagedExecutable } from "./packaged-app";

for (const terminal of ["cancel", "application close"] as const) test(`pending native repository open drains its Git child on ${terminal}`, async () => {
  test.skip(process.platform === "win32", "The blocked Git fixture uses a POSIX FIFO");
  test.setTimeout(60_000);
  const root = mkdtempSync(path.join(process.env.CARGO_TARGET_DIR ?? tmpdir(), "desktop-pending-open-"));
  const blocked = path.join(root, "blocked"), healthy = path.join(root, "healthy");
  const marker = "A healthy repository remains usable after cancelling a blocked open.\n";
  writeFileSync(path.join(root, "gitconfig"), "");
  const env: NodeJS.ProcessEnv = { ...process.env, GIT_CONFIG_GLOBAL: path.join(root, "gitconfig"), GIT_CONFIG_NOSYSTEM: "1", CRAB_CACHE_DIR: path.join(root, "cache") };
  delete env.GIT_DIR; delete env.GIT_WORK_TREE; delete env.GIT_INDEX_FILE;
  for (const directory of [blocked, healthy]) {
    mkdirSync(directory);
    execFileSync("git", ["init", "--initial-branch=main"], { cwd: directory, env });
    writeFileSync(path.join(directory, "recovered.txt"), marker);
    execFileSync("git", ["add", "recovered.txt"], { cwd: directory, env });
    execFileSync("git", ["-c", "user.name=Desktop test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Pending open fixture"], { cwd: directory, env });
  }
  const config = path.join(blocked, ".git/config");
  renameSync(config, `${config}.saved`);
  execFileSync("mkfifo", [config]);
  let application: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let writer: FileHandle | undefined;
  try {
    application = await electron.launch({ executablePath: packagedExecutable(), args: [`--user-data-dir=${path.join(root, "profile")}`], env: Object.fromEntries(Object.entries(env).filter((value): value is [string, string] => typeof value[1] === "string")) });
    const page = await application.firstWindow();
    await page.getByRole("button", { name: "Skip", exact: true }).click();
    await page.getByRole("button", { name: "No thanks", exact: true }).click();
    let outcome: { code?: number; value?: unknown } | undefined;
    await page.exposeFunction("pendingOpenOutcome", (value: typeof outcome) => { outcome = value; });
    writer = await open(config, "r+");
    await page.evaluate(directory => {
      const target = window as typeof window & { pendingOpenOutcome(value: unknown): Promise<void> };
      void window.crab!.rpc("open_local", { repo_id: "pending", path: directory, op_id: "blocked-open" }).then(
        value => target.pendingOpenOutcome({ value }),
        error => target.pendingOpenOutcome({ code: (error as { code?: number }).code }),
      );
    }, blocked);
    // Keep the FIFO open until terminal cleanup. A real Git config read must
    // block here; releasing the fixture cannot be what ends the child.
    let childPid: number | undefined;
    await expect.poll(() => {
      const rows = execFileSync("ps", ["-axo", "pid=,ppid=,command="], { encoding: "utf8" }).split("\n").map(line => /^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line));
      const native = rows.find(row => row && Number(row[2]) === application!.process().pid && row[3].includes("/agent/crab-desktop-agent "));
      const child = rows.find(row => row && native && row[2] === native[1] && row[3].includes("git --literal-pathspecs rev-parse"));
      childPid = child ? Number(child[1]) : undefined;
      return childPid;
    }).toBeDefined();
    expect(outcome).toBeUndefined();
    if (childPid === undefined) throw new Error("Blocked repository open did not own a Git child");
    const pid = childPid;
    const alive = () => { try { process.kill(pid, 0); return true; } catch (error) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; } };
    if (terminal === "cancel") {
      const sibling = await call(page, "open_local", { repo_id: "sibling", path: healthy });
      expect(sibling.repo_id).toBe("sibling");
      await call(page, "rpc/cancel", { repo_id: "pending", op_id: "blocked-open" });
      await expect.poll(() => outcome).toMatchObject({ code: 1010 });
      await expect.poll(alive).toBe(false);
      const handle = await call(page, "open_local", { repo_id: "pending", path: healthy });
      await page.evaluate(({ handle, directory }) => window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: directory } })), { handle, directory: healthy });
      await page.getByRole("navigation", { name: "Primary", exact: true }).getByRole("button", { name: "Files", exact: true }).click();
      await page.locator('[role="treeitem"][data-item-path="recovered.txt"]').click();
      await expect(page.locator(".monaco-editor").first()).toContainText(marker.trim());
      expect(readFileSync(path.join(healthy, "recovered.txt"), "utf8")).toBe(marker);
      await page.screenshot({ path: test.info().outputPath("repository-open-recovered.png") });
    } else {
      await application.close(); application = undefined;
      await expect.poll(alive).toBe(false);
    }
    writeFileSync(test.info().outputPath("repository-open-proof.json"), JSON.stringify({ terminal, realGitReaderBlocked: true, childReapedBeforeFixtureRelease: true, healthySiblingAndVisibleRetry: terminal === "cancel" }, null, 2));
  } finally {
    // Release the FIFO before app cleanup even when an assertion fails.
    await writer?.close();
    await application?.close();
    rmSync(root, { recursive: true, force: true });
  }
});
