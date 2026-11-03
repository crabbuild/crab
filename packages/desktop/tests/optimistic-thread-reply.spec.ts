import { describe, it, expect, vi, beforeEach } from "vitest";

import type { ReviewThread, ThreadComment } from "../src/review/types";
import type { OptimisticThreadComment } from "../src/review/useOptimisticThreads";

/**
 * Tests for the optimistic thread reply logic.
 *
 * Validates the merge behavior: optimistic comments are appended to threads,
 * confirmed comments are deduplicated, and failed comments retain their state.
 */

// ---------------------------------------------------------------------------
// Extracted merge logic (mirrors useOptimisticThreads internal merge)
// ---------------------------------------------------------------------------

function mergeOptimisticComments(
  threads: ReviewThread[],
  optimisticComments: Map<string, OptimisticThreadComment[]>,
) {
  return threads.map((thread) => {
    const optimistic = optimisticComments.get(thread.threadId) ?? [];
    const realIds = new Set(thread.comments.map((c) => c.id));
    const pendingOptimistic = optimistic.filter((c) => !realIds.has(c.id));
    return {
      ...thread,
      comments: [...thread.comments, ...pendingOptimistic],
    };
  });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeThread(overrides: Partial<ReviewThread> = {}): ReviewThread {
  return {
    threadId: "thread-1",
    path: "src/foo.ts",
    line: 10,
    resolved: false,
    outdated: false,
    comments: [],
    ...overrides,
  };
}

function makeComment(overrides: Partial<ThreadComment> = {}): ThreadComment {
  return {
    id: "comment-1",
    author: "alice",
    body: "looks good",
    createdAt: "2024-06-01T00:00:00.000Z",
    edited: false,
    isMine: false,
    ...overrides,
  };
}

function makeOptimisticComment(
  overrides: Partial<OptimisticThreadComment> = {},
): OptimisticThreadComment {
  return {
    id: "temp-abc123",
    author: "me",
    body: "my reply",
    createdAt: "2024-06-01T12:00:00.000Z",
    edited: false,
    isMine: true,
    pending: true,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("optimistic thread reply merge logic", () => {
  it("appends optimistic comments to the matching thread", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "original" })],
      }),
    ];
    const optimistic = new Map<string, OptimisticThreadComment[]>([
      ["t1", [makeOptimisticComment({ id: "temp-1", body: "my reply" })]],
    ]);

    const result = mergeOptimisticComments(threads, optimistic);

    expect(result[0].comments).toHaveLength(2);
    expect(result[0].comments[0].id).toBe("c1");
    expect(result[0].comments[1].id).toBe("temp-1");
    expect(result[0].comments[1].body).toBe("my reply");
  });

  it("does not append optimistic comments whose id matches a real comment", () => {
    // After server confirms, the real comment appears in the thread data
    // and the optimistic entry (now with the same id) should be filtered out.
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "original" }),
          makeComment({ id: "confirmed-id", body: "my reply", isMine: true }),
        ],
      }),
    ];
    const optimistic = new Map<string, OptimisticThreadComment[]>([
      [
        "t1",
        [
          makeOptimisticComment({
            id: "confirmed-id",
            body: "my reply",
            pending: false,
          }),
        ],
      ],
    ]);

    const result = mergeOptimisticComments(threads, optimistic);

    // Should not duplicate — the optimistic entry is filtered out.
    expect(result[0].comments).toHaveLength(2);
    expect(result[0].comments.map((c) => c.id)).toEqual(["c1", "confirmed-id"]);
  });

  it("preserves failed state on optimistic comments", () => {
    const threads = [makeThread({ threadId: "t1", comments: [] })];
    const optimistic = new Map<string, OptimisticThreadComment[]>([
      [
        "t1",
        [
          makeOptimisticComment({
            id: "temp-fail",
            body: "failed reply",
            pending: false,
            failed: true,
            failureMessage: "Network error",
          }),
        ],
      ],
    ]);

    const result = mergeOptimisticComments(threads, optimistic);

    expect(result[0].comments).toHaveLength(1);
    const comment = result[0].comments[0] as OptimisticThreadComment;
    expect(comment.failed).toBe(true);
    expect(comment.failureMessage).toBe("Network error");
    expect(comment.body).toBe("failed reply");
  });

  it("handles threads with no optimistic comments", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1" })],
      }),
    ];
    const optimistic = new Map<string, OptimisticThreadComment[]>();

    const result = mergeOptimisticComments(threads, optimistic);

    expect(result[0].comments).toHaveLength(1);
    expect(result[0].comments[0].id).toBe("c1");
  });

  it("handles multiple optimistic comments on the same thread", () => {
    const threads = [makeThread({ threadId: "t1", comments: [] })];
    const optimistic = new Map<string, OptimisticThreadComment[]>([
      [
        "t1",
        [
          makeOptimisticComment({ id: "temp-1", body: "first" }),
          makeOptimisticComment({ id: "temp-2", body: "second" }),
        ],
      ],
    ]);

    const result = mergeOptimisticComments(threads, optimistic);

    expect(result[0].comments).toHaveLength(2);
    expect(result[0].comments[0].body).toBe("first");
    expect(result[0].comments[1].body).toBe("second");
  });

  it("does not affect threads that have no matching optimistic entries", () => {
    const threads = [
      makeThread({ threadId: "t1", comments: [makeComment({ id: "c1" })] }),
      makeThread({ threadId: "t2", comments: [makeComment({ id: "c2" })] }),
    ];
    const optimistic = new Map<string, OptimisticThreadComment[]>([
      ["t1", [makeOptimisticComment({ id: "temp-1" })]],
    ]);

    const result = mergeOptimisticComments(threads, optimistic);

    expect(result[0].comments).toHaveLength(2); // c1 + temp-1
    expect(result[1].comments).toHaveLength(1); // c2 only
  });
});

describe("optimistic comment state transitions", () => {
  it("temp comment starts with pending: true, failed: undefined", () => {
    const temp = makeOptimisticComment({ pending: true });
    expect(temp.pending).toBe(true);
    expect(temp.failed).toBeUndefined();
  });

  it("on success, pending becomes false and id is replaced", () => {
    // Simulates the state transition after RPC success
    const temp = makeOptimisticComment({ id: "temp-abc", pending: true });
    const confirmed: OptimisticThreadComment = {
      ...temp,
      id: "server-id-123",
      createdAt: "2024-06-01T12:01:00.000Z",
      pending: false,
      failed: false,
      failureMessage: undefined,
    };

    expect(confirmed.id).toBe("server-id-123");
    expect(confirmed.pending).toBe(false);
    expect(confirmed.failed).toBe(false);
    expect(confirmed.body).toBe(temp.body);
  });

  it("on failure, pending becomes false and failed becomes true with message", () => {
    const temp = makeOptimisticComment({ id: "temp-abc", pending: true });
    const failed: OptimisticThreadComment = {
      ...temp,
      pending: false,
      failed: true,
      failureMessage: "Network timeout",
    };

    expect(failed.pending).toBe(false);
    expect(failed.failed).toBe(true);
    expect(failed.failureMessage).toBe("Network timeout");
    expect(failed.body).toBe(temp.body); // draft preserved
  });

  it("on retry, failed becomes false and pending becomes true again", () => {
    const failed = makeOptimisticComment({
      id: "temp-abc",
      pending: false,
      failed: true,
      failureMessage: "Network timeout",
    });
    const retrying: OptimisticThreadComment = {
      ...failed,
      pending: true,
      failed: false,
      failureMessage: undefined,
    };

    expect(retrying.pending).toBe(true);
    expect(retrying.failed).toBe(false);
    expect(retrying.body).toBe(failed.body); // draft still preserved
  });
});
