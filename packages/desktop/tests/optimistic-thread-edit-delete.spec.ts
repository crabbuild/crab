import { describe, it, expect } from "vitest";

import type { ReviewThread, ThreadComment } from "../src/review/types";
import type { OptimisticReviewThread, OptimisticThreadComment } from "../src/review/useOptimisticThreads";

/**
 * Tests for the optimistic edit/delete merge logic.
 *
 * Validates that optimistic edits update comment bodies and mark them as edited,
 * that optimistic deletes remove comments from threads, and that threads with
 * all comments deleted are removed entirely.
 */

// ---------------------------------------------------------------------------
// Extracted merge logic (mirrors useOptimisticThreads internal edit/delete merge)
// ---------------------------------------------------------------------------

interface OptimisticEditEntry {
  body: string;
  error?: string;
}

function mergeOptimisticEditsAndDeletes(
  threads: ReviewThread[],
  optimisticEdits: Map<string, OptimisticEditEntry>,
  optimisticDeletes: Set<string>,
): OptimisticReviewThread[] {
  return threads
    .map((thread) => {
      const comments: OptimisticThreadComment[] = thread.comments
        .filter((c) => !optimisticDeletes.has(c.id))
        .map((c) => {
          const edit = optimisticEdits.get(c.id);
          if (edit) {
            return { ...c, body: edit.body, edited: true };
          }
          return c;
        });

      return {
        ...thread,
        comments,
      };
    })
    .filter((thread) => thread.comments.length > 0);
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
    comments: [
      {
        id: "c1",
        author: "alice",
        body: "needs fix",
        createdAt: "2024-06-01T00:00:00.000Z",
        edited: false,
        isMine: true,
      },
    ],
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
    isMine: true,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests: Edit operations
// ---------------------------------------------------------------------------

describe("optimistic edit merge logic", () => {
  it("applies optimistic edit to a comment body and marks it as edited", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "original text", edited: false })],
      }),
    ];
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "updated text" }],
    ]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, new Set());

    expect(result[0].comments[0].body).toBe("updated text");
    expect(result[0].comments[0].edited).toBe(true);
  });

  it("preserves already-edited state when applying a new edit", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "previously edited", edited: true })],
      }),
    ];
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "edited again" }],
    ]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, new Set());

    expect(result[0].comments[0].body).toBe("edited again");
    expect(result[0].comments[0].edited).toBe(true);
  });

  it("does not affect comments without an optimistic edit", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "first" }),
          makeComment({ id: "c2", body: "second" }),
        ],
      }),
    ];
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "first edited" }],
    ]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, new Set());

    expect(result[0].comments[0].body).toBe("first edited");
    expect(result[0].comments[1].body).toBe("second");
    expect(result[0].comments[1].edited).toBe(false);
  });

  it("applies edits across multiple threads", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "thread 1 comment" })],
      }),
      makeThread({
        threadId: "t2",
        comments: [makeComment({ id: "c2", body: "thread 2 comment" })],
      }),
    ];
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "t1 edited" }],
      ["c2", { body: "t2 edited" }],
    ]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, new Set());

    expect(result[0].comments[0].body).toBe("t1 edited");
    expect(result[1].comments[0].body).toBe("t2 edited");
  });

  it("preserves all other comment properties when applying edit", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({
            id: "c1",
            author: "bob",
            body: "original",
            createdAt: "2024-01-01T00:00:00.000Z",
            isMine: true,
          }),
        ],
      }),
    ];
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "new body" }],
    ]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, new Set());

    expect(result[0].comments[0].author).toBe("bob");
    expect(result[0].comments[0].createdAt).toBe("2024-01-01T00:00:00.000Z");
    expect(result[0].comments[0].isMine).toBe(true);
    expect(result[0].comments[0].body).toBe("new body");
    expect(result[0].comments[0].edited).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Tests: Delete operations
// ---------------------------------------------------------------------------

describe("optimistic delete merge logic", () => {
  it("removes a deleted comment from the thread", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "first" }),
          makeComment({ id: "c2", body: "second" }),
        ],
      }),
    ];
    const deletes = new Set(["c1"]);

    const result = mergeOptimisticEditsAndDeletes(threads, new Map(), deletes);

    expect(result[0].comments).toHaveLength(1);
    expect(result[0].comments[0].id).toBe("c2");
  });

  it("removes the entire thread when the last comment is deleted", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "only comment" })],
      }),
    ];
    const deletes = new Set(["c1"]);

    const result = mergeOptimisticEditsAndDeletes(threads, new Map(), deletes);

    expect(result).toHaveLength(0);
  });

  it("removes the entire thread when all comments are deleted", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "first" }),
          makeComment({ id: "c2", body: "second" }),
        ],
      }),
    ];
    const deletes = new Set(["c1", "c2"]);

    const result = mergeOptimisticEditsAndDeletes(threads, new Map(), deletes);

    expect(result).toHaveLength(0);
  });

  it("does not affect other threads when deleting from one", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "t1 comment" })],
      }),
      makeThread({
        threadId: "t2",
        comments: [makeComment({ id: "c2", body: "t2 comment" })],
      }),
    ];
    const deletes = new Set(["c1"]);

    const result = mergeOptimisticEditsAndDeletes(threads, new Map(), deletes);

    expect(result).toHaveLength(1);
    expect(result[0].threadId).toBe("t2");
    expect(result[0].comments[0].id).toBe("c2");
  });

  it("handles deleting comments that do not exist in any thread", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "still here" })],
      }),
    ];
    const deletes = new Set(["nonexistent-id"]);

    const result = mergeOptimisticEditsAndDeletes(threads, new Map(), deletes);

    expect(result).toHaveLength(1);
    expect(result[0].comments).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Tests: Combined edit + delete operations
// ---------------------------------------------------------------------------

describe("combined optimistic edit and delete", () => {
  it("applies edit to one comment and deletes another in the same thread", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "to edit" }),
          makeComment({ id: "c2", body: "to delete" }),
        ],
      }),
    ];
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "edited" }],
    ]);
    const deletes = new Set(["c2"]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, deletes);

    expect(result[0].comments).toHaveLength(1);
    expect(result[0].comments[0].id).toBe("c1");
    expect(result[0].comments[0].body).toBe("edited");
    expect(result[0].comments[0].edited).toBe(true);
  });

  it("delete takes precedence over edit for the same comment", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "original" }),
          makeComment({ id: "c2", body: "keep" }),
        ],
      }),
    ];
    // Both edit and delete for c1 — delete should win (comment is removed).
    const edits = new Map<string, OptimisticEditEntry>([
      ["c1", { body: "edited but deleted" }],
    ]);
    const deletes = new Set(["c1"]);

    const result = mergeOptimisticEditsAndDeletes(threads, edits, deletes);

    expect(result[0].comments).toHaveLength(1);
    expect(result[0].comments[0].id).toBe("c2");
  });
});

// ---------------------------------------------------------------------------
// Tests: State transitions
// ---------------------------------------------------------------------------

describe("optimistic edit/delete state transitions", () => {
  it("edit flow: original → optimistic edit → confirmed (edit removed)", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "original" })],
      }),
    ];

    // Step 1: optimistic edit applied
    const duringEdit = mergeOptimisticEditsAndDeletes(
      threads,
      new Map([["c1", { body: "new body" }]]),
      new Set(),
    );
    expect(duringEdit[0].comments[0].body).toBe("new body");
    expect(duringEdit[0].comments[0].edited).toBe(true);

    // Step 2: RPC succeeds, edit removed — server data now has the new body
    const afterConfirm = mergeOptimisticEditsAndDeletes(
      [
        makeThread({
          threadId: "t1",
          comments: [makeComment({ id: "c1", body: "new body", edited: true })],
        }),
      ],
      new Map(),
      new Set(),
    );
    expect(afterConfirm[0].comments[0].body).toBe("new body");
    expect(afterConfirm[0].comments[0].edited).toBe(true);
  });

  it("edit flow with failure: original → optimistic edit → reverted (edit removed)", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [makeComment({ id: "c1", body: "original" })],
      }),
    ];

    // Step 1: optimistic edit applied
    const duringEdit = mergeOptimisticEditsAndDeletes(
      threads,
      new Map([["c1", { body: "new body" }]]),
      new Set(),
    );
    expect(duringEdit[0].comments[0].body).toBe("new body");

    // Step 2: RPC fails, edit removed — original data is restored
    const afterFailure = mergeOptimisticEditsAndDeletes(threads, new Map(), new Set());
    expect(afterFailure[0].comments[0].body).toBe("original");
    expect(afterFailure[0].comments[0].edited).toBe(false);
  });

  it("delete flow: present → optimistic delete → confirmed (stays deleted)", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "to delete" }),
          makeComment({ id: "c2", body: "keep" }),
        ],
      }),
    ];

    // Step 1: optimistic delete applied
    const duringDelete = mergeOptimisticEditsAndDeletes(
      threads,
      new Map(),
      new Set(["c1"]),
    );
    expect(duringDelete[0].comments).toHaveLength(1);
    expect(duringDelete[0].comments[0].id).toBe("c2");

    // Step 2: RPC succeeds — next data refresh won't include c1 at all
    const afterConfirm = mergeOptimisticEditsAndDeletes(
      [
        makeThread({
          threadId: "t1",
          comments: [makeComment({ id: "c2", body: "keep" })],
        }),
      ],
      new Map(),
      new Set(["c1"]), // still in set until cleanup
    );
    expect(afterConfirm[0].comments).toHaveLength(1);
    expect(afterConfirm[0].comments[0].id).toBe("c2");
  });

  it("delete flow with failure: present → optimistic delete → reverted (delete removed)", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        comments: [
          makeComment({ id: "c1", body: "still here" }),
          makeComment({ id: "c2", body: "keep" }),
        ],
      }),
    ];

    // Step 1: optimistic delete applied
    const duringDelete = mergeOptimisticEditsAndDeletes(
      threads,
      new Map(),
      new Set(["c1"]),
    );
    expect(duringDelete[0].comments).toHaveLength(1);

    // Step 2: RPC fails, delete removed — comment reappears
    const afterFailure = mergeOptimisticEditsAndDeletes(threads, new Map(), new Set());
    expect(afterFailure[0].comments).toHaveLength(2);
    expect(afterFailure[0].comments[0].id).toBe("c1");
  });
});
