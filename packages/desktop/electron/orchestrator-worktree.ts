/**
 * Worktree isolation for the Crab Desktop Orchestrator.
 *
 * Provides functions for creating, listing, and removing agent worktrees.
 * Each agent task runs in an isolated git worktree under `.crab/worktrees/`
 * with a Docker-style friendly name (`adjective-noun`), ensuring the user's
 * main checkout is never modified until they explicitly apply changes.
 *
 * Naming convention:
 * - Directory: `.crab/worktrees/<friendly-name>/`
 * - Branch: `agent/<friendly-name>`
 * - Terminal prompt: `~/repo/.crab/worktrees/swift-falcon git:(agent/swift-falcon)`
 */

import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as path from "node:path";
import * as fs from "node:fs";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface WorktreeInfo {
  /** Absolute path to the worktree directory. */
  path: string;
  /** Git branch name (e.g., `agent/swift-falcon`). */
  branch: string;
  /** The friendly name (e.g., `swift-falcon`). */
  name: string;
  /** Internal task ID stored in metadata (for desktop correlation). */
  taskId: string;
  /** HEAD commit SHA. */
  head: string;
}

export interface TaskMetadata {
  /** Friendly name (matches directory name). */
  name: string;
  /** Internal task ID for desktop correlation. */
  taskId: string;
  /** The prompt that initiated this agent session. */
  prompt?: string;
  /** ISO 8601 creation timestamp. */
  createdAt: string;
  /** Current lifecycle state. */
  status: "active" | "stopped" | "applied" | "discarded" | "unknown";
  /** Git ref the worktree was branched from. */
  baseRef: string;
  /** Original branch name. */
  originalBranch: string;
  /** Branch name after rename (if any). */
  renamedTo?: string;
}

export interface Task {
  id: string;
  status: string;
  worktreePath: string;
  branch: string;
  createdAt: number;
  [key: string]: unknown;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const WORKTREES_DIR = ".crab/worktrees";
const BRANCH_PREFIX = "agent/";
const METADATA_FILE = "crab-desktop-task.json";
const DEFAULT_RETENTION_DAYS = 7;

interface CrabEnvelope<T> {
  data?: T;
  error?: unknown;
}

interface CrabWorktreeListPayload {
  worktrees: CrabWorktreeListEntry[];
}

interface CrabWorktreeListEntry {
  path: string;
  head?: string;
  branch?: string;
}

function crabBinary(): string {
  return process.env.CRAB_BIN || "crab";
}

async function execCrabWorktreeJson<T>(repoPath: string, args: string[]): Promise<T> {
  const { stdout } = await execFile(crabBinary(), args, { cwd: repoPath });
  const jsonLine = stdout
    .split(/\r?\n/)
    .find((line) => line.trimStart().startsWith("{"));
  if (!jsonLine) {
    throw new Error("crab worktree command returned no JSON output");
  }
  const envelope = JSON.parse(jsonLine) as CrabEnvelope<T>;
  if (envelope.data) return envelope.data;
  throw new Error(`crab worktree command failed: ${JSON.stringify(envelope.error)}`);
}

// ─── Name Generator ──────────────────────────────────────────────────────────

/** Curated adjectives: 3–7 chars, common English, unambiguous. */
const ADJECTIVES = [
  "swift", "bold", "calm", "keen", "warm", "quick", "sharp",
  "steady", "quiet", "vivid", "agile", "crisp", "fresh", "light",
  "smooth", "clever", "nimble", "rapid", "silent", "gentle",
  "fierce", "noble", "proud", "brave", "witty", "lucid", "neat",
  "tidy", "snug", "clear", "dense", "fleet", "grand", "hardy",
  "jolly", "lively", "mellow", "plucky", "robust", "serene",
  "stable", "tender", "vast", "wise", "zesty", "active", "deft",
  "eager", "frank", "golden", "humble", "bright",
] as const;

/** Curated nouns: 3–7 chars, common English, unambiguous. */
const NOUNS = [
  "falcon", "cedar", "prism", "spark", "ridge", "delta", "orbit",
  "nexus", "forge", "atlas", "ember", "frost", "coral", "flint",
  "grove", "haven", "crest", "bloom", "drift", "pulse", "quartz",
  "raven", "slate", "thorn", "vapor", "arrow", "beacon", "cipher",
  "dune", "fern", "glyph", "heron", "inlet", "jade", "knoll",
  "larch", "marsh", "opal", "peak", "reef", "shard", "tide",
  "vale", "wren", "birch", "cliff", "elm", "fjord", "glen",
  "hawk", "ivy", "loom", "moss",
] as const;

/**
 * Generates a unique friendly name in `adjective-noun` format.
 * Appends a numeric suffix on collision (e.g., `swift-falcon-2`).
 */
export function generateFriendlyName(existing: Set<string>): string {
  const maxAttempts = 100;

  for (let i = 0; i < maxAttempts; i++) {
    const adj = ADJECTIVES[Math.floor(Math.random() * ADJECTIVES.length)];
    const noun = NOUNS[Math.floor(Math.random() * NOUNS.length)];
    const candidate = `${adj}-${noun}`;

    if (!existing.has(candidate)) {
      return candidate;
    }

    // Try with numeric suffix.
    for (let suffix = 2; suffix <= 99; suffix++) {
      const suffixed = `${candidate}-${suffix}`;
      if (!existing.has(suffixed)) {
        return suffixed;
      }
    }
  }

  // Extremely unlikely fallback: use timestamp.
  return `agent-${Date.now().toString(36)}`;
}

// ─── Metadata ────────────────────────────────────────────────────────────────

function metadataPath(worktreePath: string): string {
  // Relative gitdir pointers use the physical checkout, including symlinked paths.
  const workspace = fs.realpathSync(worktreePath);
  const gitPath = path.join(workspace, ".git");
  if (fs.statSync(gitPath).isDirectory()) return path.join(gitPath, METADATA_FILE);
  const pointer = fs.readFileSync(gitPath, "utf-8").trim();
  if (!pointer.startsWith("gitdir:")) throw new Error("Workspace has an invalid Git directory pointer");
  const gitDir = pointer.slice("gitdir:".length).trim();
  if (!gitDir) throw new Error("Workspace has an empty Git directory pointer");
  return path.join(path.resolve(workspace, gitDir), METADATA_FILE);
}

/**
 * Reads task metadata from the worktree's private Git administration directory.
 * Returns undefined if the file is missing or unreadable.
 */
export function readTaskMetadata(worktreePath: string): TaskMetadata | undefined {
  try {
    const content = fs.readFileSync(metadataPath(worktreePath), "utf-8");
    return JSON.parse(content) as TaskMetadata;
  } catch {
    return undefined;
  }
}

/**
 * Writes task metadata outside the working tree so snapshots cannot commit it.
 */
export function writeTaskMetadata(worktreePath: string, metadata: TaskMetadata): void {
  fs.writeFileSync(metadataPath(worktreePath), JSON.stringify(metadata, null, 2) + "\n", "utf-8");
}

/** Moves the tagged source-file ownership format into Git administration storage. */
export function migrateWorktreeMetadata(taskId: string, worktreePath: string): void {
  // v2.0.0-alpha.1 stored control data in source. Migrate that shipped format
  // once; ordinary readers only use the private Git administration directory.
  const legacyPath = path.join(worktreePath, ".crab-task.json");
  if (!fs.existsSync(legacyPath)) return;
  const current = readTaskMetadata(worktreePath);
  let legacy: TaskMetadata;
  try { legacy = JSON.parse(fs.readFileSync(legacyPath, "utf-8")) as TaskMetadata; } catch (cause: unknown) {
    if (current?.taskId === taskId) return;
    throw new Error("Workspace ownership metadata could not be migrated", { cause });
  }
  const matches = legacy?.taskId === taskId && typeof legacy.name === "string" && typeof legacy.originalBranch === "string"
    && typeof legacy.baseRef === "string" && typeof legacy.createdAt === "string" && typeof legacy.status === "string";
  if (!matches) {
    if (current?.taskId === taskId) return;
    throw new Error("Workspace ownership metadata does not match its task");
  }
  if (current && current.taskId !== taskId) throw new Error("Git administration metadata belongs to another task");
  if (!current) writeTaskMetadata(worktreePath, legacy);
  fs.unlinkSync(legacyPath);
}

/**
 * Updates a single field in the task metadata file.
 */
export function updateTaskMetadata(
  worktreePath: string,
  updates: Partial<TaskMetadata>,
): void {
  const existing = readTaskMetadata(worktreePath);
  if (!existing) return;
  writeTaskMetadata(worktreePath, { ...existing, ...updates });
}

// ─── Safety ──────────────────────────────────────────────────────────────────

/**
 * Validates that a worktree path is under the expected `.crab/worktrees/`
 * directory within a repository. Prevents accidental operations on paths
 * outside the expected location.
 */
export function validateWorktreePath(worktreePath: string, repoPath: string): boolean {
  const normalizedPath = path.resolve(worktreePath);
  const expectedBase = path.resolve(repoPath, WORKTREES_DIR);

  if (!normalizedPath.startsWith(expectedBase + path.sep)) {
    return false;
  }

  // Verify the path is exactly one level deep under worktrees/.
  const relative = path.relative(expectedBase, normalizedPath);
  return !relative.includes(path.sep) && relative.length > 0;
}

// ─── Worktree Operations ─────────────────────────────────────────────────────

/**
 * Resolves the worktrees directory for a repository, creating it if needed.
 */
function ensureWorktreesDir(repoPath: string): string {
  const worktreesDir = path.join(repoPath, WORKTREES_DIR);
  fs.mkdirSync(worktreesDir, { recursive: true });
  return worktreesDir;
}

/**
 * Collects the set of existing agent worktree names for collision detection.
 */
async function getExistingNames(repoPath: string): Promise<Set<string>> {
  const names = new Set<string>();
  const worktreesDir = path.join(repoPath, WORKTREES_DIR);

  // Read from filesystem (fast, no git command needed).
  try {
    const entries = fs.readdirSync(worktreesDir, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.isDirectory()) {
        names.add(entry.name);
      }
    }
  } catch {
    // Directory doesn't exist yet — no existing names.
  }

  return names;
}

/**
 * Creates an isolated git worktree for an agent task.
 *
 * Uses a Docker-style friendly name (`adjective-noun`) instead of a UUID.
 * The worktree is created under `.crab/worktrees/<name>/` on branch
 * `agent/<name>`.
 *
 * Returns the absolute path to the new worktree.
 */
export async function createWorktree(
  repoPath: string,
  taskId: string,
  baseRef: string,
): Promise<string> {
  const worktreesDir = ensureWorktreesDir(repoPath);
  const existing = await getExistingNames(repoPath);
  const name = generateFriendlyName(existing);

  const worktreePath = path.join(worktreesDir, name);
  const branch = `${BRANCH_PREFIX}${name}`;

  // Validate that the baseRef exists. If not, fall back to HEAD.
  let resolvedRef = baseRef;
  if (baseRef && baseRef !== "HEAD") {
    try {
      await execFile("git", ["rev-parse", "--verify", baseRef], { cwd: repoPath });
    } catch {
      // Try with origin/ prefix (remote tracking branch).
      try {
        await execFile("git", ["rev-parse", "--verify", `origin/${baseRef}`], { cwd: repoPath });
        resolvedRef = `origin/${baseRef}`;
      } catch {
        // Neither local nor remote ref exists — fall back to HEAD.
        resolvedRef = "HEAD";
      }
    }
  }

  await execCrabWorktreeJson(repoPath, [
    "worktree",
    "--json",
    "add",
    "-b",
    branch,
    worktreePath,
    resolvedRef,
  ]);

  // Write task metadata.
  const metadata: TaskMetadata = {
    name,
    taskId,
    createdAt: new Date().toISOString(),
    status: "active",
    baseRef: resolvedRef,
    originalBranch: branch,
  };
  writeTaskMetadata(worktreePath, metadata);

  return worktreePath;
}

/** Recreates a task workspace on its saved local branch; a missing branch is an error. */
export async function restoreWorktree(repoPath: string, taskId: string, branch: string, previousPath: string): Promise<string> {
  // Restore the saved branch exactly. Creating from a base ref could silently
  // replace archived work with HEAD or attach the task to a different branch.
  await execFile("git", ["show-ref", "--verify", `refs/heads/${branch}`], { cwd: repoPath });
  if (fs.existsSync(previousPath)) throw new Error("The previous workspace still exists");
  // Git records canonical paths. Resolve the surviving ancestors too when a
  // deleted workspace was reached through a symlink such as macOS /var.
  let ancestor = path.resolve(previousPath);
  const suffix: string[] = [];
  while (!fs.existsSync(ancestor)) {
    const parent = path.dirname(ancestor);
    if (parent === ancestor) throw new Error("Cannot resolve the missing workspace path");
    suffix.unshift(path.basename(ancestor));
    ancestor = parent;
  }
  const previous = path.join(fs.realpathSync(ancestor), ...suffix);
  const { stdout } = await execFile("git", ["worktree", "list", "--porcelain", "-z"], { cwd: repoPath });
  const registered = stdout.split("\0\0").some(record => {
    const fields = record.split("\0");
    const recordedPath = fields.find(field => field.startsWith("worktree "))?.slice("worktree ".length);
    return recordedPath !== undefined && path.normalize(recordedPath) === path.normalize(previous) && fields.includes(`branch refs/heads/${branch}`);
  });
  // Remove only this absent checkout's registration. Git retains locked and
  // live-worktree safeguards; broad pruning could affect unrelated workspaces.
  if (registered) await execCrabWorktreeJson(repoPath, ["worktree", "--json", "remove", previous]);
  const worktreesDir = ensureWorktreesDir(repoPath);
  const existing = await getExistingNames(repoPath);
  const name = generateFriendlyName(existing);
  const worktreePath = path.join(worktreesDir, name);
  await execCrabWorktreeJson(repoPath, ["worktree", "--json", "add", worktreePath, branch]);
  writeTaskMetadata(worktreePath, { name, taskId, createdAt: new Date().toISOString(), status: "active", baseRef: branch, originalBranch: branch });
  return worktreePath;
}

/**
 * Removes an agent worktree, preserving its branch when archiving.
 *
 * Accepts a repository-local name or task ID. Archive requires a clean tree
 * and preserves its branch; discard can remove unsaved work and the branch.
 * An already-removed workspace is a no-op; other removal failures propagate.
 */
export async function removeWorktree(
  repoPath: string,
  nameOrTaskId: string,
  options: { preserveBranch?: boolean } = {},
): Promise<void> {
  // Resolve the worktree path — try as friendly name first, then as task ID.
  const worktreePath = await resolveWorktreePath(repoPath, nameOrTaskId);
  if (!worktreePath) {
    // Nothing to remove — already gone.
    return;
  }

  // Read metadata to get the branch name (may have been renamed).
  const metadata = readTaskMetadata(worktreePath);
  const branch = metadata?.renamedTo ?? metadata?.originalBranch ?? `${BRANCH_PREFIX}${nameOrTaskId}`;

  // Safety check before any destructive operation.
  if (!validateWorktreePath(worktreePath, repoPath)) {
    throw new Error(
      `Refusing to remove worktree: path "${worktreePath}" is outside the expected worktrees directory`,
    );
  }

  // Archive requires a saved, clean tree. Discard may intentionally remove
  // unsaved work; archive must fail if changes appeared after its snapshot.
  try {
    await execCrabWorktreeJson(repoPath, [
      "worktree",
      "--json",
      "remove",
      ...(options.preserveBranch ? [] : ["--force"]),
      worktreePath,
    ]);
  } catch (err: unknown) {
    // Tolerate "not a working tree" or "does not exist" errors —
    // the worktree may have already been removed manually.
    const message = err instanceof Error ? err.message : String(err);
    if (
      !message.includes("is not a working tree") &&
      !message.includes("No such file or directory") &&
      !message.includes("is not a valid path")
    ) {
      throw err;
    }
  }

  // Prune stale worktree references so git doesn't complain.
  try {
    await execCrabWorktreeJson(repoPath, ["worktree", "--json", "prune"]);
  } catch {
    // Non-fatal — prune is best-effort.
  }

  if (options.preserveBranch) return;

  // Delete the branch.
  try {
    await execFile("git", ["branch", "-D", branch], { cwd: repoPath });
  } catch (err: unknown) {
    // Tolerate "not found" — branch may have already been deleted.
    const message = err instanceof Error ? err.message : String(err);
    if (!message.includes("not found")) {
      throw err;
    }
  }
}

/**
 * Resolves a worktree path from either a friendly name or a task ID.
 * Returns undefined if no matching worktree is found.
 */
async function resolveWorktreePath(
  repoPath: string,
  nameOrTaskId: string,
): Promise<string | undefined> {
  const worktreesDir = path.join(repoPath, WORKTREES_DIR);

  // Try as friendly name (direct directory lookup).
  const directPath = path.join(worktreesDir, nameOrTaskId);
  if (fs.existsSync(directPath)) {
    return directPath;
  }

  // Try as task ID (scan metadata files).
  try {
    const entries = fs.readdirSync(worktreesDir, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const wtPath = path.join(worktreesDir, entry.name);
      const meta = readTaskMetadata(wtPath);
      if (meta?.taskId === nameOrTaskId) {
        return wtPath;
      }
    }
  } catch {
    // Directory doesn't exist — nothing to resolve.
  }

  return undefined;
}

/**
 * Lists active agent worktrees for the given repository.
 *
 * Reads from `.crab/worktrees/` directory and augments with git state.
 */
export async function listWorktrees(repoPath: string): Promise<WorktreeInfo[]> {
  const worktreesDir = path.join(repoPath, WORKTREES_DIR);
  const worktrees: WorktreeInfo[] = [];

  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(worktreesDir, { withFileTypes: true });
  } catch {
    return worktrees; // No worktrees directory — empty list.
  }

  // Also parse core Crab worktree JSON for HEAD info.
  const headMap = new Map<string, string>();
  try {
    const payload = await execCrabWorktreeJson<CrabWorktreeListPayload>(repoPath, [
      "worktree",
      "list",
      "--json",
    ]);
    for (const worktree of payload.worktrees) {
      if (worktree.head) {
        headMap.set(path.resolve(worktree.path), worktree.head);
      }
    }
  } catch {
    // Non-fatal — we can still list without HEAD info.
  }

  for (const entry of entries) {
    if (!entry.isDirectory()) continue;

    const wtPath = path.join(worktreesDir, entry.name);
    const metadata = readTaskMetadata(wtPath);
    const name = entry.name;
    const branch = metadata?.renamedTo ?? metadata?.originalBranch ?? `${BRANCH_PREFIX}${name}`;
    const taskId = metadata?.taskId ?? name;
    const head = headMap.get(path.resolve(wtPath)) ?? "";

    worktrees.push({ path: wtPath, branch, name, taskId, head });
  }

  return worktrees;
}

/**
 * Returns the friendly name for a worktree given a task ID.
 * Used by the orchestrator to display human-readable names in the UI.
 */
export function getWorktreeName(repoPath: string, taskId: string): string | undefined {
  const worktreesDir = path.join(repoPath, WORKTREES_DIR);
  try {
    const entries = fs.readdirSync(worktreesDir, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const wtPath = path.join(worktreesDir, entry.name);
      const meta = readTaskMetadata(wtPath);
      if (meta?.taskId === taskId) {
        return entry.name;
      }
    }
  } catch {
    // Directory doesn't exist.
  }
  return undefined;
}

/**
 * Prunes agent worktrees older than the specified retention period.
 *
 * Iterates through the provided task list and removes worktrees for tasks
 * that are older than `retentionDays` and in a terminal state (applied,
 * discarded, failed, stopped).
 *
 * Returns the list of pruned task IDs.
 */
export async function pruneOldWorktrees(
  repoPath: string,
  tasks: Task[],
  retentionDays: number = DEFAULT_RETENTION_DAYS,
): Promise<string[]> {
  const cutoff = Date.now() - retentionDays * 24 * 60 * 60 * 1000;
  const terminalStatuses = new Set(["applied", "discarded", "failed", "stopped"]);
  const pruned: string[] = [];

  for (const task of tasks) {
    if (!terminalStatuses.has(task.status)) {
      continue;
    }

    if (task.createdAt >= cutoff) {
      continue;
    }

    try {
      await removeWorktree(repoPath, task.id);
      pruned.push(task.id);
    } catch {
      // Best-effort pruning — log but don't fail the batch.
    }
  }

  return pruned;
}
