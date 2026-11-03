// Compute how many lines a discard operation will touch so the
// Changes view can decide whether to show a confirm dialog.
//
// Rationale: R-A1.3 wants a blocking prompt when a discard is
// "big" — the threshold is user-configurable (default 40). The
// one-click UX for small discards stays snappy, and a careless
// click that would nuke a whole file's worth of edits demands a
// second-level confirm.
//
// We count `+` and `-` prefixed lines inside each hunk body. Lines
// like `\ No newline at end of file` don't represent real
// additions / deletions and are skipped. Context lines (` `
// prefix) are also skipped — they're unchanged either way.
//
// This file is deliberately tiny and has no dependencies so the
// renderer can call it during the same tick as user input without
// worry.

/** Minimal shape needed from each hunk. Mirrors `ParsedHunk`'s
 *  `body` field so consumers can pass `ParsedHunk` values directly
 *  without a mapping step. */
export interface HunkBodyLike {
  body: string;
}

/**
 * Sum of touched (`+` or `-`) lines across every hunk body.
 *
 * `\ No newline at end of file` markers are skipped — they describe
 * the surrounding content, not a new change. Context lines (` `)
 * and blank lines are skipped too.
 *
 * Returns 0 for an empty input so callers can short-circuit the
 * threshold check when there's nothing to discard.
 */
export function countDiscardedLines(hunks: ReadonlyArray<HunkBodyLike>): number {
  let total = 0;
  for (const h of hunks) {
    if (!h.body) continue;
    // Split on `\n` — the body trailing newline (if any) produces a
    // final empty element that contributes nothing. Iterating
    // line-by-line avoids a full filter+length allocation.
    const lines = h.body.split("\n");
    for (const line of lines) {
      if (line.length === 0) continue;
      const prefix = line[0];
      if (prefix === "+" || prefix === "-") {
        // `\ No newline at end of file` is the only line that
        // starts with `\` inside a unified diff; treat it as
        // metadata. Any legitimate content line starting with
        // `\` would itself start with `+\` or `-\`.
        if (line.startsWith("\\ ")) continue;
        total += 1;
      }
    }
  }
  return total;
}

/**
 * Decide whether a confirm dialog should interrupt a discard.
 *
 *   - `threshold <= 0` disables the confirm entirely (useful for
 *     power users or tests).
 *   - Exactly-at-threshold is allowed to proceed; the dialog fires
 *     only when the count *exceeds* it, matching the spec wording
 *     ("more than N lines").
 */
export function shouldConfirmDiscard(
  lineCount: number,
  threshold: number,
): boolean {
  if (!Number.isFinite(threshold) || threshold <= 0) return false;
  return lineCount > threshold;
}
