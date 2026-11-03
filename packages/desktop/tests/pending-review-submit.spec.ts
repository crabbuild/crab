import { describe, it, expect } from "vitest";

import type { PendingComment, ForgeInlineComment } from "../src/review/types";

/**
 * Tests for the submitReview comment mapping logic.
 *
 * Validates that PendingComment[] is correctly mapped to ForgeInlineComment[]
 * for the forge API, especially the multi-line comment handling where the
 * forge expects `line` to be the end line and `start_line` to be the start line.
 */

// ---------------------------------------------------------------------------
// Extracted mapping logic (mirrors PendingReviewContext.submitReview)
// ---------------------------------------------------------------------------

function mapToForgeComments(comments: PendingComment[]): ForgeInlineComment[] {
  return comments.map((c) => ({
    path: c.path,
    line: c.endLine != null ? c.endLine : c.line,
    side: c.side,
    body: c.body,
    ...(c.endLine != null ? { start_line: c.line } : {}),
  }));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makePendingComment(overrides: Partial<PendingComment> = {}): PendingComment {
  return {
    id: crypto.randomUUID(),
    path: "src/foo.ts",
    line: 10,
    endLine: null,
    side: "RIGHT",
    body: "looks good",
    createdAt: "2024-06-01T00:00:00.000Z",
    outdated: false,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("submitReview comment mapping", () => {
  it("maps a single-line comment without start_line", () => {
    const comments = [makePendingComment({ path: "src/app.ts", line: 42, endLine: null })];
    const result = mapToForgeComments(comments);

    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      path: "src/app.ts",
      line: 42,
      side: "RIGHT",
      body: "looks good",
    });
    expect(result[0]).not.toHaveProperty("start_line");
  });

  it("maps a multi-line comment with start_line and line swapped", () => {
    const comments = [makePendingComment({ path: "src/app.ts", line: 10, endLine: 15 })];
    const result = mapToForgeComments(comments);

    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({
      path: "src/app.ts",
      line: 15,
      side: "RIGHT",
      body: "looks good",
      start_line: 10,
    });
  });

  it("preserves the side field for LEFT-side comments", () => {
    const comments = [makePendingComment({ side: "LEFT", line: 5, endLine: null })];
    const result = mapToForgeComments(comments);

    expect(result[0].side).toBe("LEFT");
  });

  it("maps multiple comments with mixed single and multi-line", () => {
    const comments = [
      makePendingComment({ path: "a.ts", line: 1, endLine: null, body: "single" }),
      makePendingComment({ path: "b.ts", line: 20, endLine: 30, body: "multi" }),
      makePendingComment({ path: "c.ts", line: 5, endLine: 5, body: "same-line range" }),
    ];
    const result = mapToForgeComments(comments);

    expect(result).toHaveLength(3);

    // Single-line: no start_line
    expect(result[0].line).toBe(1);
    expect(result[0]).not.toHaveProperty("start_line");

    // Multi-line: line = endLine, start_line = original line
    expect(result[1].line).toBe(30);
    expect(result[1].start_line).toBe(20);

    // endLine === line (non-null): still treated as multi-line by the mapping
    expect(result[2].line).toBe(5);
    expect(result[2].start_line).toBe(5);
  });

  it("maps an empty comment list to an empty array", () => {
    expect(mapToForgeComments([])).toEqual([]);
  });
});
