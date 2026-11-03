// Invariants for the discard-line counter used by the Changes
// view's confirm-before-discard dialog. We check:
//
//   1. Pure-add, pure-del, and mixed hunks each sum their `+`/`-`
//      lines.
//   2. `\ No newline at end of file` markers don't contribute.
//   3. Context lines (` ` prefix) don't contribute.
//   4. Multi-hunk inputs accumulate across hunks.
//   5. The threshold predicate fires strictly *above* the limit
//      ("more than N") and a non-positive limit disables the gate.
//
// Pattern matches `parse-patch.test.ts` / `file-state.test.ts`:
// plain node script with assert helpers; run via
// `scripts/discard-threshold-check.mjs`.

import {
  countDiscardedLines,
  shouldConfirmDiscard,
} from "./discard-threshold";

/* eslint-disable no-console */

function assertEq<T>(actual: T, expected: T, msg: string): void {
  if (actual !== expected) {
    console.error(
      `FAIL: ${msg}\n  expected: ${String(expected)}\n  actual:   ${String(actual)}`,
    );
    process.exit(1);
  }
}

// ---------- case 1: empty inputs ----------

{
  assertEq(countDiscardedLines([]), 0, "empty array → 0");
  assertEq(countDiscardedLines([{ body: "" }]), 0, "empty body → 0");
}

// ---------- case 2: pure-add / pure-del / mixed ----------

{
  const pureAdd = { body: "+one\n+two\n+three\n" };
  assertEq(countDiscardedLines([pureAdd]), 3, "pure-add counts all `+` lines");

  const pureDel = { body: "-one\n-two\n" };
  assertEq(countDiscardedLines([pureDel]), 2, "pure-del counts all `-` lines");

  const mixed = { body: " keep\n-drop\n+add\n keep2\n" };
  assertEq(countDiscardedLines([mixed]), 2, "mixed: context skipped, +/- counted");
}

// ---------- case 3: `\ No newline at end of file` is skipped ----------

{
  const withNoNewline = {
    body: "-old\n+new\n\\ No newline at end of file\n",
  };
  assertEq(
    countDiscardedLines([withNoNewline]),
    2,
    "no-newline marker doesn't contribute",
  );

  // A content line that happens to start with `\` is still a real
  // change because the actual prefix in the body is `+\` or `-\`.
  const escapedContent = { body: "+\\ tab-separated\n-\\ weird\n" };
  assertEq(
    countDiscardedLines([escapedContent]),
    2,
    "lines starting +\\ or -\\ are real content",
  );
}

// ---------- case 4: multi-hunk accumulation ----------

{
  const h1 = { body: "-a\n+A\n" };
  const h2 = { body: "-b\n-c\n+B\n+C\n" };
  const h3 = { body: " context only\n" };
  assertEq(
    countDiscardedLines([h1, h2, h3]),
    6,
    "multi-hunk sums across hunks",
  );
}

// ---------- case 5: threshold predicate ----------

{
  // Exactly-at-threshold is allowed — "more than N" in the spec
  // wording means strictly greater than.
  assertEq(shouldConfirmDiscard(40, 40), false, "at threshold → no confirm");
  assertEq(shouldConfirmDiscard(41, 40), true, "above threshold → confirm");
  assertEq(shouldConfirmDiscard(39, 40), false, "below threshold → no confirm");
  assertEq(shouldConfirmDiscard(0, 40), false, "zero lines → no confirm");
}

// ---------- case 6: non-positive threshold disables the gate ----------

{
  assertEq(shouldConfirmDiscard(1_000_000, 0), false, "0 threshold → disabled");
  assertEq(shouldConfirmDiscard(1_000_000, -1), false, "negative threshold → disabled");
  assertEq(
    shouldConfirmDiscard(10, Number.NaN),
    false,
    "NaN threshold → disabled (defensive)",
  );
  assertEq(
    shouldConfirmDiscard(10, Number.POSITIVE_INFINITY),
    false,
    "Infinity threshold → confirm never triggers (finite-only gate)",
  );
}

// ---------- case 7: blank lines don't contribute ----------

{
  // Git never emits a completely empty line inside a hunk body —
  // every line carries at least a space prefix — but the body
  // string ends with `\n`, so splitting produces a trailing "".
  // Our counter must not count that as a touched line.
  const trailing = { body: "-a\n+b\n" };
  assertEq(countDiscardedLines([trailing]), 2, "trailing blank doesn't count");
}

console.log("discard-threshold: OK");
