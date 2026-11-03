/**
 * Helpers for normalising Codex `file_change.diff` payloads into a
 * shape the rest of the desktop UI (`<SafePatchDiff />`, the parse
 * helpers in `parse-patch.ts`) can render correctly.
 *
 * Codex emits patches in three loosely-related shapes:
 *
 *   1. Full unified diff with `--- a/path`, `+++ b/path`, and `@@`
 *      headers. Already valid; no transformation needed.
 *
 *   2. Single-hunk patch fragment with just an `@@ -L,N +L,N @@` line
 *      followed by `+`/`-`/` ` rows. We synthesise the file header so
 *      the parser recognises it.
 *
 *   3. "Plus/minus" fragment with no headers at all — just lines
 *      prefixed with `+ `/`- ` /`  ` (or `+`/`-`/` ` without the
 *      space). Common in `kind: "update"` payloads. We synthesise
 *      both the file header and a fake `@@` hunk header that covers
 *      the entire fragment.
 *
 * For `kind: "add"` payloads where the body is a series of `+ ` rows
 * (the common output of `apply_patch` add-file blocks), we strip the
 * `+ ` prefix and surface the body as plain new-file content. Callers
 * use `extractAddedFileContent` for that.
 */

const HUNK_HEADER_RE = /^@@\s+-\d+(?:,\d+)?\s+\+\d+(?:,\d+)?\s+@@/;
const FILE_HEADER_RE = /^---\s/;
const DIFF_GIT_RE = /^diff --git\s/;

/**
 * Returns `true` if the body is already a recognisable unified diff
 * (has either a `diff --git`, `---`, or `@@` header).
 */
export function isUnifiedDiff(body: string): boolean {
  if (!body) return false;
  // Check the first ~5 non-blank lines. Codex sometimes prepends a
  // status banner like "Applying patch…" before the real header.
  const lines = body.split("\n");
  let seen = 0;
  for (const line of lines) {
    if (!line) continue;
    if (DIFF_GIT_RE.test(line) || FILE_HEADER_RE.test(line) || HUNK_HEADER_RE.test(line)) {
      return true;
    }
    if (++seen >= 5) break;
  }
  return false;
}

/**
 * Detects a "plus/minus" fragment: the body has at least one `+` or
 * `-` line and no full diff headers. Quick heuristic, not a parser.
 */
export function isPartialDiff(body: string): boolean {
  if (!body) return false;
  if (isUnifiedDiff(body)) return false;
  const lines = body.split("\n");
  let signed = 0;
  for (const line of lines) {
    if (!line) continue;
    const ch = line[0];
    if (ch === "+" || ch === "-") signed++;
    if (signed >= 1) return true;
  }
  return false;
}

/**
 * Build a synthetic unified diff from a partial fragment so the
 * existing `<SafePatchDiff />` renderer can highlight it. The
 * synthesised hunk header uses line numbers starting at 1 with the
 * counts derived from the fragment itself (which is good enough for
 * the renderer — it doesn't validate that line numbers match the
 * worktree).
 *
 * For `add` kind, the body is treated as pure additions; for
 * `delete`, pure deletions; for `update`, the existing `+`/`-`
 * markers are used verbatim.
 */
export function synthesizeUnifiedDiff(
  body: string,
  filePath: string,
  kind: "add" | "update" | "delete",
): string {
  if (isUnifiedDiff(body)) {
    // If the body already has a `diff --git` header, it's complete.
    // Otherwise, prepend the file header so PatchDiff can render it.
    if (DIFF_GIT_RE.test(body.split("\n")[0] ?? "")) return body;
    // Check if any line starts with `diff --git`
    const lines = body.split("\n");
    const hasDiffGit = lines.some((l) => DIFF_GIT_RE.test(l));
    if (hasDiffGit) return body;
    // Has @@ or --- but no diff --git — prepend the file header.
    const hasFileHeader = lines.some((l) => FILE_HEADER_RE.test(l));
    if (hasFileHeader) {
      return `diff --git a/${filePath} b/${filePath}\n${body}`;
    }
    // Only has @@ header — prepend full preamble.
    return [
      `diff --git a/${filePath} b/${filePath}`,
      `--- a/${filePath}`,
      `+++ b/${filePath}`,
      body,
    ].join("\n");
  }
  // Strip a leading "Applying patch…" banner some Codex builds emit.
  const cleaned = body.replace(/^Applying patch[^\n]*\n/, "");

  // Normalise the body so each line carries a +/-/space prefix.
  const normalised = normaliseFragmentLines(cleaned, kind);
  const oldLines = countPrefix(normalised, "-") + countPrefix(normalised, " ");
  const newLines = countPrefix(normalised, "+") + countPrefix(normalised, " ");

  const oldStart = oldLines === 0 ? 0 : 1;
  const newStart = newLines === 0 ? 0 : 1;

  const oldPart = oldLines === 1 ? String(oldStart) : `${oldStart},${oldLines}`;
  const newPart = newLines === 1 ? String(newStart) : `${newStart},${newLines}`;

  return [
    `diff --git a/${filePath} b/${filePath}`,
    `--- a/${filePath}`,
    `+++ b/${filePath}`,
    `@@ -${oldPart} +${newPart} @@`,
    normalised,
  ].join("\n");
}

/**
 * For `kind: "add"` payloads, the body is the new file contents.
 * Codex usually emits raw lines (no `+` prefix), but some agents emit
 * `+ <line>` style. This strips that prefix when present so the
 * caller can feed the body straight into a syntax highlighter.
 */
export function extractAddedFileContent(body: string): string {
  if (isUnifiedDiff(body)) {
    // Pull out the post-image lines from the unified diff.
    const out: string[] = [];
    let inHunk = false;
    for (const line of body.split("\n")) {
      if (HUNK_HEADER_RE.test(line)) {
        inHunk = true;
        continue;
      }
      if (!inHunk) continue;
      if (line.startsWith("+") && !line.startsWith("+++")) out.push(line.slice(1));
      else if (line.startsWith(" ")) out.push(line.slice(1));
    }
    return out.join("\n");
  }
  // Plus-prefixed fragment: strip leading `+ ` or `+`.
  const lines = body.split("\n");
  if (lines.every((l) => l === "" || l.startsWith("+"))) {
    return lines.map((l) => (l.startsWith("+ ") ? l.slice(2) : l.startsWith("+") ? l.slice(1) : l)).join("\n");
  }
  // No prefixes — return as-is.
  return body;
}

/**
 * Maps a file path's extension to a Shiki language identifier.
 * Returns `"plaintext"` for unknown extensions so the highlighter
 * still produces theme-coloured output (just without keyword
 * tokenisation).
 */
export function languageForPath(path: string): string {
  if (!path) return "plaintext";
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  const map: Record<string, string> = {
    ts: "typescript", tsx: "tsx",
    js: "javascript", jsx: "jsx", mjs: "javascript", cjs: "javascript",
    rs: "rust", py: "python", rb: "ruby", go: "go", zig: "zig",
    java: "java", kt: "kotlin", scala: "scala", groovy: "groovy",
    swift: "swift", m: "objective-c", mm: "objective-c",
    c: "c", h: "c", cpp: "cpp", cc: "cpp", cxx: "cpp", hpp: "cpp",
    cs: "csharp", fs: "fsharp",
    css: "css", scss: "scss", less: "less",
    html: "html", xml: "xml", svg: "xml", vue: "vue", svelte: "svelte", astro: "astro",
    json: "json", jsonc: "jsonc", yaml: "yaml", yml: "yaml", toml: "toml",
    md: "markdown", mdx: "markdown",
    sh: "bash", bash: "bash", zsh: "bash", fish: "fish",
    ps1: "powershell", bat: "bat", cmd: "bat",
    sql: "sql", graphql: "graphql", gql: "graphql",
    dockerfile: "dockerfile",
    tf: "hcl", hcl: "hcl",
    proto: "protobuf",
    ini: "ini", env: "dotenv",
    diff: "diff", patch: "diff",
    lua: "lua", php: "php", pl: "perl", r: "r", jl: "julia",
    ex: "elixir", exs: "elixir", erl: "erlang", clj: "clojure", cljs: "clojure",
    dart: "dart", makefile: "makefile", mk: "makefile", cmake: "cmake",
    csv: "csv", mermaid: "mermaid",
    tex: "latex", bib: "bibtex",
  };
  return map[ext] ?? "plaintext";
}

// ── Internal helpers ─────────────────────────────────────────────────

function countPrefix(body: string, prefix: string): number {
  let n = 0;
  for (const line of body.split("\n")) {
    if (line.startsWith(prefix)) n++;
  }
  return n;
}

function normaliseFragmentLines(
  body: string,
  kind: "add" | "update" | "delete",
): string {
  const out: string[] = [];
  for (const raw of body.split("\n")) {
    if (raw === "") {
      // Preserve blank lines inside the hunk as context lines so
      // the parser keeps them on both sides of the diff.
      out.push(" ");
      continue;
    }
    const ch = raw[0];
    if (ch === "+" || ch === "-") {
      // Already prefixed. Keep the marker and the rest of the line.
      out.push(raw);
    } else if (ch === " ") {
      out.push(raw);
    } else {
      // Unprefixed line — interpret per kind.
      if (kind === "add") out.push("+" + raw);
      else if (kind === "delete") out.push("-" + raw);
      else out.push(" " + raw);
    }
  }
  return out.join("\n");
}
