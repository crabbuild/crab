/**
 * Hunk-level apply / revert for the orchestrator diff viewer.
 *
 * Reads the file in the agent's worktree, performs an in-memory string
 * replacement for the `old_string` → `new_string` pair the Edit tool
 * reported, and writes the result back atomically. Revert does the
 * opposite: `new_string` → `old_string`.
 *
 * This is intentionally narrower than `git apply` — we don't need to
 * resolve a full patch, just to re-apply a single tool call's Edit or
 * roll it back. That keeps the surface tiny and avoids requiring a
 * staged index.
 */

import * as fs from "node:fs/promises";
import * as path from "node:path";

export interface HunkPatch {
  /** Absolute path (inside the worktree) of the file to patch. */
  filePath: string;
  oldString: string;
  newString: string;
}

/**
 * Apply a single edit to disk. If `oldString` is empty this creates the
 * file with `newString` as its content (matches the Write tool semantics).
 * Fails if the old content isn't found — callers should surface the
 * error so the user knows to re-run the diff.
 */
export async function applyHunk(patch: HunkPatch): Promise<void> {
  const { filePath, oldString, newString } = patch;

  // Write tool: no old_string means create-or-overwrite.
  if (!oldString) {
    await fs.mkdir(path.dirname(filePath), { recursive: true });
    await fs.writeFile(filePath, newString, "utf-8");
    return;
  }

  const current = await fs.readFile(filePath, "utf-8");
  if (!current.includes(oldString)) {
    throw new Error(
      `Cannot apply hunk: old content not found in ${filePath}. ` +
        "The file may have changed since the edit was proposed.",
    );
  }

  const next = replaceFirst(current, oldString, newString);
  await fs.writeFile(filePath, next, "utf-8");
}

/** Revert a single edit (swap new_string back to old_string). */
export async function revertHunk(patch: HunkPatch): Promise<void> {
  const { filePath, oldString, newString } = patch;

  // If the original was a file creation, revert = delete.
  if (!oldString) {
    try {
      await fs.unlink(filePath);
    } catch (err: unknown) {
      if ((err as NodeJS.ErrnoException)?.code !== "ENOENT") throw err;
    }
    return;
  }

  const current = await fs.readFile(filePath, "utf-8");
  if (!current.includes(newString)) {
    throw new Error(
      `Cannot revert hunk: new content not found in ${filePath}. ` +
        "The file may have been further modified.",
    );
  }

  const next = replaceFirst(current, newString, oldString);
  await fs.writeFile(filePath, next, "utf-8");
}

/**
 * Replace the first occurrence of `needle` with `replacement` in
 * `haystack`. Using `String.prototype.replace` would interpret `$&`,
 * `$1`, etc. as backreferences, which corrupts agent output containing
 * dollar signs — so we build the result manually.
 */
function replaceFirst(haystack: string, needle: string, replacement: string): string {
  const idx = haystack.indexOf(needle);
  if (idx < 0) return haystack;
  return haystack.slice(0, idx) + replacement + haystack.slice(idx + needle.length);
}
