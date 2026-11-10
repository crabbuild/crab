/**
 * Diff Computation & Apply for the Crab Desktop Orchestrator.
 *
 * Computes structured diffs for saved trees and complete live workspaces,
 * applies changes via merge or cherry-pick, and handles conflict detection.
 */

import { execFile as execFileCb, spawn } from "node:child_process";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";
import { createInterface } from "node:readline";
import { promisify } from "node:util";
import type { BrowserWindow, IpcMain } from "electron";

import type { Orchestrator, Task } from "./orchestrator";
import { assertWorkspace, currentBranch, removeWorktree } from "./orchestrator-worktree";
import type { RpcRouter } from "./ssh-router";
import { decodeGitPath } from "./git-path";

const execFile = promisify(execFileCb);
const GIT_DIFF_TIMEOUT_MS = 10_000;

async function gitForDiff(
  args: string[],
  options: { cwd: string; maxBuffer?: number; indexFile?: string },
): Promise<{ stdout: string; stderr: string }> {
  return execFile("git", args, {
    cwd: options.cwd,
    maxBuffer: options.maxBuffer,
    timeout: GIT_DIFF_TIMEOUT_MS,
    killSignal: "SIGTERM",
    env: {
      ...process.env,
      GIT_OPTIONAL_LOCKS: "0",
      ...(options.indexFile ? { GIT_INDEX_FILE: options.indexFile } : {}),
    },
  });
}

// ─── Types ───────────────────────────────────────────────────────────────────

export interface DiffFile {
  path: string;
  status: "added" | "modified" | "deleted" | "renamed";
  additions: number;
  deletions: number;
  patch: string;
  oldPath?: string;
}

export interface TaskDiff {
  taskId: string;
  baseRef: string;
  headRef: string;
  files: DiffFile[];
  totalAdditions: number;
  totalDeletions: number;
  rawDiff: string;
}

export interface ApplyResult {
  success: boolean;
  conflicts?: string[];
}

type RefMergeResult =
  | { kind: "merged"; commit: string; targetBranch: string }
  | { kind: "conflict"; conflicts: string[] }
  | { kind: "target-checked-out"; targetBranch: string };

function isPrimaryWorkspaceTask(task: Task): boolean {
  return task.worktreeMode === "primary";
}

function execErrorText(err: unknown): string {
  const e = err as { message?: string; stdout?: string; stderr?: string };
  return [e.message, e.stdout, e.stderr].filter(Boolean).join("\n");
}

function localBranchRef(branch: string): string {
  return branch.startsWith("refs/heads/") ? branch : `refs/heads/${branch}`;
}

function shortBranch(branch: string): string {
  return branch.replace(/^refs\/heads\//, "");
}

async function applyTarget(task: Task): Promise<string> {
  if (!task.baseBranch) throw new Error("Apply target was not recorded. Recover the task's destination before applying changes.");
  await assertWorkspace(task);
  if (await currentBranch(task.worktreePath) !== shortBranch(task.branch)) throw new Error("Task workspace branch changed. Recover its ownership before applying changes.");
  const target = localBranchRef(task.baseBranch);
  try { await execFile("git", ["show-ref", "--verify", "--", target], { cwd: task.repoPath }); }
  catch (cause: unknown) { throw new Error("Saved Apply target is unavailable. Restore its branch before applying changes.", { cause }); }
  await requireDirectBranch(task.repoPath, target);
  return shortBranch(target);
}

async function requireDirectBranch(repoPath: string, branchRef: string): Promise<void> {
  try {
    await execFile("git", ["symbolic-ref", "-q", "--no-recurse", branchRef], { cwd: repoPath });
  } catch (error: unknown) {
    if ((error as { code?: number | string }).code === 1) return;
    throw error;
  }
  throw new Error(`Apply target "${branchRef}" is a symbolic branch ref. Restore its original direct branch before applying changes.`);
}

async function branchIsCheckedOut(repoPath: string, branchRef: string): Promise<boolean> {
  const { stdout } = await execFile(
    "git",
    ["worktree", "list", "--porcelain"],
    { cwd: repoPath },
  );
  return stdout
    .split("\n")
    .some((line) => line.startsWith("branch ") && line.slice("branch ".length) === branchRef);
}

async function mergeTreeConflictPaths(
  repoPath: string,
  targetSha: string,
  sourceSha: string,
): Promise<string[]> {
  try {
    await execFile(
      "git",
      ["merge-tree", "--write-tree", "--name-only", targetSha, sourceSha],
      { cwd: repoPath },
    );
    return [];
  } catch (err) {
    const e = err as { stdout?: string; stderr?: string };
    const text = [e.stdout, e.stderr].filter(Boolean).join("\n") || execErrorText(err);
    return text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) =>
        line.length > 0 &&
        !/^[0-9a-f]{40}$/.test(line) &&
        !line.startsWith("Auto-merging ") &&
        !line.startsWith("CONFLICT") &&
        !line.includes("\t")
      );
  }
}

async function mergeWithoutCheckout(
  repoPath: string,
  sourceBranch: string,
  targetBranchInput: string,
  message: string,
): Promise<RefMergeResult> {
  const targetRef = localBranchRef(targetBranchInput);
  const targetBranch = shortBranch(targetRef);
  await execFile("git", ["check-ref-format", targetRef], { cwd: repoPath });

  if (await branchIsCheckedOut(repoPath, targetRef)) {
    return { kind: "target-checked-out", targetBranch };
  }

  const { stdout: targetOut } = await execFile(
    "git",
    ["rev-parse", `${targetRef}^{commit}`],
    { cwd: repoPath },
  );
  const targetSha = targetOut.trim();

  const { stdout: sourceOut } = await execFile(
    "git",
    ["rev-parse", `${sourceBranch}^{commit}`],
    { cwd: repoPath },
  );
  const sourceSha = sourceOut.trim();

  const sourceAlreadyMerged = await execFile("git", ["merge-base", "--is-ancestor", sourceSha, targetSha], { cwd: repoPath })
    .then(() => true, (error: unknown) => {
      if ((error as { code?: number | string }).code === 1) return false;
      throw error;
    });
  if (sourceAlreadyMerged) {
    if (!await publishInactiveRef(repoPath, targetRef, targetSha, undefined, message)) {
      return { kind: "target-checked-out", targetBranch };
    }
    return { kind: "merged", commit: targetSha, targetBranch };
  }

  let tree: string;
  try {
    const { stdout } = await execFile(
      "git",
      ["merge-tree", "--write-tree", targetSha, sourceSha],
      { cwd: repoPath },
    );
    tree = stdout.trim();
  } catch {
    return {
      kind: "conflict",
      conflicts: await mergeTreeConflictPaths(repoPath, targetSha, sourceSha),
    };
  }

  const { stdout: commitOut } = await execFile(
    "git",
    ["commit-tree", tree, "-p", targetSha, "-p", sourceSha, "-m", message],
    { cwd: repoPath },
  );
  const mergeCommit = commitOut.trim();

  if (!await publishInactiveRef(repoPath, targetRef, targetSha, mergeCommit, message)) {
    return { kind: "target-checked-out", targetBranch };
  }

  return { kind: "merged", commit: mergeCommit, targetBranch };
}

async function publishInactiveRef(
  repoPath: string,
  targetRef: string,
  old: string,
  next: string | undefined,
  message: string,
): Promise<boolean> {
  const child = spawn("git", ["update-ref", "--stdin", "-m", message], { cwd: repoPath, stdio: "pipe" });
  let failure: Error | undefined;
  let stderr = "";
  child.on("error", error => { failure = error; });
  child.stdin.on("error", error => { failure = error; });
  child.stderr.setEncoding("utf8").on("data", chunk => { stderr += chunk; });
  const closed = new Promise<number | null>(resolve => child.once("close", resolve));
  const reader = createInterface({ input: child.stdout, crlfDelay: Infinity });
  const lines = reader[Symbol.asyncIterator]();
  const step = async (command: string, reply: string) => {
    await new Promise<void>((resolve, reject) => child.stdin.write(`${command}\n`, error => error ? reject(error) : resolve()));
    if ((await lines.next()).value !== reply) throw new Error(`Git reference transaction did not acknowledge ${reply}`);
  };
  const run = async () => {
    await step("start", "start: ok");
    const edit = next ? `update ${targetRef} ${next} ${old}` : `verify ${targetRef} ${old}`;
    await step(`option no-deref\n${edit}\nprepare`, "prepare: ok");
    let available: boolean;
    try {
      // Git checks the old OID of a symbolic ref through its referent.
      // Prepare owns the exact ref while we check whether it is direct.
      await requireDirectBranch(repoPath, targetRef);
      available = !await branchIsCheckedOut(repoPath, targetRef);
    } catch (error: unknown) {
      await step("abort", "abort: ok");
      throw error;
    }
    const action = available ? "commit" : "abort";
    await step(action, `${action}: ok`);
    return available;
  };
  const result = await run().then(value => ({ value }), (error: unknown) => ({ error }));
  child.stdin.end();
  const code = await closed;
  reader.close();
  if (code !== 0 || failure) throw new Error(stderr.trim() || "Git reference transaction failed", { cause: failure ?? ("error" in result ? result.error : undefined) });
  if ("error" in result) throw result.error;
  return result.value;
}

// ─── Diff Computation ────────────────────────────────────────────────────────

/**
 * Parses the status letter from `git diff --numstat` / `--name-status` output
 * into a typed status string.
 */
function parseFileStatus(letter: string): DiffFile["status"] {
  switch (letter) {
    case "A":
    case "C":
      return "added";
    case "D":
      return "deleted";
    case "R":
      return "renamed";
    default:
      return "modified";
  }
}

/**
 * Parses a unified diff string into per-file DiffFile entries.
 *
 * Splits on `diff --git` boundaries and extracts the patch, additions,
 * and deletions for each file.
 */
export function parseDiffOutput(
  rawDiff: string,
  nameStatusOutput: string,
): DiffFile[] {
  const entries = nameStatusOutput.split("\n").filter(line => line.length > 0).map(line => {
    const [code, first, second] = line.split("\t");
    const status = parseFileStatus(code.charAt(0));
    const twoPaths = code.startsWith("R") || code.startsWith("C");
    const filePath = twoPaths ? second : first;
    const decoded = filePath === undefined ? null : decodeGitPath(filePath);
    const oldPath = twoPaths ? decodeGitPath(first) : undefined;
    if (decoded === null || oldPath === null) throw new Error("Remote Git returned an invalid file path");
    return { path: decoded, status, ...(oldPath ? { oldPath } : {}) };
  });
  const patches = rawDiff.split(/(?=^diff --git )/m).filter(Boolean);
  if (patches.length !== entries.length) throw new Error("Remote Git could not pair every changed file with its patch");
  return entries.map((entry, index) => {
    const patch = patches[index];
    return { ...entry, patch, ...patchStats(patch) };
  });
}

/** Owns local/remote comparison routing and exposes read failures to the renderer. */
export function registerDiffIpc(ipcMain: IpcMain, orchestrator: Pick<Orchestrator, "getTask" | "getRpcRouter" | "revertFile">, getMainWindow: () => BrowserWindow | null): void {
  ipcMain.handle("orchestrator:revert-file", async (_evt, payload: { taskId: string; filePath: string }) => {
    try {
      await orchestrator.revertFile(payload.taskId, payload.filePath);
      return { success: true };
    } catch (error: unknown) {
      return { success: false, error: error instanceof Error ? error.message : String(error) };
    }
  });
  ipcMain.handle(
    "orchestrator:get-diff",
    async (_evt, payload: { taskId: string; checkpointSha?: string; checkpointFromSha?: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        let diff;
        if (task.isRemote) {
          if (payload.checkpointSha) throw new Error("Saved-file comparisons require a local workspace");
          const router = orchestrator.getRpcRouter();
          if (!router) throw new Error("SSH router not configured for remote diff");
          diff = await computeRemoteDiff(task, router);
        } else {
          const fromRef = payload.checkpointSha ? payload.checkpointFromSha ?? task.baseRef : task.baseRef;
          const toRef = payload.checkpointSha ?? "HEAD";
          diff = await computeDiff(task, fromRef, toRef);
        }

        // Also relay as a diff-ready event for any listeners.
        const win = getMainWindow();
        if (win && !win.isDestroyed()) {
          win.webContents.send("orchestrator:diff-ready", {
            taskId: task.id,
            diff: diff.rawDiff,
            files: diff.files,
          });
        }
        return { success: true, diff };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );
}

/** Compares saved trees or the complete live workspace without changing its index. */
export async function computeDiff(task: Task, fromRef = task.baseRef, toRef = "HEAD"): Promise<TaskDiff> {
  if (task.isRemote) throw new Error("Remote file comparisons require a connected remote agent");
  const live = toRef === "HEAD";
  const cwd = live ? task.worktreePath : task.repoPath;
  const { stdout: from } = await gitForDiff(["rev-parse", "--verify", "--end-of-options", `${fromRef}^{tree}`], { cwd });
  const args = ["diff", "--patch-with-raw", "-z", "--no-ext-diff", "--no-textconv", "--no-color", "--no-relative",
    "--src-prefix=a/", "--dst-prefix=b/", "--find-renames", "--submodule=short",
    "--output-indicator-new=+", "--output-indicator-old=-", "--output-indicator-context= ", from.trim()];
  let directory: string | undefined;
  let indexFile: string | undefined;
  try {
    if (live) {
      const [root, common, repository] = await Promise.all([
        gitDirectory(cwd, "--show-toplevel"), gitDirectory(cwd, "--git-common-dir"), gitDirectory(task.repoPath, "--git-common-dir"),
      ]);
      if (root !== await fs.realpath(cwd) || common !== repository) throw new Error("Saved workspace no longer matches its Git repository");
      directory = await fs.mkdtemp(path.join(os.tmpdir(), "crab-diff-"));
      indexFile = path.join(directory, "index");
      const { stdout: indexPath } = await gitForDiff(["rev-parse", "--git-path", "index"], { cwd });
      try { await fs.copyFile(path.resolve(cwd, indexPath.replace(/\r?\n$/, "")), indexFile); } catch (error: unknown) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
      // Intent entries let Git compare untracked files (including saved baseline
      // paths) without staging content. The user's real index remains untouched.
      await gitForDiff(["add", "--intent-to-add", "--all", "--", "."], { cwd, indexFile });
    } else {
      const { stdout: to } = await gitForDiff(["rev-parse", "--verify", "--end-of-options", `${toRef}^{tree}`], { cwd });
      args.push(to.trim());
    }
    args.push("--");
    // One invocation owns paths and patches; separate reads can disagree while
    // files change. NUL-delimited raw paths preserve quoted/newline filenames.
    const { stdout } = await gitForDiff(args, { cwd, indexFile, maxBuffer: 50 * 1024 * 1024 });
    const { files, rawDiff } = parseRawDiff(stdout);
    return {
      taskId: task.id, baseRef: fromRef, headRef: toRef, files, rawDiff,
      totalAdditions: files.reduce((sum, file) => sum + file.additions, 0),
      totalDeletions: files.reduce((sum, file) => sum + file.deletions, 0),
    };
  } finally {
    if (directory) await fs.rm(directory, { recursive: true, force: true });
  }
}

async function gitDirectory(cwd: string, option: "--show-toplevel" | "--git-common-dir"): Promise<string> {
  const { stdout } = await gitForDiff(["rev-parse", option], { cwd });
  return fs.realpath(path.resolve(cwd, stdout.replace(/\r?\n$/, "")));
}

function parseRawDiff(output: string): { files: DiffFile[]; rawDiff: string } {
  const entries: Array<Pick<DiffFile, "path" | "status" | "oldPath">> = [];
  let cursor = 0;
  const field = () => {
    const end = output.indexOf("\0", cursor);
    if (end < 0) throw new Error("Git returned an incomplete file comparison");
    const value = output.slice(cursor, end);
    cursor = end + 1;
    return value;
  };
  while (output[cursor] === ":") {
    const status = field().split(" ").at(-1)?.charAt(0);
    const first = field();
    const renamed = status === "R" || status === "C";
    entries.push({ path: renamed ? field() : first, status: parseFileStatus(status ?? ""), ...(renamed ? { oldPath: first } : {}) });
  }
  const rawDiff = output.slice(cursor).replace(/^\0/, "");
  const patches = rawDiff.split(/(?=^diff --git )/m).filter(Boolean);
  if (entries.length !== patches.length) throw new Error("Git could not pair every changed file with its patch");
  const files = entries.map((entry, index) => {
    const patch = patches[index];
    return { ...entry, patch, ...patchStats(patch) };
  });
  return { files, rawDiff };
}

function patchStats(patch: string): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  let inHunk = false;
  for (const line of patch.split("\n")) {
    if (line.startsWith("@@")) inHunk = true;
    else if (inHunk && line.startsWith("+")) additions++;
    else if (inHunk && line.startsWith("-")) deletions++;
  }
  return { additions, deletions };
}

// ─── Remote Diff Computation ─────────────────────────────────────────────────

/**
 * Computes a structured diff for a remote task by sending a `task_compute_diff`
 * RPC to the remote agent. The remote agent runs `git diff` in the worktree
 * and returns the raw diff text + name-status output.
 *
 * The returned TaskDiff is identical in shape to local diffs, so the renderer's
 * Diff tab renders it without any changes.
 */
export async function computeRemoteDiff(
  task: Task,
  rpcRouter: RpcRouter,
): Promise<TaskDiff> {
  if (!task.remoteProfileId) {
    throw new Error(
      `Cannot compute remote diff: task "${task.id}" has no remoteProfileId.`,
    );
  }

  rpcRouter.assertRemoteEndpoint(task.remoteProfileId, task.remoteEndpoint);

  const result = await rpcRouter.sendToRemoteAgent(
    task.remoteProfileId,
    "task_compute_diff",
    {
      repo_id: task.repoPath,
      worktree_path: task.worktreePath,
      base_ref: task.baseRef,
      branch: task.branch,
    },
  ) as {
    diff: string;
    name_status: string;
    head_ref: string;
  };

  const rawDiff = result.diff;
  const nameStatus = result.name_status;
  const headRef = result.head_ref;
  if (typeof rawDiff !== "string" || typeof nameStatus !== "string" || typeof headRef !== "string") throw new Error("Remote agent returned an incomplete file comparison");

  const files = parseDiffOutput(rawDiff, nameStatus);
  const totalAdditions = files.reduce((sum, f) => sum + f.additions, 0);
  const totalDeletions = files.reduce((sum, f) => sum + f.deletions, 0);

  return {
    taskId: task.id,
    baseRef: task.baseRef,
    headRef,
    files,
    totalAdditions,
    totalDeletions,
    rawDiff,
  };
}

// ─── Apply ───────────────────────────────────────────────────────────────────

/**
 * Merges the task branch into its captured destination after committing pending source changes.
 * Rejects changed workspace identities, missing targets and failed source commits;
 * returns conflicts after aborting a conflicting merge.
 */
export async function applyAll(task: Task): Promise<ApplyResult> {
  if (isPrimaryWorkspaceTask(task)) {
    return { success: true };
  }

  const commitMessage = `Apply agent task: ${task.prompt.slice(0, 50)}`;
  // Validate the captured destination before staging source work. A missing
  // target cannot grant the branch checked out today ownership of this Apply.
  const targetBranch = await applyTarget(task);

  const { stdout: status } = await execFile("git", ["status", "--porcelain"], { cwd: task.worktreePath });
  if (status.trim()) {
    await execFile("git", ["add", "-A"], { cwd: task.worktreePath });
    await execFile("git", ["commit", "-m", "Agent work (auto-committed before apply)"], { cwd: task.worktreePath });
  }

  const activeBranch = await currentBranch(task.repoPath);
  if (activeBranch !== targetBranch) {
    const refMerge = await mergeWithoutCheckout(task.repoPath, task.branch, targetBranch, commitMessage);
    if (refMerge.kind === "merged") {
      return { success: true };
    }
    if (refMerge.kind === "conflict") {
      return { success: false, conflicts: refMerge.conflicts };
    }
    throw new Error(`Target branch "${refMerge.targetBranch}" is checked out in another worktree; switch to that worktree to merge.`);
  }

  try {
    await execFile(
      "git",
      ["merge", task.branch, "--no-ff", "-m", commitMessage],
      { cwd: task.repoPath },
    );
    return { success: true };
  } catch (err: unknown) {
    // execFile errors include stdout/stderr as properties on the error object.
    const errObj = err as { message?: string; stdout?: string; stderr?: string };
    const combined = [errObj.message, errObj.stdout, errObj.stderr]
      .filter(Boolean)
      .join("\n");

    // Check if this is a merge conflict.
    if (combined.includes("CONFLICT") || combined.includes("Automatic merge failed")) {
      const conflicts = await getConflictFiles(task.repoPath);

      // Abort the failed merge to leave the repo in a clean state.
      try {
        await execFile("git", ["merge", "--abort"], { cwd: task.repoPath });
      } catch {
        // Best-effort abort.
      }

      return { success: false, conflicts };
    }

    throw err;
  }
}

/**
 * Parses the list of conflicting files from `git diff --name-only --diff-filter=U`.
 */
async function getConflictFiles(repoPath: string): Promise<string[]> {
  try {
    const { stdout } = await execFile(
      "git",
      ["diff", "--name-only", "--diff-filter=U"],
      { cwd: repoPath },
    );
    return stdout.trim().split("\n").filter((f) => f.trim());
  } catch {
    return [];
  }
}

// ─── Cherry-Pick ─────────────────────────────────────────────────────────────

/**
 * Checks out selected files from the agent branch, stages, and commits them.
 *
 * Ensures the main repo is on the correct base branch before applying.
 * Uses `git checkout <branch> -- <file>` for each file, then stages
 * and commits all checked-out files into the base branch.
 */
export async function cherryPickFiles(
  task: Task,
  files: string[],
): Promise<void> {
  if (isPrimaryWorkspaceTask(task)) return;
  if (files.length === 0) return;

  const targetBranch = await applyTarget(task);
  if (await currentBranch(task.repoPath) !== targetBranch) {
    await execFile("git", ["checkout", targetBranch], { cwd: task.repoPath });
  }

  // Checkout each file from the agent branch.
  for (const file of files) {
    await execFile(
      "git",
      ["checkout", task.branch, "--", file],
      { cwd: task.repoPath },
    );
  }

  // Stage all the checked-out files.
  await execFile("git", ["add", ...files], { cwd: task.repoPath });

  // Check if there are actually staged changes to commit.
  // If the files are already identical (e.g., after a prior merge), git commit
  // would fail with "nothing to commit". In that case, succeed silently.
  const { stdout: diffStaged } = await execFile(
    "git",
    ["diff", "--cached", "--name-only"],
    { cwd: task.repoPath },
  );
  if (!diffStaged.trim()) {
    // Nothing to commit — files are already up to date.
    return;
  }

  // Commit the cherry-picked files.
  const fileList = files.length <= 3
    ? files.join(", ")
    : `${files.slice(0, 3).join(", ")} (+${files.length - 3} more)`;
  const commitMessage = `Apply selected files from agent task: ${fileList}`;
  await execFile(
    "git",
    ["commit", "-m", commitMessage],
    { cwd: task.repoPath },
  );
}

// ─── Discard ─────────────────────────────────────────────────────────────────

/**
 * Discards a task by removing its worktree and branch.
 *
 * Delegates to `removeWorktree` from the worktree module.
 */
export async function discard(task: Task): Promise<string | undefined> {
  if (isPrimaryWorkspaceTask(task)) return;
  return removeWorktree(task);
}
