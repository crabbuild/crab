/**
 * Hunk-level apply / revert for the orchestrator diff viewer.
 *
 * Reads the file in the agent's worktree, performs an in-memory string
 * replacement for the `old_string` → `new_string` pair the Edit tool
 * reported, and writes the result back. Revert does the
 * opposite: `new_string` → `old_string`.
 *
 * Text edits require an exact, unique preimage. Unified patch changes
 * need their provider format or a checkpoint; empty strings cannot
 * substitute for a missing preimage.
 */

import * as fs from "node:fs/promises";
import * as path from "node:path";

export interface HunkPatch {
  /** Workspace-relative or absolute path of the file to patch. */
  filePath: string;
  oldString: string;
  newString: string;
}

/** Applies or reverts an exact text edit within the physical workspace. */
export async function editHunk(worktreePath: string, patch: HunkPatch, operation: "apply" | "revert"): Promise<void> {
  if (!patch.filePath) throw new Error("Missing file path for hunk operation");
  const root = await fs.realpath(worktreePath);
  const target = path.resolve(worktreePath, patch.filePath);
  const entry = await fs.lstat(target).catch((error: NodeJS.ErrnoException) => {
    if (error.code !== "ENOENT") throw error;
    return undefined;
  });
  if (entry?.isSymbolicLink()) throw new Error("Open the symbolic link's target before editing its hunks");
  let parent = path.dirname(target);
  const suffix = [path.basename(target)];
  let physical: string;
  for (;;) {
    try { physical = await fs.realpath(parent); break; } catch (error: unknown) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      suffix.unshift(path.basename(parent));
      const ancestor = path.dirname(parent);
      if (ancestor === parent) throw error;
      parent = ancestor;
    }
  }
  const filePath = path.join(physical, ...suffix);
  const relative = path.relative(root, filePath);
  if (!relative || relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`File ${patch.filePath} is outside the task worktree`);
  }
  const resolved = { ...patch, filePath };
  if (operation === "apply") await applyHunk(resolved);
  else await revertHunk(resolved);
}

/**
 * Apply a single edit to disk. If `oldString` is empty this creates the
 * file with `newString` as its content. Existing different contents are rejected.
 * Fails if the old content isn't found — callers should surface the
 * error so the user knows to re-run the diff.
 */
async function applyHunk(patch: HunkPatch): Promise<void> {
  const { filePath, oldString, newString } = patch;

  // Without a preimage, overwriting an existing file would lose unrelated edits.
  if (!oldString) {
    const current = await fs.readFile(filePath, "utf-8").catch((error: NodeJS.ErrnoException) => {
      if (error.code !== "ENOENT") throw error;
      return undefined;
    });
    if (current === newString) return;
    if (current !== undefined) throw new Error(`Cannot apply hunk: ${filePath} already has different contents`);
    await fs.mkdir(path.dirname(filePath), { recursive: true });
    await fs.writeFile(filePath, newString, { encoding: "utf-8", flag: "wx" });
    return;
  }

  const current = await fs.readFile(filePath, "utf-8");
  const next = replaceUnique(current, oldString, newString);
  await fs.writeFile(filePath, next, "utf-8");
}

/** Revert a single edit (swap new_string back to old_string). */
async function revertHunk(patch: HunkPatch): Promise<void> {
  const { filePath, oldString, newString } = patch;

  // If the original was a file creation, revert = delete.
  if (!oldString) {
    try {
      const current = await fs.readFile(filePath, "utf-8");
      if (current !== newString) throw new Error(`Cannot revert hunk: ${filePath} has changed since its creation`);
      await fs.unlink(filePath);
    } catch (err: unknown) {
      if ((err as NodeJS.ErrnoException)?.code !== "ENOENT") throw err;
    }
    return;
  }

  const current = await fs.readFile(filePath, "utf-8");
  const next = replaceUnique(current, newString, oldString);
  await fs.writeFile(filePath, next, "utf-8");
}

/**
 * Replace the unique occurrence of `needle` with `replacement` in
 * `haystack`. Using `String.prototype.replace` would interpret `$&`,
 * `$1`, etc. as backreferences, which corrupts agent output containing
 * dollar signs — so we build the result manually.
 */
function replaceUnique(haystack: string, needle: string, replacement: string): string {
  const idx = haystack.indexOf(needle);
  // String-only edits carry no position. Repeated or empty matches cannot
  // identify which edit to undo; guessing can corrupt a different occurrence.
  if (idx < 0 || idx !== haystack.lastIndexOf(needle) || (!needle && haystack)) {
    throw new Error("Cannot locate a unique hunk preimage. The file has changed or the edit has no position; restore its checkpoint instead.");
  }
  return haystack.slice(0, idx) + replacement + haystack.slice(idx + needle.length);
}
