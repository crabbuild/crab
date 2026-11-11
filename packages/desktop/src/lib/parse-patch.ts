import { decodeGitPath } from "../../electron/git-path";

// Minimal unified-diff parser used by the Changes view's per-hunk
// toolbar. We only need the hunk header positions and the raw hunk
// body verbatim (the agent takes the body as-is and validates
// against the worktree), so this is deliberately strict and small:
//
//   - We handle standard git diff headers: `diff --git a/X b/Y`,
//     `--- a/X`, `+++ b/Y`, optional `index`/`mode`/`new file`/
//     `deleted file`/`similarity`/`rename from|to` lines.
//   - We handle the conventional `@@ -old_start[,old_lines]
//     +new_start[,new_lines] @@` header, including the "single-
//     line" form where the count is omitted.
//   - Binary and mode-only file headers remain in `files` with no
//     text hunks, allowing the caller to offer whole-file actions.
//
// The output bodies include trailing newlines and the standard
// `\ No newline at end of file` marker lines when git emits them,
// matching what the agent expects in `HunkAnchor.body`.

/** One hunk inside a parsed patch. */
export interface ParsedHunk {
  /** File path (the `b/<path>` side — the "new" filename). For
   *  rename hunks this is the destination path; the agent's staging
   *  verbs always key off the post-rename path. */
  path: string;
  /** Line number in the pre-image; 0 for pure-add into an empty file. */
  oldStart: number;
  /** Line count in the pre-image (0 for pure-add hunks). */
  oldLines: number;
  /** Line number in the post-image; 0 for pure-delete. */
  newStart: number;
  /** Line count in the post-image (0 for pure-delete hunks). */
  newLines: number;
  /** Raw hunk body — the lines between this `@@` header and the
   *  next `@@` / next `diff --git` / EOF, verbatim. Each line
   *  still carries its ` `/`+`/`-` prefix. Terminates with `\n`. */
  body: string;
  /** Zero-based index of this hunk among its file's hunks. Used by
   *  the renderer to key components stably across refreshes. */
  indexInFile: number;
  /** Zero-based index of this hunk in the whole patch. Global
   *  renderer key — survives file reorderings. */
  globalIndex: number;
}

/** Grouping convenience: all hunks for a single file. */
export interface ParsedFile {
  path: string;
  /** Old path when the hunk came from a rename; same as `path`
   *  otherwise. */
  oldPath: string;
  hunks: ParsedHunk[];
}

export interface ParsedPatch {
  files: ParsedFile[];
  hunks: ParsedHunk[];
}

const HUNK_HEADER_RE =
  /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;

/**
 * Parse a unified patch string into a flat list of hunks plus a
 * per-file grouping. Non-hunk sections (`diff --git`, `index`,
 * rename / mode metadata, binary markers) retain the file header
 * without inventing text hunks.
 *
 * The parser is deliberately lenient: anything it doesn't
 * understand between hunks is skipped, not thrown on. The agent
 * does the strict validation (anchor matches live content) and
 * renderer-side reporting is better off surfacing "no hunks" than
 * exploding on a weird patch.
 */
export function parsePatch(patch: string): ParsedPatch {
  const lines = patch.split("\n");
  const files: ParsedFile[] = [];
  const hunks: ParsedHunk[] = [];
  let currentFile: ParsedFile | null = null;
  let pendingOldPath: string | null = null;
  let pendingNewPath: string | null = null;

  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    if (line.startsWith("diff --git ")) {
      // Parse `diff --git a/<old> b/<new>`. Paths may be quoted when
      // they contain spaces; git emits the quoted form with
      // double-quotes around each side. We handle the common
      // unquoted case first and fall through to a best-effort quoted
      // parse otherwise.
      const parts = parseDiffGit(line);
      pendingOldPath = parts?.oldPath ?? null;
      pendingNewPath = parts?.newPath ?? null;
      currentFile = parts ? { path: parts.newPath, oldPath: parts.oldPath, hunks: [] } : null;
      if (currentFile) files.push(currentFile);
      i += 1;
      continue;
    }

    if (line.startsWith("--- ")) {
      const extracted = stripABPrefix(line.slice(4));
      if (extracted !== null) pendingOldPath = extracted;
      i += 1;
      continue;
    }

    if (line.startsWith("+++ ")) {
      const extracted = stripABPrefix(line.slice(4));
      if (extracted !== null) pendingNewPath = extracted;
      i += 1;
      continue;
    }

    if (line.startsWith("@@")) {
      const m = HUNK_HEADER_RE.exec(line);
      if (!m) {
        i += 1;
        continue;
      }
      // Prefer the `+++ b/<path>` we saw most recently; fall back
      // to the `diff --git` new side. An unknown path is a broken
      // patch and we skip this hunk.
      const path = pendingNewPath ?? pendingOldPath;
      if (path == null) {
        i += 1;
        continue;
      }
      if (!currentFile || currentFile.path !== path) {
        currentFile = {
          path,
          oldPath: pendingOldPath ?? path,
          hunks: [],
        };
        files.push(currentFile);
      }

      const oldStart = Number(m[1]);
      // When `,count` is absent git means "1 line" per the unified-
      // diff convention — *except* when the start itself is 0,
      // which only happens for pure-insert-into-empty-file (count
      // is also 0 in that case and git always emits it explicitly).
      const oldLines = m[2] !== undefined ? Number(m[2]) : 1;
      const newStart = Number(m[3]);
      const newLines = m[4] !== undefined ? Number(m[4]) : 1;

      // Body: lines from here to the next `@@`, next `diff --git`,
      // or EOF. Keep the trailing newline on each line (the input
      // was split on `\n`, so we re-add it).
      let j = i + 1;
      const bodyLines: string[] = [];
      while (j < lines.length) {
        const l = lines[j];
        if (l.startsWith("@@") || l.startsWith("diff --git ")) break;
        bodyLines.push(l);
        j += 1;
      }
      // `split("\n")` leaves a trailing empty element when the
      // input ends with `\n`; strip it so the body doesn't end
      // with a spurious blank line.
      if (bodyLines.length > 0 && bodyLines[bodyLines.length - 1] === "") {
        bodyLines.pop();
      }
      const body = bodyLines.length === 0 ? "" : `${bodyLines.join("\n")}\n`;

      const hunk: ParsedHunk = {
        path,
        oldStart,
        oldLines,
        newStart,
        newLines,
        body,
        indexInFile: currentFile.hunks.length,
        globalIndex: hunks.length,
      };
      currentFile.hunks.push(hunk);
      hunks.push(hunk);

      i = j;
      continue;
    }

    // Anything else (index lines, mode changes, binary markers,
    // stray blank lines) is ignored between headers.
    i += 1;
  }

  return { files, hunks };
}

/**
 * Build a [`HunkAnchor`](../ipc) payload from a parsed hunk. Thin
 * helper so call sites don't repeat the field mapping.
 */
export function hunkToAnchor(hunk: ParsedHunk): {
  path: string;
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  body: string;
} {
  return {
    path: hunk.path,
    old_start: hunk.oldStart,
    old_lines: hunk.oldLines,
    new_start: hunk.newStart,
    new_lines: hunk.newLines,
    body: hunk.body,
  };
}

/**
 * Synthesise a single-line hunk anchor from a parsed hunk plus a
 * chosen absolute line number + side. Used by the per-line discard
 * affordance: we want to revert exactly one `+` or `-` line without
 * the whole hunk. The resulting anchor keeps the original hunk's
 * old/new start/lines — `git apply --unidiff-zero` is tolerant of
 * body subsets as long as the anchor and body match the live
 * content (the agent's freshness check covers that).
 *
 * Returns `null` if the requested line is not present in the hunk
 * body (pre-condition mismatch).
 */
export function lineToDiscardAnchor(
  hunk: ParsedHunk,
  side: "add" | "del",
  absoluteLine: number,
): {
  path: string;
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  body: string;
} | null {
  const bodyLines = hunk.body.split("\n");
  // The body may end with a trailing "" from split; drop it so we
  // iterate only real content lines.
  if (bodyLines.length > 0 && bodyLines[bodyLines.length - 1] === "") {
    bodyLines.pop();
  }
  let oldCursor = hunk.oldStart;
  let newCursor = hunk.newStart;
  for (const bl of bodyLines) {
    if (bl.startsWith("\\ ")) {
      // `\ No newline at end of file` metadata, doesn't advance.
      continue;
    }
    const prefix = bl[0];
    if (prefix === "+") {
      if (side === "add" && newCursor === absoluteLine) {
        return {
          path: hunk.path,
          old_start: oldCursor,
          old_lines: 0,
          new_start: newCursor,
          new_lines: 1,
          body: `${bl}\n`,
        };
      }
      newCursor += 1;
    } else if (prefix === "-") {
      if (side === "del" && oldCursor === absoluteLine) {
        return {
          path: hunk.path,
          old_start: oldCursor,
          old_lines: 1,
          new_start: newCursor,
          new_lines: 0,
          body: `${bl}\n`,
        };
      }
      oldCursor += 1;
    } else if (prefix === " ") {
      oldCursor += 1;
      newCursor += 1;
    }
  }
  return null;
}

// --- helpers -----------------------------------------------------

/** Strip the `a/` or `b/` prefix git adds to diff filenames.
 *  Returns null for `/dev/null` (which means file creation/deletion
 *  sentinel — the caller should rely on the other side's path). */
function stripABPrefix(raw: string): string | null {
  let s = raw;
  // Strip the timestamp tail `\t<date>` that some diff producers
  // add. Git doesn't, but other tools do.
  const tab = s.indexOf("\t");
  if (tab !== -1) s = s.slice(0, tab);
  if (s === "/dev/null") return null;
  if (s.startsWith('"') && s.endsWith('"')) {
    const decoded = decodeGitPath(s);
    if (decoded === null) return null;
    s = decoded;
  }
  if (s.startsWith("a/")) return s.slice(2);
  if (s.startsWith("b/")) return s.slice(2);
  return s;
}

/**
 * Parse the paths out of a `diff --git a/… b/…` header. Handles:
 *   - plain paths: `diff --git a/foo b/foo`
 *   - quoted paths (spaces, unicode): `diff --git "a/f o" "b/f o"`
 *
 * Returns null when the line is malformed.
 */
function parseDiffGit(line: string): { oldPath: string; newPath: string } | null {
  const rest = line.slice("diff --git ".length);
  // Unquoted fast path: split on the first " b/" occurrence.
  if (!rest.startsWith('"')) {
    // Grab "a/... b/...". We can't just split on space because paths
    // might contain spaces (not common but possible). Use a lazy
    // regex that locks in `a/` at start and the ` b/` separator.
    const m = /^a\/(.+?) b\/(.+)$/.exec(rest);
    if (m) return { oldPath: m[1], newPath: m[2] };
    return null;
  }
  // Quoted path on the left. Find the matching closing quote,
  // accounting for git's `\"` escape sequence.
  const leftEnd = findQuotedEnd(rest, 0);
  if (leftEnd === -1) return null;
  const leftQuoted = rest.slice(0, leftEnd + 1);
  // Expect " " then either `b/…` or `"b/…"`.
  const after = rest.slice(leftEnd + 1).replace(/^ /, "");
  let rightQuoted = after;
  if (after.startsWith('"')) {
    const rightEnd = findQuotedEnd(after, 0);
    if (rightEnd === -1) return null;
    rightQuoted = after.slice(0, rightEnd + 1);
  }
  const oldPath = stripABPrefix(leftQuoted);
  const newPath = stripABPrefix(rightQuoted);
  if (oldPath == null || newPath == null) return null;
  return { oldPath, newPath };
}

/** Index of the closing `"` of a quoted git path starting at
 *  `start`. Returns -1 if unterminated. Handles `\"` escapes. */
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
