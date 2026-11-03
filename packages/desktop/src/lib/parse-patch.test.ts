// Invariants for the unified-patch parser used by the Changes
// view's per-hunk toolbar. We check:
//
//   1. Hunk headers are parsed with correct start / count values,
//      including the elided-count-of-one convention.
//   2. Multi-file patches produce one `ParsedFile` per file and
//      group hunks correctly.
//   3. Hunk bodies round-trip verbatim — the agent uses the body
//      as-is for `git apply`, so any rewrite here would land the
//      wrong patch.
//   4. `lineToDiscardAnchor` picks the exact line the user clicks
//      on, not a sibling.
//   5. Rename headers route to the new path (the agent's staging
//      verbs always key off the post-rename filename).
//
// Follow the `commit-graph.test.ts` pattern: a standalone node
// script that asserts and exits non-zero on failure. Runs under
// `node --experimental-strip-types` / `tsx` / or the companion
// script in `scripts/parse-patch-check.mjs`.

import {
  parsePatch,
  hunkToAnchor,
  lineToDiscardAnchor,
} from "./parse-patch";

/* eslint-disable no-console */

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    console.error("FAIL:", msg);
    process.exit(1);
  }
}

function assertEq<T>(actual: T, expected: T, msg: string): void {
  if (actual !== expected) {
    console.error(`FAIL: ${msg}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`);
    process.exit(1);
  }
}

// ---------- case 1: single-hunk, single-file patch ----------

const singleHunk = [
  "diff --git a/foo.txt b/foo.txt",
  "index abc..def 100644",
  "--- a/foo.txt",
  "+++ b/foo.txt",
  "@@ -1,3 +1,4 @@",
  " alpha",
  "-beta",
  "+BETA",
  "+gamma",
  " delta",
  "",
].join("\n");

{
  const p = parsePatch(singleHunk);
  assertEq(p.files.length, 1, "single-hunk: one file");
  assertEq(p.hunks.length, 1, "single-hunk: one hunk");
  const h = p.hunks[0];
  assertEq(h.path, "foo.txt", "single-hunk: path");
  assertEq(h.oldStart, 1, "single-hunk: oldStart");
  assertEq(h.oldLines, 3, "single-hunk: oldLines");
  assertEq(h.newStart, 1, "single-hunk: newStart");
  assertEq(h.newLines, 4, "single-hunk: newLines");
  // Body should match the lines between `@@` and EOF, verbatim.
  const expectedBody = " alpha\n-beta\n+BETA\n+gamma\n delta\n";
  assertEq(h.body, expectedBody, "single-hunk: body verbatim");
}

// ---------- case 2: count-of-one elided in header ----------

const oneLine = [
  "diff --git a/a.txt b/a.txt",
  "--- a/a.txt",
  "+++ b/a.txt",
  "@@ -5 +5 @@",
  "-old",
  "+new",
  "",
].join("\n");

{
  const p = parsePatch(oneLine);
  const h = p.hunks[0];
  assertEq(h.oldLines, 1, "one-line hunk: oldLines defaults to 1");
  assertEq(h.newLines, 1, "one-line hunk: newLines defaults to 1");
  assertEq(h.oldStart, 5, "one-line hunk: oldStart");
}

// ---------- case 3: multi-file patch ----------

const multi = [
  "diff --git a/a.txt b/a.txt",
  "--- a/a.txt",
  "+++ b/a.txt",
  "@@ -1 +1 @@",
  "-a1",
  "+A1",
  "diff --git a/b.txt b/b.txt",
  "--- a/b.txt",
  "+++ b/b.txt",
  "@@ -2,2 +2,2 @@",
  " keep",
  "-drop",
  "+KEEP",
  "@@ -10 +10 @@",
  "-x",
  "+X",
  "",
].join("\n");

{
  const p = parsePatch(multi);
  assertEq(p.files.length, 2, "multi: two files");
  assertEq(p.files[0].path, "a.txt", "multi: first file path");
  assertEq(p.files[1].path, "b.txt", "multi: second file path");
  assertEq(p.files[1].hunks.length, 2, "multi: b.txt has two hunks");
  assertEq(p.hunks.length, 3, "multi: 3 hunks global");
  assertEq(p.hunks[2].globalIndex, 2, "multi: globalIndex threaded");
  assertEq(p.hunks[2].indexInFile, 1, "multi: indexInFile resets per file");
}

// ---------- case 4: pure-add into empty file ----------

const pureAdd = [
  "diff --git a/new.txt b/new.txt",
  "new file mode 100644",
  "--- /dev/null",
  "+++ b/new.txt",
  "@@ -0,0 +1,2 @@",
  "+hello",
  "+world",
  "",
].join("\n");

{
  const p = parsePatch(pureAdd);
  assertEq(p.files.length, 1, "pureAdd: one file");
  const h = p.hunks[0];
  assertEq(h.path, "new.txt", "pureAdd: picks up +++ b/ path even with --- /dev/null");
  assertEq(h.oldStart, 0, "pureAdd: oldStart=0");
  assertEq(h.oldLines, 0, "pureAdd: oldLines=0");
  assertEq(h.newLines, 2, "pureAdd: newLines=2");
  assertEq(h.body, "+hello\n+world\n", "pureAdd: body");
}

// ---------- case 5: pure-delete ----------

const pureDel = [
  "diff --git a/gone.txt b/gone.txt",
  "deleted file mode 100644",
  "--- a/gone.txt",
  "+++ /dev/null",
  "@@ -1,2 +0,0 @@",
  "-one",
  "-two",
  "",
].join("\n");

{
  const p = parsePatch(pureDel);
  const h = p.hunks[0];
  // `+++ /dev/null` → fall back to `--- a/<path>` (oldPath).
  assertEq(h.path, "gone.txt", "pureDel: path falls back to old side");
  assertEq(h.newLines, 0, "pureDel: newLines=0");
}

// ---------- case 6: rename ----------

const rename = [
  "diff --git a/old.txt b/new.txt",
  "similarity index 90%",
  "rename from old.txt",
  "rename to new.txt",
  "--- a/old.txt",
  "+++ b/new.txt",
  "@@ -3 +3 @@",
  "-line3",
  "+LINE3",
  "",
].join("\n");

{
  const p = parsePatch(rename);
  assertEq(p.files[0].path, "new.txt", "rename: routes to new path");
  assertEq(p.files[0].oldPath, "old.txt", "rename: remembers old path");
  assertEq(p.hunks[0].path, "new.txt", "rename: hunk on new path");
}

// ---------- case 7: hunkToAnchor mapping ----------

{
  const p = parsePatch(singleHunk);
  const a = hunkToAnchor(p.hunks[0]);
  assertEq(a.path, "foo.txt", "anchor: path");
  assertEq(a.old_start, 1, "anchor: old_start");
  assertEq(a.old_lines, 3, "anchor: old_lines");
  assertEq(a.new_start, 1, "anchor: new_start");
  assertEq(a.new_lines, 4, "anchor: new_lines");
  // Body is byte-identical — this is the contract with the agent.
  assert(a.body === p.hunks[0].body, "anchor: body verbatim");
}

// ---------- case 8: lineToDiscardAnchor picks the clicked line ----------

{
  const p = parsePatch(singleHunk);
  const h = p.hunks[0];
  // `@@ -1,3 +1,4 @@` with body:
  //   ` alpha`   → new line 1, old line 1
  //   `-beta`    → old line 2
  //   `+BETA`    → new line 2
  //   `+gamma`   → new line 3
  //   ` delta`   → new line 4, old line 3
  const addGamma = lineToDiscardAnchor(h, "add", 3);
  assert(addGamma !== null, "lineToDiscardAnchor: add line 3 exists");
  assertEq(addGamma!.new_lines, 1, "lineToDiscardAnchor: single addition anchor");
  assertEq(addGamma!.old_lines, 0, "lineToDiscardAnchor: no old-side churn");
  assertEq(addGamma!.body, "+gamma\n", "lineToDiscardAnchor: body is the one line");

  const delBeta = lineToDiscardAnchor(h, "del", 2);
  assert(delBeta !== null, "lineToDiscardAnchor: del line 2 exists");
  assertEq(delBeta!.body, "-beta\n", "lineToDiscardAnchor: del body is the one line");

  const miss = lineToDiscardAnchor(h, "add", 99);
  assertEq(miss, null, "lineToDiscardAnchor: miss returns null");
}

// ---------- case 9: malformed input is tolerated ----------

{
  const p = parsePatch("totally not a patch\nnope\n");
  assertEq(p.files.length, 0, "malformed: no files");
  assertEq(p.hunks.length, 0, "malformed: no hunks");
}

// ---------- case 10: trailing `\ No newline at end of file` marker ----------

const noEof = [
  "diff --git a/n.txt b/n.txt",
  "--- a/n.txt",
  "+++ b/n.txt",
  "@@ -1,2 +1,2 @@",
  " keep",
  "-old",
  "+new",
  "\\ No newline at end of file",
  "",
].join("\n");

{
  const p = parsePatch(noEof);
  const h = p.hunks[0];
  assert(
    h.body.endsWith("\\ No newline at end of file\n"),
    "noEof: marker preserved in body",
  );
}

console.log("parse-patch: OK");
