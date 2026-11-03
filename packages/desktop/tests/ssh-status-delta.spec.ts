/**
 * Unit tests for the StatusDeltaTracker module.
 *
 * Tests hash computation, delta application, full response handling,
 * eviction, and router integration.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  StatusDeltaTracker,
  computeStatusHash,
  applyDeltaToEntries,
  requestWithDelta,
  type StatusEntry,
  type DeltaEntry,
  type DeltaResponse,
  type FullStatusResponse,
} from "../electron/ssh-status-delta";

describe("StatusDeltaTracker", () => {
  let tracker: StatusDeltaTracker;

  beforeEach(() => {
    tracker = new StatusDeltaTracker();
  });

  describe("getLastHash", () => {
    it("returns null for unknown repos", () => {
      expect(tracker.getLastHash("ssh://prof1/repo")).toBeNull();
    });

    it("returns the hash after processing a full response", () => {
      const entries: StatusEntry[] = [
        { status: "M ", path: "src/main.ts" },
        { status: "??", path: "new-file.txt" },
      ];
      const hash = computeStatusHash(entries);

      tracker.processResponse("ssh://prof1/repo", {
        hash,
        is_delta: false,
        entries,
      });

      expect(tracker.getLastHash("ssh://prof1/repo")).toBe(hash);
    });
  });

  describe("getLastEntries", () => {
    it("returns null for unknown repos", () => {
      expect(tracker.getLastEntries("ssh://prof1/repo")).toBeNull();
    });

    it("returns entries after processing a full response", () => {
      const entries: StatusEntry[] = [
        { status: "M ", path: "src/main.ts" },
      ];

      tracker.processResponse("ssh://prof1/repo", {
        hash: computeStatusHash(entries),
        is_delta: false,
        entries,
      });

      expect(tracker.getLastEntries("ssh://prof1/repo")).toEqual(entries);
    });
  });

  describe("processResponse — full response", () => {
    it("stores entries from a DeltaResponse with is_delta=false", () => {
      const entries: StatusEntry[] = [
        { status: "M ", path: "a.ts" },
        { status: " M", path: "b.ts" },
        { status: "??", path: "c.ts" },
      ];
      const hash = computeStatusHash(entries);

      const result = tracker.processResponse("repo1", {
        hash,
        is_delta: false,
        entries,
      });

      expect(result).toEqual(entries);
      expect(tracker.getLastHash("repo1")).toBe(hash);
    });

    it("computes hash for a plain FullStatusResponse (no hash field)", () => {
      const entries: StatusEntry[] = [
        { status: "A ", path: "new.ts" },
      ];

      const result = tracker.processResponse("repo1", { entries });

      expect(result).toEqual(entries);
      expect(tracker.getLastHash("repo1")).toBe(computeStatusHash(entries));
    });
  });

  describe("processResponse — delta response", () => {
    it("applies add deltas to reconstruct full status", () => {
      const baseEntries: StatusEntry[] = [
        { status: "M ", path: "existing.ts" },
      ];
      tracker.processResponse("repo1", {
        hash: computeStatusHash(baseEntries),
        is_delta: false,
        entries: baseEntries,
      });

      const newEntry: StatusEntry = { status: "??", path: "new-file.ts" };
      const expectedFull = [...baseEntries, newEntry];
      const newHash = computeStatusHash(expectedFull);

      const deltas: DeltaEntry[] = [
        { action: "add", path: "new-file.ts", entry: newEntry },
      ];

      const result = tracker.processResponse("repo1", {
        hash: newHash,
        is_delta: true,
        entries: deltas,
      });

      expect(result).toEqual(expectedFull);
      expect(tracker.getLastHash("repo1")).toBe(newHash);
    });

    it("applies remove deltas to reconstruct full status", () => {
      const baseEntries: StatusEntry[] = [
        { status: "M ", path: "a.ts" },
        { status: "??", path: "b.ts" },
        { status: " M", path: "c.ts" },
      ];
      tracker.processResponse("repo1", {
        hash: computeStatusHash(baseEntries),
        is_delta: false,
        entries: baseEntries,
      });

      const expectedFull: StatusEntry[] = [
        { status: "M ", path: "a.ts" },
        { status: " M", path: "c.ts" },
      ];
      const newHash = computeStatusHash(expectedFull);

      const deltas: DeltaEntry[] = [
        { action: "remove", path: "b.ts" },
      ];

      const result = tracker.processResponse("repo1", {
        hash: newHash,
        is_delta: true,
        entries: deltas,
      });

      expect(result).toEqual(expectedFull);
    });

    it("applies modify deltas to reconstruct full status", () => {
      const baseEntries: StatusEntry[] = [
        { status: "M ", path: "a.ts" },
        { status: "??", path: "b.ts" },
      ];
      tracker.processResponse("repo1", {
        hash: computeStatusHash(baseEntries),
        is_delta: false,
        entries: baseEntries,
      });

      const modifiedEntry: StatusEntry = { status: "A ", path: "b.ts" };
      const expectedFull: StatusEntry[] = [
        { status: "M ", path: "a.ts" },
        modifiedEntry,
      ];
      const newHash = computeStatusHash(expectedFull);

      const deltas: DeltaEntry[] = [
        { action: "modify", path: "b.ts", entry: modifiedEntry },
      ];

      const result = tracker.processResponse("repo1", {
        hash: newHash,
        is_delta: true,
        entries: deltas,
      });

      expect(result).toEqual(expectedFull);
    });

    it("applies mixed deltas (add + remove + modify)", () => {
      const baseEntries: StatusEntry[] = [
        { status: "M ", path: "keep.ts" },
        { status: "??", path: "remove-me.ts" },
        { status: " M", path: "modify-me.ts" },
      ];
      tracker.processResponse("repo1", {
        hash: computeStatusHash(baseEntries),
        is_delta: false,
        entries: baseEntries,
      });

      const modifiedEntry: StatusEntry = { status: "M ", path: "modify-me.ts" };
      const addedEntry: StatusEntry = { status: "A ", path: "added.ts" };

      const deltas: DeltaEntry[] = [
        { action: "remove", path: "remove-me.ts" },
        { action: "modify", path: "modify-me.ts", entry: modifiedEntry },
        { action: "add", path: "added.ts", entry: addedEntry },
      ];

      const expectedFull: StatusEntry[] = [
        { status: "M ", path: "keep.ts" },
        modifiedEntry,
        addedEntry,
      ];
      const newHash = computeStatusHash(expectedFull);

      const result = tracker.processResponse("repo1", {
        hash: newHash,
        is_delta: true,
        entries: deltas,
      });

      expect(result).toEqual(expectedFull);
    });

    it("returns empty array when delta is received without baseline", () => {
      const deltas: DeltaEntry[] = [
        { action: "add", path: "file.ts", entry: { status: "??", path: "file.ts" } },
      ];

      const result = tracker.processResponse("no-baseline", {
        hash: "abc123",
        is_delta: true,
        entries: deltas,
      });

      expect(result).toEqual([]);
    });
  });

  describe("clear", () => {
    it("removes tracked state for a specific repo", () => {
      const entries: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
      tracker.processResponse("repo1", { entries });
      tracker.processResponse("repo2", { entries });

      tracker.clear("repo1");

      expect(tracker.getLastHash("repo1")).toBeNull();
      expect(tracker.getLastHash("repo2")).not.toBeNull();
    });
  });

  describe("clearAll", () => {
    it("removes all tracked state", () => {
      const entries: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
      tracker.processResponse("repo1", { entries });
      tracker.processResponse("repo2", { entries });

      tracker.clearAll();

      expect(tracker.size).toBe(0);
      expect(tracker.getLastHash("repo1")).toBeNull();
      expect(tracker.getLastHash("repo2")).toBeNull();
    });
  });

  describe("eviction", () => {
    it("evicts oldest repo when at capacity", () => {
      vi.useFakeTimers();
      const smallTracker = new StatusDeltaTracker({ maxTrackedRepos: 3 });

      const entries: StatusEntry[] = [{ status: "M ", path: "a.ts" }];

      vi.setSystemTime(1000);
      smallTracker.processResponse("repo1", { entries });

      vi.setSystemTime(2000);
      smallTracker.processResponse("repo2", { entries });

      vi.setSystemTime(3000);
      smallTracker.processResponse("repo3", { entries });

      // Adding a 4th should evict repo1 (oldest)
      vi.setSystemTime(4000);
      smallTracker.processResponse("repo4", { entries });

      expect(smallTracker.size).toBe(3);
      expect(smallTracker.getLastHash("repo1")).toBeNull();
      expect(smallTracker.getLastHash("repo2")).not.toBeNull();
      expect(smallTracker.getLastHash("repo3")).not.toBeNull();
      expect(smallTracker.getLastHash("repo4")).not.toBeNull();

      vi.useRealTimers();
    });

    it("does not evict when updating an existing repo", () => {
      const smallTracker = new StatusDeltaTracker({ maxTrackedRepos: 2 });

      const entries1: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
      const entries2: StatusEntry[] = [{ status: "??", path: "b.ts" }];

      smallTracker.processResponse("repo1", { entries: entries1 });
      smallTracker.processResponse("repo2", { entries: entries2 });

      // Update repo1 — should not trigger eviction
      const updatedEntries: StatusEntry[] = [{ status: "A ", path: "c.ts" }];
      smallTracker.processResponse("repo1", { entries: updatedEntries });

      expect(smallTracker.size).toBe(2);
      expect(smallTracker.getLastHash("repo1")).not.toBeNull();
      expect(smallTracker.getLastHash("repo2")).not.toBeNull();
    });
  });

  describe("size", () => {
    it("returns the number of tracked repos", () => {
      expect(tracker.size).toBe(0);

      tracker.processResponse("repo1", { entries: [] });
      expect(tracker.size).toBe(1);

      tracker.processResponse("repo2", { entries: [] });
      expect(tracker.size).toBe(2);
    });
  });
});

describe("computeStatusHash", () => {
  it("produces a deterministic hash for the same entries", () => {
    const entries: StatusEntry[] = [
      { status: "M ", path: "src/main.ts" },
      { status: "??", path: "new-file.txt" },
    ];

    const hash1 = computeStatusHash(entries);
    const hash2 = computeStatusHash(entries);

    expect(hash1).toBe(hash2);
  });

  it("produces the same hash regardless of entry order", () => {
    const entries1: StatusEntry[] = [
      { status: "M ", path: "b.ts" },
      { status: "??", path: "a.ts" },
    ];
    const entries2: StatusEntry[] = [
      { status: "??", path: "a.ts" },
      { status: "M ", path: "b.ts" },
    ];

    expect(computeStatusHash(entries1)).toBe(computeStatusHash(entries2));
  });

  it("produces different hashes for different entries", () => {
    const entries1: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
    const entries2: StatusEntry[] = [{ status: "??", path: "a.ts" }];

    expect(computeStatusHash(entries1)).not.toBe(computeStatusHash(entries2));
  });

  it("produces different hashes for different paths", () => {
    const entries1: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
    const entries2: StatusEntry[] = [{ status: "M ", path: "b.ts" }];

    expect(computeStatusHash(entries1)).not.toBe(computeStatusHash(entries2));
  });

  it("handles empty entries", () => {
    const hash = computeStatusHash([]);
    expect(hash).toBeTruthy();
    expect(hash.length).toBe(64); // SHA-256 hex
  });

  it("includes origPath in hash computation for renames", () => {
    const withOrig: StatusEntry[] = [
      { status: "R ", path: "new-name.ts", origPath: "old-name.ts" },
    ];
    const withoutOrig: StatusEntry[] = [
      { status: "R ", path: "new-name.ts" },
    ];

    expect(computeStatusHash(withOrig)).not.toBe(computeStatusHash(withoutOrig));
  });

  it("returns a 64-character hex string (SHA-256)", () => {
    const hash = computeStatusHash([{ status: "M ", path: "file.ts" }]);
    expect(hash).toMatch(/^[0-9a-f]{64}$/);
  });
});

describe("applyDeltaToEntries", () => {
  it("adds new entries", () => {
    const base: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
    const deltas: DeltaEntry[] = [
      { action: "add", path: "b.ts", entry: { status: "??", path: "b.ts" } },
    ];

    const result = applyDeltaToEntries(base, deltas);

    expect(result).toHaveLength(2);
    expect(result.find((e) => e.path === "b.ts")).toEqual({ status: "??", path: "b.ts" });
  });

  it("removes entries", () => {
    const base: StatusEntry[] = [
      { status: "M ", path: "a.ts" },
      { status: "??", path: "b.ts" },
    ];
    const deltas: DeltaEntry[] = [{ action: "remove", path: "a.ts" }];

    const result = applyDeltaToEntries(base, deltas);

    expect(result).toHaveLength(1);
    expect(result[0].path).toBe("b.ts");
  });

  it("modifies entries", () => {
    const base: StatusEntry[] = [
      { status: "??", path: "a.ts" },
    ];
    const deltas: DeltaEntry[] = [
      { action: "modify", path: "a.ts", entry: { status: "A ", path: "a.ts" } },
    ];

    const result = applyDeltaToEntries(base, deltas);

    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ status: "A ", path: "a.ts" });
  });

  it("handles remove for non-existent path gracefully", () => {
    const base: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
    const deltas: DeltaEntry[] = [{ action: "remove", path: "nonexistent.ts" }];

    const result = applyDeltaToEntries(base, deltas);

    expect(result).toEqual(base);
  });

  it("handles empty base with add deltas", () => {
    const deltas: DeltaEntry[] = [
      { action: "add", path: "new.ts", entry: { status: "??", path: "new.ts" } },
    ];

    const result = applyDeltaToEntries([], deltas);

    expect(result).toHaveLength(1);
    expect(result[0]).toEqual({ status: "??", path: "new.ts" });
  });

  it("handles empty deltas (no changes)", () => {
    const base: StatusEntry[] = [
      { status: "M ", path: "a.ts" },
      { status: "??", path: "b.ts" },
    ];

    const result = applyDeltaToEntries(base, []);

    expect(result).toEqual(base);
  });

  it("does not mutate the base array", () => {
    const base: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
    const baseCopy = [...base];
    const deltas: DeltaEntry[] = [
      { action: "add", path: "b.ts", entry: { status: "??", path: "b.ts" } },
    ];

    applyDeltaToEntries(base, deltas);

    expect(base).toEqual(baseCopy);
  });
});

describe("requestWithDelta", () => {
  let tracker: StatusDeltaTracker;

  beforeEach(() => {
    tracker = new StatusDeltaTracker();
  });

  it("includes last_hash in request params", async () => {
    const entries: StatusEntry[] = [{ status: "M ", path: "a.ts" }];
    const hash = computeStatusHash(entries);

    // Seed the tracker with a baseline
    tracker.processResponse("repo1", { hash, is_delta: false, entries });

    const mockRouter = {
      route: vi.fn().mockResolvedValue({
        hash,
        is_delta: false,
        entries,
      }),
    };

    await requestWithDelta(
      mockRouter as any,
      tracker,
      "repo1",
      { repo_id: "repo1" },
    );

    expect(mockRouter.route).toHaveBeenCalledWith(
      "repo1",
      "git_status_porcelain",
      { repo_id: "repo1", last_hash: hash },
    );
  });

  it("sends null last_hash on first request", async () => {
    const entries: StatusEntry[] = [{ status: "M ", path: "a.ts" }];

    const mockRouter = {
      route: vi.fn().mockResolvedValue({ entries }),
    };

    await requestWithDelta(
      mockRouter as any,
      tracker,
      "repo1",
      { repo_id: "repo1" },
    );

    expect(mockRouter.route).toHaveBeenCalledWith(
      "repo1",
      "git_status_porcelain",
      { repo_id: "repo1", last_hash: null },
    );
  });

  it("processes a full response and stores state", async () => {
    const entries: StatusEntry[] = [
      { status: "M ", path: "a.ts" },
      { status: "??", path: "b.ts" },
    ];
    const hash = computeStatusHash(entries);

    const mockRouter = {
      route: vi.fn().mockResolvedValue({
        hash,
        is_delta: false,
        entries,
      }),
    };

    const result = await requestWithDelta(
      mockRouter as any,
      tracker,
      "repo1",
      { repo_id: "repo1" },
    );

    expect(result).toEqual(entries);
    expect(tracker.getLastHash("repo1")).toBe(hash);
  });

  it("processes a delta response and reconstructs full status", async () => {
    const baseEntries: StatusEntry[] = [
      { status: "M ", path: "a.ts" },
      { status: "??", path: "b.ts" },
    ];
    const baseHash = computeStatusHash(baseEntries);

    // Seed baseline
    tracker.processResponse("repo1", { hash: baseHash, is_delta: false, entries: baseEntries });

    const newEntry: StatusEntry = { status: "A ", path: "c.ts" };
    const expectedFull = [...baseEntries, newEntry];
    const newHash = computeStatusHash(expectedFull);

    const mockRouter = {
      route: vi.fn().mockResolvedValue({
        hash: newHash,
        is_delta: true,
        entries: [{ action: "add", path: "c.ts", entry: newEntry }],
      }),
    };

    const result = await requestWithDelta(
      mockRouter as any,
      tracker,
      "repo1",
      { repo_id: "repo1" },
    );

    expect(result).toEqual(expectedFull);
    expect(tracker.getLastHash("repo1")).toBe(newHash);
  });

  it("falls back to regular request on error", async () => {
    const entries: StatusEntry[] = [{ status: "M ", path: "a.ts" }];

    const mockRouter = {
      route: vi.fn()
        .mockRejectedValueOnce(new Error("Unknown param: last_hash"))
        .mockResolvedValueOnce({ entries }),
    };

    const result = await requestWithDelta(
      mockRouter as any,
      tracker,
      "repo1",
      { repo_id: "repo1" },
    );

    expect(result).toEqual(entries);
    // First call with last_hash, second without
    expect(mockRouter.route).toHaveBeenCalledTimes(2);
    expect(mockRouter.route).toHaveBeenNthCalledWith(
      2,
      "repo1",
      "git_status_porcelain",
      { repo_id: "repo1" },
    );
  });
});

describe("StatusDeltaTracker.computeHash (static)", () => {
  it("matches the standalone computeStatusHash function", () => {
    const entries: StatusEntry[] = [
      { status: "M ", path: "src/index.ts" },
      { status: "??", path: "README.md" },
    ];

    expect(StatusDeltaTracker.computeHash(entries)).toBe(computeStatusHash(entries));
  });
});
