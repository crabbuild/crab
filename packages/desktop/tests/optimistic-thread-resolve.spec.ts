import { describe, it, expect } from "vitest";

import type { ReviewThread, ThreadComment } from "../src/review/types";
import type { OptimisticReviewThread } from "../src/review/useOptimisticThreads";

/**
 * Tests for the optimistic thread resolve/unresolve merge logic.
 *
 * Validates that optimistic resolve overrides are correctly applied to threads,
 * that errors are surfaced, and that the original state is preserved when no
 * override exists.
 */

// ---------------------------------------------------------------------------
// Extracted merge logic (mirrors useOptimisticThreads internal resolve merge)
// ---------------------------------------------------------------------------

interface OptimisticResolveEntry {
  resolved: boolean;
  error?: string;
}

function mergeOptimisticResolves(
  threads: ReviewThread[],
  optimisticResolves: Map<string, OptimisticResolveEntry>,
): OptimisticReviewThread[] {
  return threads.map((thread) => {
    const resolveOverride = optimisticResolves.get(thread.threadId);
    const resolved = resolveOverride ? resolveOverride.resolved : thread.resolved;
    const resolveError = resolveOverride?.error;

    return {
      ...thread,
      resolved,
      resolveError,
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
    comments: [
      {
        id: "c1",
        author: "alice",
        body: "needs fix",
        createdAt: "2024-06-01T00:00:00.000Z",
        edited: false,
        isMine: false,
      },
    ],
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("optimistic thread resolve merge logic", () => {
  it("applies optimistic resolved=true override to an unresolved thread", () => {
    const threads = [makeThread({ threadId: "t1", resolved: false })];
    const resolves = new Map<string, OptimisticResolveEntry>([
      ["t1", { resolved: true }],
    ]);

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].resolved).toBe(true);
    expect(result[0].resolveError).toBeUndefined();
  });

  it("applies optimistic resolved=false override to a resolved thread", () => {
    const threads = [makeThread({ threadId: "t1", resolved: true })];
    const resolves = new Map<string, OptimisticResolveEntry>([
      ["t1", { resolved: false }],
    ]);

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].resolved).toBe(false);
    expect(result[0].resolveError).toBeUndefined();
  });

  it("preserves original resolved state when no override exists", () => {
    const threads = [
      makeThread({ threadId: "t1", resolved: false }),
      makeThread({ threadId: "t2", resolved: true }),
    ];
    const resolves = new Map<string, OptimisticResolveEntry>();

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].resolved).toBe(false);
    expect(result[1].resolved).toBe(true);
    expect(result[0].resolveError).toBeUndefined();
    expect(result[1].resolveError).toBeUndefined();
  });

  it("surfaces error message on failed resolve attempt (reverted state)", () => {
    // Scenario: user tried to resolve (set true), RPC failed, state reverted to false with error.
    const threads = [makeThread({ threadId: "t1", resolved: false })];
    const resolves = new Map<string, OptimisticResolveEntry>([
      ["t1", { resolved: false, error: "Network error" }],
    ]);

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].resolved).toBe(false);
    expect(result[0].resolveError).toBe("Network error");
  });

  it("surfaces error message on failed unresolve attempt (reverted state)", () => {
    // Scenario: user tried to unresolve (set false), RPC failed, state reverted to true with error.
    const threads = [makeThread({ threadId: "t1", resolved: true })];
    const resolves = new Map<string, OptimisticResolveEntry>([
      ["t1", { resolved: true, error: "Permission denied" }],
    ]);

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].resolved).toBe(true);
    expect(result[0].resolveError).toBe("Permission denied");
  });

  it("only affects threads with matching overrides", () => {
    const threads = [
      makeThread({ threadId: "t1", resolved: false }),
      makeThread({ threadId: "t2", resolved: false }),
      makeThread({ threadId: "t3", resolved: true }),
    ];
    const resolves = new Map<string, OptimisticResolveEntry>([
      ["t2", { resolved: true }],
    ]);

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].resolved).toBe(false); // unchanged
    expect(result[1].resolved).toBe(true); // overridden
    expect(result[2].resolved).toBe(true); // unchanged
  });

  it("preserves all other thread properties when applying resolve override", () => {
    const threads = [
      makeThread({
        threadId: "t1",
        path: "src/bar.ts",
        line: 42,
        resolved: false,
        outdated: true,
        comments: [
          {
            id: "c1",
            author: "bob",
            body: "fix this",
            createdAt: "2024-01-01T00:00:00.000Z",
            edited: true,
            isMine: true,
          },
        ],
      }),
    ];
    const resolves = new Map<string, OptimisticResolveEntry>([
      ["t1", { resolved: true }],
    ]);

    const result = mergeOptimisticResolves(threads, resolves);

    expect(result[0].path).toBe("src/bar.ts");
    expect(result[0].line).toBe(42);
    expect(result[0].outdated).toBe(true);
    expect(result[0].comments).toHaveLength(1);
    expect(result[0].comments[0].author).toBe("bob");
    expect(result[0].resolved).toBe(true);
  });
});

describe("optimistic resolve state transitions", () => {
  it("resolve flow: unresolved → optimistic resolved → confirmed (override removed)", () => {
    const threads = [makeThread({ threadId: "t1", resolved: false })];

    // Step 1: optimistic resolve applied
    const duringResolve = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: true }]]),
    );
    expect(duringResolve[0].resolved).toBe(true);

    // Step 2: RPC succeeds, override removed — server data now has resolved: true
    const afterConfirm = mergeOptimisticResolves(
      [makeThread({ threadId: "t1", resolved: true })],
      new Map(),
    );
    expect(afterConfirm[0].resolved).toBe(true);
    expect(afterConfirm[0].resolveError).toBeUndefined();
  });

  it("resolve flow with failure: unresolved → optimistic resolved → reverted with error", () => {
    const threads = [makeThread({ threadId: "t1", resolved: false })];

    // Step 1: optimistic resolve applied
    const duringResolve = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: true }]]),
    );
    expect(duringResolve[0].resolved).toBe(true);

    // Step 2: RPC fails, reverted to original (false) with error
    const afterFailure = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: false, error: "API error" }]]),
    );
    expect(afterFailure[0].resolved).toBe(false);
    expect(afterFailure[0].resolveError).toBe("API error");
  });

  it("unresolve flow: resolved → optimistic unresolved → confirmed (override removed)", () => {
    const threads = [makeThread({ threadId: "t1", resolved: true })];

    // Step 1: optimistic unresolve applied
    const duringUnresolve = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: false }]]),
    );
    expect(duringUnresolve[0].resolved).toBe(false);

    // Step 2: RPC succeeds, override removed — server data now has resolved: false
    const afterConfirm = mergeOptimisticResolves(
      [makeThread({ threadId: "t1", resolved: false })],
      new Map(),
    );
    expect(afterConfirm[0].resolved).toBe(false);
    expect(afterConfirm[0].resolveError).toBeUndefined();
  });

  it("unresolve flow with failure: resolved → optimistic unresolved → reverted with error", () => {
    const threads = [makeThread({ threadId: "t1", resolved: true })];

    // Step 1: optimistic unresolve applied
    const duringUnresolve = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: false }]]),
    );
    expect(duringUnresolve[0].resolved).toBe(false);

    // Step 2: RPC fails, reverted to original (true) with error
    const afterFailure = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: true, error: "Server unavailable" }]]),
    );
    expect(afterFailure[0].resolved).toBe(true);
    expect(afterFailure[0].resolveError).toBe("Server unavailable");
  });

  it("dismiss error: after failure, removing override restores server state", () => {
    const threads = [makeThread({ threadId: "t1", resolved: false })];

    // Error state present
    const withError = mergeOptimisticResolves(
      threads,
      new Map([["t1", { resolved: false, error: "Timeout" }]]),
    );
    expect(withError[0].resolveError).toBe("Timeout");

    // After dismiss (override removed)
    const afterDismiss = mergeOptimisticResolves(threads, new Map());
    expect(afterDismiss[0].resolved).toBe(false);
    expect(afterDismiss[0].resolveError).toBeUndefined();
  });
});
