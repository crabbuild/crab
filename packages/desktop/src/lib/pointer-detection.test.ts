// Invariants for `isPointerPatch` — the classifier that decides
// whether a patch touches a Crab or LFS pointer blob. The
// Changes view uses the result to switch from per-hunk controls
// to a whole-file "Stage entire pointer change" card.
//
// We check:
//
//   1. Crab pointer patches classify as `{ isPointer: true,
//      kind: "crab" }` regardless of which side of the diff
//      carries the signature.
//   2. LFS pointer patches classify as `{ isPointer: true, kind:
//      "lfs" }`.
//   3. Plain-text patches, empty patches, and binary-only patches
//      classify as `{ isPointer: false, kind: null }`.
//   4. Both signatures present → Crab wins (migration-in-flight
//      case; documented in the module).
//   5. Case-insensitive matching survives unusual capitalisation.
//   6. `pointerKindLabel` returns the expected label strings.
//
// Pattern follows `file-state.test.ts`: plain node script, assert
// helpers, runs via `scripts/pointer-detection-check.mjs`.

import { isPointerPatch, pointerKindLabel } from "./pointer-detection";

/* eslint-disable no-console */

// @ts-expect-error Used below in test assertions
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

// ---------- case 1: Crab pointer patches ----------

{
  // Freshly-tracked file: the "old" side is empty, the "new" side
  // is the pointer blob introduced by `crab add`.
  const addPointer = [
    "diff --git a/model.bin b/model.bin",
    "new file mode 100644",
    "--- /dev/null",
    "+++ b/model.bin",
    "@@ -0,0 +1,3 @@",
    "+crab.dev/v1",
    "+size 12345678",
    "+hash blake3:abcd1234",
    "",
  ].join("\n");
  const r = isPointerPatch(addPointer);
  assertEq(r.isPointer, true, "crab add: isPointer=true");
  assertEq(r.kind, "crab", "crab add: kind=crab");
}

{
  // Swap-content case: both sides are pointers. The signature
  // appears in context (` `-prefixed) and in `-` / `+` lines.
  const swapPointer = [
    "diff --git a/weights.safetensors b/weights.safetensors",
    "--- a/weights.safetensors",
    "+++ b/weights.safetensors",
    "@@ -1,3 +1,3 @@",
    " crab.dev/v1",
    "-hash blake3:oldhash",
    "+hash blake3:newhash",
    " size 98765432",
    "",
  ].join("\n");
  const r = isPointerPatch(swapPointer);
  assertEq(r.isPointer, true, "crab swap: isPointer=true");
  assertEq(r.kind, "crab", "crab swap: kind=crab");
}

{
  // Deletion case: the pointer signature only appears on the `-`
  // side. Still counts as a pointer patch — the whole-file
  // affordance is the right UX.
  const deletePointer = [
    "diff --git a/stale.bin b/stale.bin",
    "deleted file mode 100644",
    "--- a/stale.bin",
    "+++ /dev/null",
    "@@ -1,2 +0,0 @@",
    "-crab.dev/v1",
    "-hash blake3:abcd",
    "",
  ].join("\n");
  const r = isPointerPatch(deletePointer);
  assertEq(r.isPointer, true, "crab delete: isPointer=true");
  assertEq(r.kind, "crab", "crab delete: kind=crab");
}

{
  // Alternate Crab signature form.
  const altCrab = [
    "diff --git a/f b/f",
    "--- a/f",
    "+++ b/f",
    "@@ -1 +1 @@",
    "-version crab v1",
    "+version crab v1",
    "",
  ].join("\n");
  const r = isPointerPatch(altCrab);
  assertEq(r.isPointer, true, "crab alt: isPointer=true");
  assertEq(r.kind, "crab", "crab alt: kind=crab");
}

// ---------- case 2: LFS pointer patches ----------

{
  const addLfs = [
    "diff --git a/dataset.bin b/dataset.bin",
    "new file mode 100644",
    "--- /dev/null",
    "+++ b/dataset.bin",
    "@@ -0,0 +1,3 @@",
    "+version https://git-lfs.github.com/spec/v1",
    "+oid sha256:0123456789abcdef",
    "+size 123456",
    "",
  ].join("\n");
  const r = isPointerPatch(addLfs);
  assertEq(r.isPointer, true, "lfs add: isPointer=true");
  assertEq(r.kind, "lfs", "lfs add: kind=lfs");
}

{
  // Future-proofing: spec URL may revise from `/spec/v1` to
  // `/spec/v2` or similar. The broad prefix signature still
  // classifies.
  const futureLfs = [
    "diff --git a/f b/f",
    "--- a/f",
    "+++ b/f",
    "@@ -1 +1 @@",
    "-version https://git-lfs.github.com/spec/v1",
    "+version https://git-lfs.github.com/spec/v2-draft",
    "",
  ].join("\n");
  const r = isPointerPatch(futureLfs);
  assertEq(r.isPointer, true, "lfs future: isPointer=true");
  assertEq(r.kind, "lfs", "lfs future: kind=lfs");
}

// ---------- case 3: non-pointer inputs ----------

{
  const plain = [
    "diff --git a/src/main.rs b/src/main.rs",
    "--- a/src/main.rs",
    "+++ b/src/main.rs",
    "@@ -1,3 +1,3 @@",
    " fn main() {",
    '-    println!("hello");',
    '+    println!("world");',
    " }",
    "",
  ].join("\n");
  const r = isPointerPatch(plain);
  assertEq(r.isPointer, false, "plain text: isPointer=false");
  assertEq(r.kind, null, "plain text: kind=null");
}

{
  const empty = "";
  const r = isPointerPatch(empty);
  assertEq(r.isPointer, false, "empty: isPointer=false");
  assertEq(r.kind, null, "empty: kind=null");
}

{
  const nullish = isPointerPatch(null);
  assertEq(nullish.isPointer, false, "null input: isPointer=false");
  assertEq(nullish.kind, null, "null input: kind=null");

  const undef = isPointerPatch(undefined);
  assertEq(undef.isPointer, false, "undefined input: isPointer=false");
  assertEq(undef.kind, null, "undefined input: kind=null");
}

{
  // Binary-only diff stanza — no hunk body at all, just the
  // "Binary files differ" marker. Should not be mistaken for a
  // pointer; the caller will fall through to the whole-file path
  // via other means if it's a binary.
  const binaryOnly = [
    "diff --git a/image.png b/image.png",
    "Binary files a/image.png and b/image.png differ",
    "",
  ].join("\n");
  const r = isPointerPatch(binaryOnly);
  assertEq(r.isPointer, false, "binary only: isPointer=false");
  assertEq(r.kind, null, "binary only: kind=null");
}

{
  // Prose mentioning pointer-ish words but not the signatures
  // themselves. Regression guard against overly-loose matching.
  const prose = [
    "diff --git a/README.md b/README.md",
    "--- a/README.md",
    "+++ b/README.md",
    "@@ -1 +1,3 @@",
    " # Project",
    "+",
    "+See the crab documentation for pointer details.",
    "",
  ].join("\n");
  const r = isPointerPatch(prose);
  assertEq(r.isPointer, false, "prose: isPointer=false");
  assertEq(r.kind, null, "prose: kind=null");
}

// ---------- case 4: conflict resolution (crab wins) ----------

{
  // Mid-migration patch that mentions both. The module contract
  // says crab takes precedence so the UI steers the user toward
  // the native format.
  const both = [
    "diff --git a/f b/f",
    "--- a/f",
    "+++ b/f",
    "@@ -1,2 +1,2 @@",
    "-version https://git-lfs.github.com/spec/v1",
    "+crab.dev/v1",
    "",
  ].join("\n");
  const r = isPointerPatch(both);
  assertEq(r.isPointer, true, "both signatures: isPointer=true");
  assertEq(r.kind, "crab", "both signatures: crab wins");
}

// ---------- case 5: case-insensitive matching ----------

{
  const shouty = [
    "diff --git a/f b/f",
    "--- a/f",
    "+++ b/f",
    "@@ -1 +1 @@",
    "-VERSION https://GIT-LFS.GITHUB.COM/spec/v1",
    "+VERSION https://GIT-LFS.GITHUB.COM/spec/v1",
    "",
  ].join("\n");
  const r = isPointerPatch(shouty);
  assertEq(r.isPointer, true, "case-insensitive LFS: isPointer=true");
  assertEq(r.kind, "lfs", "case-insensitive LFS: kind=lfs");
}

{
  const mixedCase = [
    "diff --git a/f b/f",
    "--- a/f",
    "+++ b/f",
    "@@ -1 +1 @@",
    "-GitioX.Dev/V1",
    "+GitioX.Dev/V1",
    "",
  ].join("\n");
  const r = isPointerPatch(mixedCase);
  assertEq(r.isPointer, true, "case-insensitive Crab: isPointer=true");
  assertEq(r.kind, "crab", "case-insensitive Crab: kind=crab");
}

// ---------- case 6: pointerKindLabel ----------

{
  assertEq(pointerKindLabel("crab"), "Crab pointer", "label: crab");
  assertEq(pointerKindLabel("lfs"), "LFS pointer", "label: lfs");
  assertEq(pointerKindLabel(null), "Pointer", "label: null fallback");
}

console.log("pointer-detection: OK");
