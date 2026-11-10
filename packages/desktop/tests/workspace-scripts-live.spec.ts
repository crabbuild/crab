import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { Orchestrator } from "../electron/orchestrator";
import { CheckpointManager } from "../electron/orchestrator-checkpoints";

const liveRoot = process.env.CRAB_AGENT_LIVE_ROOT;
const run = promisify(execFile);
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

describe.skipIf(!liveRoot || process.platform === "win32")("owned workspace scripts", () => {
  let directory: string;
  let orchestrator: Orchestrator;
  let outputs: Set<string>;
  const writer = (output = directory) => { outputs.add(output); return `${quote(process.execPath)} ${quote(path.resolve("tests/fixtures/workspace-writer.mjs"))} ${quote(output)}`; };
  const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch (error: unknown) { if ((error as NodeJS.ErrnoException).code === "ESRCH") return false; throw error; } };
  const ready = async (output = directory) => {
    await vi.waitFor(() => expect(existsSync(path.join(output, "child.writes"))).toBe(true), { timeout: 10_000 });
    return Promise.all(["parent", "child"].map(async name => Number(await readFile(path.join(output, `${name}.pid`), "utf8"))));
  };

  beforeEach(async () => {
    outputs = new Set();
    directory = await mkdtemp(path.join(liveRoot!, "workspace-scripts-"));
    vi.stubEnv("CRAB_DESKTOP_SETTINGS_PATH", path.join(directory, "settings.json"));
    await run("git", ["init", "--initial-branch=main", directory]);
    await run("git", ["config", "user.name", "Crab Fixture"], { cwd: directory });
    await run("git", ["config", "user.email", "fixture@example.invalid"], { cwd: directory });
    await run("git", ["commit", "--allow-empty", "-m", "Owned fixture"], { cwd: directory });
    orchestrator = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") });
  });
  afterEach(async () => {
    try { await orchestrator.shutdown(); } finally {
      for (const output of outputs) for (const name of ["parent", "child"]) {
        const pid = await readFile(path.join(output, `${name}.pid`), "utf8").then(Number, () => undefined);
        if (pid && alive(pid)) process.kill(pid, "SIGKILL");
      }
      vi.unstubAllEnvs();
      await rm(directory, { recursive: true, force: true });
    }
  });

  it.each(["owned", "borrowed", "primary"])("retires actual Run descendants before a hunk edits an %s workspace", async mode => {
    const parent = await orchestrator.createTask("Workspace", "codex", directory, "HEAD", { worktreeMode: mode === "primary" ? "primary" : "auto" });
    const editor = mode === "borrowed" ? orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Edit")
      : mode === "primary" ? await orchestrator.createTask("Peer", "claude-code", directory, "HEAD", { worktreeMode: "primary" }) : parent;
    const filePath = path.join(parent.worktreePath, "edit-proof.txt");
    await writeFile(filePath, "before");
    await orchestrator.runWorkspace(parent.id, { command: writer() });
    const pids = await ready();
    await orchestrator.editHunk(editor.id, { filePath, oldString: "before", newString: "after" }, "apply");
    expect({ alive: pids.map(alive), contents: await readFile(filePath, "utf8") }).toEqual({ alive: [false, false], contents: "after" });
  }, 30_000);

  it("drains actual shared Run writers before restoring files and can recover its backup", async () => {
    const parent = await orchestrator.createTask("Workspace", "codex", directory);
    const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Restore");
    const snapshots = new CheckpointManager();
    const filePath = path.join(parent.worktreePath, "restore-proof.txt");
    await writeFile(filePath, "baseline");
    const cp = await snapshots.createCheckpoint(child, 0);
    await writeFile(filePath, "output");
    await orchestrator.runWorkspace(parent.id, { command: writer() });
    const pids = await ready();
    await orchestrator.restoreFiles(child.id, cp.ref);
    expect({ alive: pids.map(alive), contents: await readFile(filePath, "utf8") }).toEqual({ alive: [false, false], contents: "baseline" });
    const [backup] = await snapshots.listRestoreBackups(child);
    await orchestrator.restoreFiles(child.id, backup.ref);
    expect(await readFile(filePath, "utf8")).toBe("output");
  }, 30_000);

  it("retires shell descendants before Archive runs its hook or removes the worktree", async () => {
    const task = await orchestrator.createTask("Owned workspace", "codex", directory);
    await orchestrator.runWorkspace(task.id, { command: writer() });
    const pids = await ready();
    const hook = `${quote(process.execPath)} -e ${quote(`require('node:fs').writeFileSync('archive-proof.txt', 'saved')`)}`;
    await writeFile(path.join(directory, "crab-orchestrator.json"), JSON.stringify({ scripts: { archive: hook } }));
    await orchestrator.archiveTask(task.id);
    expect({ alive: pids.map(alive), removed: !existsSync(task.worktreePath), status: task.status,
      saved: (await run("git", ["show", `${task.branch}:archive-proof.txt`], { cwd: directory })).stdout }).toEqual({ alive: [false, false], removed: true, status: "archived", saved: "saved" });
  }, 30_000);

  it("does not stop a peer's shared workspace process when a child conversation is archived", async () => {
    const parent = await orchestrator.createTask("Parent", "codex", directory);
    const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Borrow");
    await orchestrator.runWorkspace(parent.id, { command: writer() });
    const pids = await ready();
    await orchestrator.archiveTask(child.id);
    expect(pids.map(alive)).toEqual([true, true]);
    await orchestrator.archiveTask(parent.id);
    expect(pids.map(alive)).toEqual([false, false]);
  }, 30_000);

  it("stops setup writers on quit and reports cancellation rather than success", async () => {
    const task = await orchestrator.createTask("Setup", "claude-code", directory);
    const setup = orchestrator.runSetupScript(task.id, { setupScript: writer() }).catch(error => String(error));
    const pids = await ready();
    await orchestrator.shutdown();
    expect({ alive: pids.map(alive), result: await setup }).toEqual({ alive: [false, false], result: "Error: Workspace script cancelled" });
  }, 30_000);

  it("waits for the final setup subscriber's real descendants before acknowledging Stop", async () => {
    await writeFile(path.join(directory, "settings.json"), JSON.stringify({ orchestratorSetupScript: writer() }));
    const parent = await orchestrator.createTask("Parent", "codex", directory);
    const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Borrow");
    const starts = [parent, child].map(task => orchestrator.startTask(task.id).catch(error => String(error)));
    try {
      const pids = await ready();
      await orchestrator.cancelTask(child.id);
      expect({ alive: pids.map(alive), parent: parent.status, child: child.status }).toEqual({ alive: [true, true], parent: "running", child: "stopped" });
      await orchestrator.cancelTask(parent.id);
      await Promise.all(starts);
      expect({ alive: pids.map(alive), sessions: [parent.codexThreadId, child.claudeSessionId], statuses: [parent.status, child.status] }).toEqual({ alive: [false, false], sessions: [undefined, undefined], statuses: ["stopped", "stopped"] });
    } finally { await orchestrator.shutdown(); await Promise.all(starts); }
  }, 30_000);

  it("persists a real setup failure and keeps its output without starting a provider", async () => {
    await writeFile(path.join(directory, "settings.json"), JSON.stringify({ orchestratorSetupScript: "printf setup-denied; exit 17" }));
    const task = await orchestrator.createTask("Never dispatch", "claude-code", directory);
    await expect(orchestrator.startTask(task.id)).rejects.toThrow("Workspace setup failed (exit code 17)");
    const saved = new Orchestrator({ persistPath: path.join(directory, "tasks.sqlite") }).getTask(task.id);
    expect({ status: saved?.status, setup: saved?.workspaceSetup, session: saved?.claudeSessionId,
      output: task.events.find(event => event.tool === "Workspace setup")?.stdout }).toEqual({ status: "failed", setup: "failed", session: undefined, output: "setup-denied" });
  }, 30_000);

  it("keeps a primary conversation's user baseline reachable until its conversation is discarded", async () => {
    await writeFile(path.join(directory, "user-baseline.txt"), "user bytes");
    const task = await orchestrator.createTask("Primary", "codex", directory, "HEAD", { worktreeMode: "primary" });
    await run("git", ["prune", "--expire=now"], { cwd: directory });
    const { stdout } = await run("git", ["show", `${task.baseRef}:user-baseline.txt`], { cwd: directory });
    await orchestrator.discardTask(task.id);
    await run("git", ["prune", "--expire=now"], { cwd: directory });
    const present = await run("git", ["cat-file", "-e", task.baseRef], { cwd: directory }).then(() => true, () => false);
    expect({ baseline: stdout, present, file: await readFile(path.join(directory, "user-baseline.txt"), "utf8") }).toEqual({ baseline: "user bytes", present: false, file: "user bytes" });
  }, 30_000);

  it("retains an inherited primary baseline after parent removal until its last child is discarded", async () => {
    await writeFile(path.join(directory, "user-baseline.txt"), "shared bytes");
    const parent = await orchestrator.createTask("Parent", "codex", directory, "HEAD", { worktreeMode: "primary" });
    const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Child");
    await orchestrator.discardTask(parent.id);
    await orchestrator.removeTask(parent.id);
    await run("git", ["prune", "--expire=now"], { cwd: directory });
    const { stdout } = await run("git", ["show", `${child.baseRef}:user-baseline.txt`], { cwd: directory });
    await orchestrator.discardTask(child.id);
    await run("git", ["prune", "--expire=now"], { cwd: directory });
    const present = await run("git", ["cat-file", "-e", child.baseRef], { cwd: directory }).then(() => true, () => false);
    expect({ baseline: stdout, present, file: await readFile(path.join(directory, "user-baseline.txt"), "utf8") }).toEqual({ baseline: "shared bytes", present: false, file: "shared bytes" });
  }, 30_000);

  it("closes writers before Retry restores an externally deleted workspace from its saved branch", async () => {
    const task = await orchestrator.createTask("Restore saved work", "codex", directory);
    await writeFile(path.join(task.worktreePath, "saved.txt"), "saved work");
    await run("git", ["add", "saved.txt"], { cwd: task.worktreePath });
    await run("git", ["commit", "-m", "Saved work"], { cwd: task.worktreePath });
    await orchestrator.runWorkspace(task.id, { command: writer() });
    const pids = await ready();
    const previous = task.worktreePath;
    await rm(previous, { recursive: true, force: true });
    task.status = "failed";
    await writeFile(path.join(directory, "settings.json"), JSON.stringify({ orchestratorSetupScript: "exit 17" }));
    await expect(orchestrator.retryTask(task.id)).rejects.toThrow("Workspace setup failed");
    expect({ alive: pids.map(alive), restored: task.worktreePath !== previous,
      saved: await readFile(path.join(task.worktreePath, "saved.txt"), "utf8"), session: task.codexThreadId }).toEqual({ alive: [false, false], restored: true, saved: "saved work", session: undefined });
  }, 30_000);

  it("closes workspace writers before Apply captures and merges the branch", async () => {
    const task = await orchestrator.createTask("Apply", "codex", directory);
    await writeFile(path.join(task.worktreePath, "apply-proof.txt"), "exact");
    await orchestrator.runWorkspace(task.id, { command: writer() });
    const pids = await ready();
    const result = await orchestrator.applyTask(task.id);
    expect({ result, alive: pids.map(alive), applied: await readFile(path.join(directory, "apply-proof.txt"), "utf8"), status: task.status }).toEqual({ result: { success: true }, alive: [false, false], applied: "exact", status: "applied" });
  }, 30_000);

  it("honors saved nonconcurrent mode and waits for the replaced workspace's descendants", async () => {
    const first = await orchestrator.createTask("First", "codex", directory);
    const second = await orchestrator.createTask("Second", "claude-code", directory);
    const output = path.join(directory, "second");
    await mkdir(output);
    await writeFile(path.join(directory, "settings.json"), JSON.stringify({ orchestratorNonconcurrentRun: true }));
    await orchestrator.runWorkspace(first.id, { command: writer() });
    const firstPids = await ready();
    const result = await orchestrator.runWorkspace(second.id, { command: writer(output) });
    const secondPids = await ready(output);
    expect({ replaced: result.stoppedTaskId, first: firstPids.map(alive), second: secondPids.map(alive) }).toEqual({ replaced: first.id, first: [false, false], second: [true, true] });
    await orchestrator.stopWorkspace(second.id);
    expect(secondPids.map(alive)).toEqual([false, false]);
  }, 30_000);

  it("preserves the workspace when the configured archive hook fails", async () => {
    const task = await orchestrator.createTask("Preserve", "claude-code", directory);
    await writeFile(path.join(task.worktreePath, "retained.txt"), "retained");
    await writeFile(path.join(directory, "settings.json"), JSON.stringify({ orchestratorArchiveScript: "exit 17" }));
    await expect(orchestrator.archiveTask(task.id)).rejects.toThrow("Script exited with code 17");
    expect({ status: task.status, file: await readFile(path.join(task.worktreePath, "retained.txt"), "utf8") }).toEqual({ status: "stopped", file: "retained" });
  }, 30_000);

  it("cancels an in-flight archive hook on quit without reporting a shutdown failure or removing files", async () => {
    const task = await orchestrator.createTask("Archive then quit", "codex", directory);
    await writeFile(path.join(directory, "settings.json"), JSON.stringify({ orchestratorArchiveScript: writer() }));
    const archive = orchestrator.archiveTask(task.id).catch(error => String(error));
    const pids = await ready();
    await orchestrator.shutdown();
    expect({ error: await archive, alive: pids.map(alive), retained: existsSync(task.worktreePath), archived: task.status === "archived" }).toEqual({
      error: "Error: Workspace script cancelled", alive: [false, false], retained: true, archived: false,
    });
  }, 30_000);

  it("blocks new work while a borrowed conversation is being discarded", async () => {
    const parent = await orchestrator.createTask("Parent", "codex", directory);
    const child = orchestrator.createChildTaskForAgent(parent.id, "claude-code", "Borrow");
    await orchestrator.runWorkspace(child.id, { command: writer() });
    await ready();
    const discard = orchestrator.discardTask(child.id);
    try {
      await vi.waitFor(() => expect(existsSync(path.join(directory, "parent.term"))).toBe(true));
      await expect(orchestrator.runWorkspace(child.id, { command: "printf late > late.txt" })).rejects.toThrow("Wait for the operation to finish");
      await discard;
      expect({ status: child.status, peer: parent.status, late: existsSync(path.join(parent.worktreePath, "late.txt")) }).toEqual({ status: "discarded", peer: "created", late: false });
    } finally { await discard; }
  }, 30_000);

  it("keeps ownership through a transient permission-denied existence probe", async () => {
    const task = await orchestrator.createTask("Probe", "codex", directory);
    await orchestrator.runWorkspace(task.id, { command: writer() });
    const pids = await ready();
    const kill = process.kill.bind(process);
    let probes = 0;
    const signal = vi.spyOn(process, "kill").mockImplementation((pid, value) => {
      if (pid < 0 && value === 0 && ++probes === 1) throw Object.assign(new Error("Owned probe denied"), { code: "EPERM" });
      return kill(pid, value);
    });
    try {
      await orchestrator.stopWorkspace(task.id);
      expect({ retried: probes > 1, alive: pids.map(alive) }).toEqual({ retried: true, alive: [false, false] });
    } finally { signal.mockRestore(); }
  }, 30_000);

  it("preserves the workspace when an actual termination signal is denied", async () => {
    const task = await orchestrator.createTask("Signal", "codex", directory);
    await orchestrator.runWorkspace(task.id, { command: writer() });
    const pids = await ready();
    const kill = process.kill.bind(process);
    const signal = vi.spyOn(process, "kill").mockImplementation((pid, value) => {
      if (pid < 0 && value === "SIGTERM") throw Object.assign(new Error("Owned signal denied"), { code: "EPERM" });
      return kill(pid, value);
    });
    try {
      await expect(orchestrator.archiveTask(task.id)).rejects.toThrow("Workspace scripts did not close");
      expect({ retained: existsSync(task.worktreePath), alive: pids.map(alive), archived: task.status === "archived" }).toEqual({ retained: true, alive: [true, true], archived: false });
    } finally { signal.mockRestore(); }
  }, 30_000);
});
