import { decodeGitPath } from "../../electron/git-path";

// Tri-state checkbox derivation for the Changes view.
//
// Each file in the status list has three possible states relative
// to the index:
//
//   - `empty`   — nothing staged for this file. Worktree may be
//                 dirty, or the file may be untracked. Either way
//                 the staging progress bar is 0%.
//   - `full`    — everything is staged. Worktree is either clean
//                 vs the index (so `git diff` emits no patch for
//                 this path) or the file is fully new / deleted
//                 in the index and flushed to disk.
//   - `partial` — both sides report hunks: some are staged, some
//                 remain. The user has checked off a subset.
//
// The inputs are the *per-file* unified patches filtered out of
// the whole-repo `git diff` and `git diff --cached` outputs. We
// deliberately don't re-parse them here — any non-empty body for a
// given path is enough signal. Treating the patch as an opaque
// string is important for binary hunks too, which won't parse as
// text but still carry `diff --git` / `Binary files … differ`
// markers.
//
// Consumers: `ChangesView` batches `rpc.gitDiffIndexPatch` and
// `rpc.gitDiffCachedPatch` (both without a path filter), groups the
// output by file using `parsePatch`, and derives one state per
// file. Untracked files never appear in either diff; the caller
// is responsible for marking them `empty` up front (they become
// staged only via `git add`, which the higher-level "stage
// whole file" flow covers).

/** Possible tri-state values for a file checkbox. */
export type FileState = "empty" | "partial" | "full";

/**
 * Derive the tri-state label from the worktree-vs-index patch and
 * the index-vs-HEAD patch scoped to a single file.
 *
 * Both inputs may be empty strings — we treat `null` / `undefined`
 * / all-whitespace as "no patch for this file".
 *
 * The semantics mirror what the renderer shows the user:
 *
 *   - worktree empty, cached empty → `empty` (the file is clean or
 *     the caller is about to mark an untracked file, which is the
 *     same initial state as far as the UI is concerned).
 *   - worktree non-empty, cached empty → `empty`. Everything on
 *     disk is unstaged; nothing has been added to the index yet.
 *   - worktree empty, cached non-empty → `full`. Everything is
 *     already in the index.
 *   - both non-empty → `partial`. Some of the file is staged,
 *     some isn't.
 */
export function deriveFileState(
  workTreePatch: string | null | undefined,
  cachedPatch: string | null | undefined,
): FileState {
  const worktree = hasContent(workTreePatch);
  const cached = hasContent(cachedPatch);

  if (cached && worktree) return "partial";
  if (cached) return "full";
  // cached empty: worktree presence doesn't move us off `empty`.
  // A pending edit that the user hasn't touched the checkbox on
  // is still "nothing staged" as far as the tri-state goes.
  return "empty";
}

/**
 * Group a whole-repo unified patch into per-file patches. The keys
 * are the `b/<path>` side of the diff (post-rename), mirroring how
 * the agent's staging verbs address files.
 *
 * We scan the raw patch string rather than re-running the
 * `parsePatch` library here because state derivation only needs to
 * know whether each file has *any* patch body, not the hunk
 * details. That keeps the tri-state path independent of the hunk
 * parser's shape and lets us handle binary-only diffs (`Binary
 * files … differ`) without special-casing.
 *
 * Output: `{ "src/foo.ts": "diff --git a/src/foo.ts ...", ... }`.
 * Paths without a matching `diff --git` header are silently
 * skipped — they can't be addressed by staging verbs anyway.
 */
export function groupPatchByFile(patch: string | null | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  if (!patch) return out;
  const lines = patch.split("\n");
  let currentPath: string | null = null;
  let currentStart = 0;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (!line.startsWith("diff --git ")) continue;
    // Flush the previous file chunk.
    if (currentPath !== null) {
      out[currentPath] = sliceJoin(lines, currentStart, i);
    }
    currentPath = extractNewPath(line);
    currentStart = i;
  }
  if (currentPath !== null && currentStart < lines.length) {
    out[currentPath] = sliceJoin(lines, currentStart, lines.length);
  }
  return out;
}

/**
 * Map every file path mentioned in either diff (or in the untracked
 * / always-present set) to its tri-state. The extras argument lets
 * callers include files that have zero diff in both sides but
 * should still show `empty` (e.g. porcelain-reported untracked
 * files, which git-diff ignores).
 *
 * Untracked files are always `empty` by definition — you can't
 * have something staged but nothing on disk *and* a status entry.
 * Staging an untracked file promotes it to `full` by way of a
 * whole-file `git add`, which is the task-1.4 affordance.
 */
export function deriveFileStatesFromPatches(
  workTreePatch: string | null | undefined,
  cachedPatch: string | null | undefined,
  extras: Iterable<string> = [],
): Record<string, FileState> {
  const worktree = groupPatchByFile(workTreePatch);
  const cached = groupPatchByFile(cachedPatch);
  const paths = new Set<string>();
  for (const p of Object.keys(worktree)) paths.add(p);
  for (const p of Object.keys(cached)) paths.add(p);
  for (const p of extras) paths.add(p);
  const out: Record<string, FileState> = {};
  for (const p of paths) {
    out[p] = deriveFileState(worktree[p] ?? "", cached[p] ?? "");
  }
  return out;
}

// --- helpers ---------------------------------------------------------

function hasContent(s: string | null | undefined): boolean {
  if (s == null) return false;
  // Whitespace-only means no hunks. Use a cheap check — `trim()` on
  // potentially large patches is still O(n) but only ever runs
  // against the per-file slice.
  for (let i = 0; i < s.length; i += 1) {
    const ch = s.charCodeAt(i);
    if (ch !== 0x20 && ch !== 0x09 && ch !== 0x0a && ch !== 0x0d) return true;
  }
  return false;
}

function sliceJoin(lines: string[], start: number, end: number): string {
  if (start >= end) return "";
  const slice = lines.slice(start, end);
  // Re-attach a trailing newline so individual per-file patches
  // are well-formed inputs for anything that wants to re-parse
  // them (e.g. `parsePatch`). `split("\n")` dropped the terminator;
  // put it back when the original had it.
  return `${slice.join("\n")}\n`;
}

/**
 * Extract the `b/<path>` side of a `diff --git` header. Git emits
 * either the plain form `diff --git a/foo b/foo` or a quoted form
 * `diff --git "a/foo bar" "b/foo bar"` when the path has spaces /
 * non-ASCII bytes. We handle the plain path case inline and fall
 * back to a best-effort quoted parse so the tri-state derivation
 * doesn't misclassify files with awkward names.
 */
function extractNewPath(line: string): string | null {
  const rest = line.slice("diff --git ".length);
  if (!rest.startsWith('"')) {
    // Non-quoted form: `a/<old> b/<new>`. Match lazily so we don't
    // split on a space inside `<old>`.
    const m = /^a\/(.+?) b\/(.+)$/.exec(rest);
    if (m) return m[2];
    return null;
  }
  // Quoted left side; scan to its closing `"`.
  const leftEnd = findQuotedEnd(rest, 0);
  if (leftEnd === -1) return null;
  const after = rest.slice(leftEnd + 1).replace(/^ /, "");
  if (after.startsWith('"')) {
    const rightEnd = findQuotedEnd(after, 0);
    if (rightEnd === -1) return null;
    const decoded = decodeGitPath(after.slice(0, rightEnd + 1));
    return decoded === null ? null : stripB(decoded);
  }
  return stripB(after);
}

function stripB(s: string): string | null {
  if (s.startsWith("b/")) return s.slice(2);
  return s || null;
}


function findQuotedEnd(s: string, start: number): number {
  if (s[start] !== '"') return -1;
  let i = start + 1;
  while (i < s.length) {
    const ch = s[i];
    if (ch === "\\") {
      i += 2;
      continue;
    }
    if (ch === '"') return i;
    i += 1;
  }
  return -1;
}
