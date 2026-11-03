// Invariants for tri-state derivation used by the Changes view
// file-level checkbox. We check:
//
//   1. The four interesting (worktree × cached) cases each map
//      to the expected label.
//   2. Whitespace-only patches count as "no content" — git -s
//      scans sometimes emit blank lines and we don't want those
//      to be mistaken for real hunks.
//   3. `groupPatchByFile` keys by the post-rename path (the
//      `b/<new>` side), matching what the agent's staging verbs
//      expect.
//   4. A multi-file patch groups correctly, including renames and
//      binary-only diff stanzas.
//   5. `deriveFileStatesFromPatches` unions paths across both
//      sides and the `extras` iterable.
//
// Pattern follows `parse-patch.test.ts`: plain node script with
// assert helpers; run via `scripts/file-state-check.mjs`.

import {
  deriveFileState,
  deriveFileStatesFromPatches,
  groupPatchByFile,
} from "./file-state";

/* eslint-disable no-console */

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    console.error("FAIL:", msg);
    process.exit(1);
  }
}

function assertEq<T>(actual: T, expected: T, msg: string): void {
  if (actual !== expected) {
    console.error(
      `FAIL: ${msg}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`,
    );
    process.exit(1);
  }
}

// ---------- case 1: each cell of the 2×2 truth table ----------

{
  assertEq(deriveFileState("", ""), "empty", "both empty → empty");
  assertEq(deriveFileState(null, null), "empty", "both null → empty");
  assertEq(
    deriveFileState("diff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n", ""),
    "empty",
    "worktree only → empty (nothing staged yet)",
  );
  assertEq(
    deriveFileState("", "diff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n"),
    "full",
    "cached only → full (everything staged)",
  );
  assertEq(
    deriveFileState(
      "diff --git a/f b/f\n@@ -3 +3 @@\n-x\n+y\n",
      "diff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n",
    ),
    "partial",
    "both non-empty → partial",
  );
}

// ---------- case 2: whitespace-only inputs count as empty ----------

{
  assertEq(deriveFileState("   \n\t\n", ""), "empty", "whitespace worktree → empty");
  assertEq(deriveFileState("", "\n \n"), "empty", "whitespace cached → empty");
}

// ---------- case 3: groupPatchByFile picks the b/ side ----------

const renamePatch = [
  "diff --git a/old.txt b/new.txt",
  "similarity index 95%",
  "rename from old.txt",
  "rename to new.txt",
  "index abc..def 100644",
  "--- a/old.txt",
  "+++ b/new.txt",
  "@@ -3 +3 @@",
  "-line3",
  "+LINE3",
  "",
].join("\n");

{
  const g = groupPatchByFile(renamePatch);
  const paths = Object.keys(g);
  assertEq(paths.length, 1, "rename: one file");
  assertEq(paths[0], "new.txt", "rename: keyed by new path");
  assert(g["new.txt"].includes("--- a/old.txt"), "rename: slice includes old path line");
  assert(g["new.txt"].includes("LINE3"), "rename: slice includes hunk body");
}

// ---------- case 4: multi-file patch groups correctly ----------

const multi = [
  "diff --git a/a.txt b/a.txt",
  "--- a/a.txt",
  "+++ b/a.txt",
  "@@ -1 +1 @@",
  "-a1",
  "+A1",
  "diff --git a/bin.bin b/bin.bin",
  "Binary files a/bin.bin and b/bin.bin differ",
  "diff --git a/deep/nested.ts b/deep/nested.ts",
  "--- a/deep/nested.ts",
  "+++ b/deep/nested.ts",
  "@@ -10,2 +10,2 @@",
  " keep",
  "-drop",
  "+KEEP",
  "",
].join("\n");

{
  const g = groupPatchByFile(multi);
  const keys = Object.keys(g).sort();
  assertEq(keys.length, 3, "multi: three files");
  assertEq(keys[0], "a.txt", "multi: includes a.txt");
  assertEq(keys[1], "bin.bin", "multi: includes binary-only file");
  assertEq(keys[2], "deep/nested.ts", "multi: includes nested path");
  assert(
    g["bin.bin"].includes("Binary files a/bin.bin"),
    "multi: binary slice kept verbatim",
  );
  assert(
    g["deep/nested.ts"].includes("KEEP"),
    "multi: nested slice includes trailing hunk body",
  );
  // No stanza should leak into a neighbour.
  assert(!g["a.txt"].includes("Binary files"), "multi: no cross-file leak (binary)");
  assert(!g["a.txt"].includes("KEEP"), "multi: no cross-file leak (nested)");
}

// ---------- case 5: deriveFileStatesFromPatches unions paths ----------

{
  const worktree = [
    "diff --git a/a.txt b/a.txt",
    "--- a/a.txt",
    "+++ b/a.txt",
    "@@ -1 +1 @@",
    "-a",
    "+A",
    "",
  ].join("\n");
  const cached = [
    "diff --git a/b.txt b/b.txt",
    "--- a/b.txt",
    "+++ b/b.txt",
    "@@ -1 +1 @@",
    "-b",
    "+B",
    "diff --git a/a.txt b/a.txt",
    "--- a/a.txt",
    "+++ b/a.txt",
    "@@ -5 +5 @@",
    "-x",
    "+X",
    "",
  ].join("\n");

  const states = deriveFileStatesFromPatches(worktree, cached, ["untracked.md"]);
  assertEq(states["a.txt"], "partial", "union: a.txt present in both → partial");
  assertEq(states["b.txt"], "full", "union: b.txt only staged → full");
  assertEq(states["untracked.md"], "empty", "union: untracked extra → empty");
  assertEq(Object.keys(states).length, 3, "union: exactly three files");
}

// ---------- case 6: empty inputs produce empty extras-only map ----------

{
  const states = deriveFileStatesFromPatches("", "", ["note.md"]);
  assertEq(states["note.md"], "empty", "extras-only: note.md defaults to empty");
  assertEq(Object.keys(states).length, 1, "extras-only: single entry");
}

// ---------- case 7: deriveFileState is symmetric in direction only when agreed ----------

{
  // Regression guard: the order of arguments matters. Worktree-only
  // means unstaged (empty); cached-only means fully staged (full).
  // These should not be equal — catching a future refactor that
  // accidentally swaps the inputs.
  const a = deriveFileState("diff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n", "");
  const b = deriveFileState("", "diff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n");
  assert(a !== b, "direction matters: worktree-only vs cached-only differ");
}

// ---------- property-ish: binary-only diff still produces one file ----------

{
  const binaryOnly = [
    "diff --git a/image.png b/image.png",
    "Binary files a/image.png and b/image.png differ",
    "",
  ].join("\n");
  const g = groupPatchByFile(binaryOnly);
  assertEq(Object.keys(g).length, 1, "binary-only: one file");
  assertEq(
    deriveFileState(binaryOnly, ""),
    "empty",
    "binary-only in worktree only → empty",
  );
  assertEq(
    deriveFileState("", binaryOnly),
    "full",
    "binary-only in cached only → full",
  );
  assertEq(
    deriveFileState(binaryOnly, binaryOnly),
    "partial",
    "binary-only on both sides → partial",
  );
}

console.log("file-state: OK");
