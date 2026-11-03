/**
 * Diff Computation & Apply for the Crab Desktop Orchestrator.
 *
 * Computes structured diffs between the base ref and agent branch HEAD,
 * applies changes via merge or cherry-pick, and handles conflict detection.
 */

import { execFile as execFileCb } from "node:child_process";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { promisify } from "node:util";

import type { Task } from "./orchestrator";
import { removeWorktree } from "./orchestrator-worktree";
import type { RpcRouter } from "./ssh-router";

const execFile = promisify(execFileCb);
const GIT_DIFF_TIMEOUT_MS = 10_000;
const MAX_UNTRACKED_DIFF_BYTES = 256 * 1024;

async function gitForDiff(
  args: string[],
  options: { cwd: string; maxBuffer?: number },
): Promise<{ stdout: string; stderr: string }> {
  return execFile("git", args, {
    cwd: options.cwd,
    maxBuffer: options.maxBuffer,
    timeout: GIT_DIFF_TIMEOUT_MS,
    killSignal: "SIGTERM",
    env: {
      ...process.env,
      GIT_OPTIONAL_LOCKS: "0",
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

async function currentBranch(repoPath: string): Promise<string | null> {
  try {
    const { stdout } = await execFile(
      "git",
      ["symbolic-ref", "-q", "--short", "HEAD"],
      { cwd: repoPath },
    );
    const branch = stdout.trim();
    return branch || null;
  } catch {
    return null;
  }
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

async function gitExitCode(repoPath: string, args: string[]): Promise<number> {
  try {
    await execFile("git", args, { cwd: repoPath });
    return 0;
  } catch (err) {
    const code = (err as { code?: number | string }).code;
    return typeof code === "number" ? code : 1;
  }
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

  const sourceAlreadyMerged = await gitExitCode(
    repoPath,
    ["merge-base", "--is-ancestor", sourceSha, targetSha],
  );
  if (sourceAlreadyMerged === 0) {
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

  await execFile(
    "git",
    ["update-ref", "-m", message, targetRef, mergeCommit, targetSha],
    { cwd: repoPath },
  );

  return { kind: "merged", commit: mergeCommit, targetBranch };
}

// ─── Diff Computation ────────────────────────────────────────────────────────

/**
 * Parses the status letter from `git diff --numstat` / `--name-status` output
 * into a typed status string.
 */
function parseFileStatus(letter: string): DiffFile["status"] {
  switch (letter) {
    case "A":
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
  if (!rawDiff.trim()) return [];

  // Parse name-status to get file statuses and rename info.
  const statusMap = new Map<string, { status: DiffFile["status"]; oldPath?: string }>();
  for (const line of nameStatusOutput.trim().split("\n")) {
    if (!line.trim()) continue;
    const parts = line.split("\t");
    const statusLetter = parts[0].charAt(0); // R100 → R
    const status = parseFileStatus(statusLetter);

    if (status === "renamed" && parts.length >= 3) {
      statusMap.set(parts[2], { status, oldPath: parts[1] });
    } else if (parts.length >= 2) {
      statusMap.set(parts[1], { status });
    }
  }

  // Split the raw diff into per-file patches.
  const fileDiffs = rawDiff.split(/^diff --git /m).filter((s) => s.trim());
  const files: DiffFile[] = [];

  for (const fileDiff of fileDiffs) {
    const patch = `diff --git ${fileDiff}`;

    // Extract the file path from the diff header.
    // Format: "a/path/to/file b/path/to/file"
    const headerMatch = fileDiff.match(/^a\/(.+?) b\/(.+)/m);
    if (!headerMatch) continue;

    const filePath = headerMatch[2];

    // Count additions and deletions from the hunk lines.
    let additions = 0;
    let deletions = 0;
    for (const line of fileDiff.split("\n")) {
      if (line.startsWith("+") && !line.startsWith("+++")) {
        additions++;
      } else if (line.startsWith("-") && !line.startsWith("---")) {
        deletions++;
      }
    }

    const statusInfo = statusMap.get(filePath) ?? { status: "modified" as const };

    files.push({
      path: filePath,
      status: statusInfo.status,
      additions,
      deletions,
      patch,
      ...(statusInfo.oldPath ? { oldPath: statusInfo.oldPath } : {}),
    });
  }

  return files;
}

/**
 * Computes a structured diff between the task's base ref and the current
 * worktree state (including uncommitted changes).
 *
 * Strategy:
 * 1. First try: `git diff <baseRef>` — diffs baseRef against the working tree,
 *    capturing both committed AND uncommitted changes.
 * 2. If that's empty, try uncommitted changes only: `git diff HEAD`
 * 3. If still empty, try staged changes: `git diff --cached`
 */
export async function computeDiff(task: Task): Promise<TaskDiff> {
  const cwd = task.worktreePath || task.repoPath;

  let rawDiff = "";
  let nameStatus = "";

  // Strategy 1: diff from base ref to working tree (committed + uncommitted).
  try {
    const result = await gitForDiff(
      ["diff", task.baseRef],
      { cwd, maxBuffer: 50 * 1024 * 1024 },
    );
    rawDiff = result.stdout;

    const nsResult = await gitForDiff(
      ["diff", "--name-status", task.baseRef],
      { cwd },
    );
    nameStatus = nsResult.stdout;
  } catch {
    // baseRef might not be resolvable in the worktree — try alternatives.
  }

  // Strategy 2: unstaged working tree changes (git diff with no args).
  // This catches edits Claude made without committing.
  if (!rawDiff.trim()) {
    try {
      const result = await gitForDiff(
        ["diff"],
        { cwd, maxBuffer: 50 * 1024 * 1024 },
      );
      rawDiff = result.stdout;

      const nsResult = await gitForDiff(
        ["diff", "--name-status"],
        { cwd },
      );
      nameStatus = nsResult.stdout;
    } catch {
      // Empty repo or no changes.
    }
  }

  // Strategy 3: staged changes (git diff --cached).
  if (!rawDiff.trim()) {
    try {
      const result = await gitForDiff(
        ["diff", "--cached"],
        { cwd, maxBuffer: 50 * 1024 * 1024 },
      );
      rawDiff = result.stdout;

      const nsResult = await gitForDiff(
        ["diff", "--cached", "--name-status"],
        { cwd },
      );
      nameStatus = nsResult.stdout;
    } catch {
      // No staged changes.
    }
  }

  // Strategy 4: check for new untracked files without staging them.
  // Staging from a read-only diff request can block large/filter-backed repos
  // and should never mutate the user's index.
  if (!rawDiff.trim()) {
    try {
      const untracked = await buildUntrackedDiff(cwd);
      rawDiff = untracked.rawDiff;
      nameStatus = untracked.nameStatus;
    } catch {
      // Best-effort.
    }
  }

  // Get the HEAD ref.
  let headRef = "";
  try {
    const result = await gitForDiff(["rev-parse", "HEAD"], { cwd });
    headRef = result.stdout.trim();
  } catch {
    headRef = "unknown";
  }

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

async function buildUntrackedDiff(cwd: string): Promise<{ rawDiff: string; nameStatus: string }> {
  const { stdout } = await gitForDiff(["ls-files", "--others", "--exclude-standard", "-z"], { cwd });
  const files = stdout.split("\0").filter(Boolean);
  const patches: string[] = [];
  const nameStatus: string[] = [];

  for (const filePath of files) {
    const absolutePath = path.resolve(cwd, filePath);
    if (!absolutePath.startsWith(path.resolve(cwd) + path.sep)) continue;

    let stat;
    try {
      stat = await fs.stat(absolutePath);
    } catch {
      continue;
    }
    if (!stat.isFile()) continue;

    nameStatus.push(`A\t${filePath}`);

    let content: Buffer;
    try {
      content = await fs.readFile(absolutePath);
    } catch {
      continue;
    }

    if (content.includes(0) || content.byteLength > MAX_UNTRACKED_DIFF_BYTES) {
      patches.push([
        `diff --git a/${filePath} b/${filePath}`,
        "new file mode 100644",
        "index 0000000..0000000",
        `Binary files /dev/null and b/${filePath} differ`,
      ].join("\n"));
      continue;
    }

    const text = content.toString("utf-8").replace(/\r\n/g, "\n");
    const lines = text.length === 0
      ? []
      : text.endsWith("\n") ? text.slice(0, -1).split("\n") : text.split("\n");
    patches.push([
      `diff --git a/${filePath} b/${filePath}`,
      "new file mode 100644",
      "index 0000000..0000000",
      "--- /dev/null",
      `+++ b/${filePath}`,
      `@@ -0,0 +1,${lines.length} @@`,
      ...lines.map((line) => `+${line}`),
    ].join("\n"));
  }

  return {
    rawDiff: patches.length > 0 ? `${patches.join("\n")}\n` : "",
    nameStatus: nameStatus.length > 0 ? `${nameStatus.join("\n")}\n` : "",
  };
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

  const rawDiff = result.diff ?? "";
  const nameStatus = result.name_status ?? "";
  const headRef = result.head_ref ?? "unknown";

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
 * Merges the agent branch into the base branch where the task was created.
 *
 * Before merging, ensures all changes in the worktree are committed to the
 * agent branch so nothing is lost. Then checks out the base branch in the
 * main repo and merges the agent branch into it.
 *
 * Returns an ApplyResult indicating success or conflict. If the merge fails
 * due to conflicts, the conflict file list is returned and the merge is aborted.
 */
export async function applyAll(task: Task): Promise<ApplyResult> {
  if (isPrimaryWorkspaceTask(task)) {
    return { success: true };
  }

  const commitMessage = `Apply agent task: ${task.prompt.slice(0, 50)}`;

  // First, ensure all worktree changes are committed to the agent branch.
  // The agent may have left uncommitted changes.
  try {
    const { stdout: status } = await execFile(
      "git",
      ["status", "--porcelain"],
      { cwd: task.worktreePath },
    );
    if (status.trim()) {
      await execFile("git", ["add", "-A"], { cwd: task.worktreePath });
      await execFile(
        "git",
        ["commit", "-m", "Agent work (auto-committed before apply)"],
        { cwd: task.worktreePath },
      );
    }
  } catch {
    // Best-effort — if commit fails, the merge will still pick up
    // whatever was already committed on the branch.
  }

  // Determine which branch to merge into.
  // Use the stored baseBranch if available, otherwise detect the current branch.
  let targetBranch: string | undefined = task.baseBranch;

  if (!targetBranch) {
    // Fallback: detect the current branch in the main repo.
    try {
      const { stdout } = await execFile(
        "git", ["rev-parse", "--abbrev-ref", "HEAD"], { cwd: task.repoPath },
      );
      const current = stdout.trim();
      if (current && current !== "HEAD") {
        targetBranch = current;
      }
    } catch {
      // Will proceed without explicit checkout.
    }
  }

  const activeBranch = await currentBranch(task.repoPath);
  if (targetBranch) {
    const targetShort = shortBranch(targetBranch);
    if (activeBranch !== targetShort) {
      const refMerge = await mergeWithoutCheckout(
        task.repoPath,
        task.branch,
        targetBranch,
        commitMessage,
      );
      if (refMerge.kind === "merged") {
        return { success: true };
      }
      if (refMerge.kind === "conflict") {
        return { success: false, conflicts: refMerge.conflicts };
      }
      throw new Error(
        `Target branch "${refMerge.targetBranch}" is checked out in another worktree; switch to that worktree to merge.`,
      );
    }
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

  // Ensure we're on the correct branch before applying files.
  const targetBranch: string | undefined = task.baseBranch;
  if (targetBranch) {
    try {
      const { stdout: currentBranch } = await execFile(
        "git", ["rev-parse", "--abbrev-ref", "HEAD"], { cwd: task.repoPath },
      );
      if (currentBranch.trim() !== targetBranch) {
        await execFile("git", ["checkout", targetBranch], { cwd: task.repoPath });
      }
    } catch {
      // Proceed anyway — let the checkout/add fail with a clear error.
    }
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
export async function discard(task: Task): Promise<void> {
  if (isPrimaryWorkspaceTask(task)) return;
  await removeWorktree(task.repoPath, task.id);
}
