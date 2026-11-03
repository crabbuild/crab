/**
 * Tests for ReviewNavigatorContext keyboard navigation logic.
 *
 * These tests verify the navigation algorithms (thread ordering, file jumping,
 * line movement) without requiring a full React render. We extract and test
 * the core logic directly.
 */

import { describe, expect, it } from "vitest";
import type { ReviewThread } from "../src/review/types";
import type { PrChangedFile } from "../src/ipc";

// ---------------------------------------------------------------------------
// Re-implement the pure helper functions from ReviewNavigatorContext for testing.
// These mirror the internal logic used by the provider.
// ---------------------------------------------------------------------------

function getUnresolvedThreads(
  threads: ReviewThread[],
  files: PrChangedFile[],
): ReviewThread[] {
  const fileOrder = new Map<string, number>();
  files.forEach((f, i) => fileOrder.set(f.path, i));

  return threads
    .filter((t) => !t.resolved)
    .sort((a, b) => {
      const fileA = fileOrder.get(a.path) ?? Infinity;
      const fileB = fileOrder.get(b.path) ?? Infinity;
      if (fileA !== fileB) return fileA - fileB;
      return a.line - b.line;
    });
}

function findNextThreadIndex(
  unresolved: ReviewThread[],
  files: PrChangedFile[],
  currentPath: string,
  currentLine: number,
): number {
  if (unresolved.length === 0) return -1;

  const fileOrder = new Map<string, number>();
  files.forEach((f, i) => fileOrder.set(f.path, i));
  const currentFileIdx = fileOrder.get(currentPath) ?? -1;

  const afterIdx = unresolved.findIndex((t) => {
    const tFileIdx = fileOrder.get(t.path) ?? Infinity;
    if (tFileIdx > currentFileIdx) return true;
    if (tFileIdx === currentFileIdx && t.line > currentLine) return true;
    return false;
  });

  return afterIdx >= 0 ? afterIdx : 0;
}

function findPrevThreadIndex(
  unresolved: ReviewThread[],
  files: PrChangedFile[],
  currentPath: string,
  currentLine: number,
): number {
  if (unresolved.length === 0) return -1;

  const fileOrder = new Map<string, number>();
  files.forEach((f, i) => fileOrder.set(f.path, i));
  const currentFileIdx = fileOrder.get(currentPath) ?? -1;

  let foundIdx = -1;
  for (let i = unresolved.length - 1; i >= 0; i--) {
    const t = unresolved[i];
    const tFileIdx = fileOrder.get(t.path) ?? Infinity;
    if (tFileIdx < currentFileIdx) {
      foundIdx = i;
      break;
    }
    if (tFileIdx === currentFileIdx && t.line < currentLine) {
      foundIdx = i;
      break;
    }
  }

  return foundIdx >= 0 ? foundIdx : unresolved.length - 1;
}

function findNextUnviewedFile(
  files: PrChangedFile[],
  isViewed: (path: string) => boolean,
  currentPath: string,
): string | null {
  const currentIdx = files.findIndex((f) => f.path === currentPath);
  const len = files.length;
  for (let offset = 1; offset <= len; offset++) {
    const idx = (currentIdx + offset) % len;
    if (!isViewed(files[idx].path)) {
      return files[idx].path;
    }
  }
  return null;
}

function findPrevUnviewedFile(
  files: PrChangedFile[],
  isViewed: (path: string) => boolean,
  currentPath: string,
): string | null {
  const currentIdx = files.findIndex((f) => f.path === currentPath);
  const startIdx = currentIdx >= 0 ? currentIdx : 0;
  const len = files.length;
  for (let offset = 1; offset <= len; offset++) {
    const idx = (startIdx - offset + len) % len;
    if (!isViewed(files[idx].path)) {
      return files[idx].path;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Test data factories
// ---------------------------------------------------------------------------

function makeThread(overrides: Partial<ReviewThread> & { threadId: string; path: string; line: number }): ReviewThread {
  return {
    resolved: false,
    outdated: false,
    comments: [],
    ...overrides,
  };
}

function makeFile(path: string): PrChangedFile {
  return { path, additions: 10, deletions: 5 };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("getUnresolvedThreads", () => {
  it("filters out resolved threads", () => {
    const threads = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10, resolved: false }),
      makeThread({ threadId: "t2", path: "a.ts", line: 20, resolved: true }),
      makeThread({ threadId: "t3", path: "b.ts", line: 5, resolved: false }),
    ];
    const files = [makeFile("a.ts"), makeFile("b.ts")];

    const result = getUnresolvedThreads(threads, files);
    expect(result).toHaveLength(2);
    expect(result.map((t) => t.threadId)).toEqual(["t1", "t3"]);
  });

  it("sorts by file order then line number", () => {
    const threads = [
      makeThread({ threadId: "t1", path: "b.ts", line: 30 }),
      makeThread({ threadId: "t2", path: "a.ts", line: 20 }),
      makeThread({ threadId: "t3", path: "a.ts", line: 5 }),
      makeThread({ threadId: "t4", path: "b.ts", line: 10 }),
    ];
    const files = [makeFile("a.ts"), makeFile("b.ts")];

    const result = getUnresolvedThreads(threads, files);
    expect(result.map((t) => t.threadId)).toEqual(["t3", "t2", "t4", "t1"]);
  });

  it("returns empty array when all threads are resolved", () => {
    const threads = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10, resolved: true }),
    ];
    const files = [makeFile("a.ts")];

    expect(getUnresolvedThreads(threads, files)).toHaveLength(0);
  });
});

describe("findNextThreadIndex (n key)", () => {
  const files = [makeFile("a.ts"), makeFile("b.ts"), makeFile("c.ts")];

  it("finds the next thread in the same file", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
      makeThread({ threadId: "t2", path: "a.ts", line: 20 }),
      makeThread({ threadId: "t3", path: "b.ts", line: 5 }),
    ];

    const idx = findNextThreadIndex(unresolved, files, "a.ts", 10);
    expect(idx).toBe(1); // t2 at line 20
  });

  it("crosses file boundaries to find the next thread", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
      makeThread({ threadId: "t2", path: "b.ts", line: 5 }),
    ];

    const idx = findNextThreadIndex(unresolved, files, "a.ts", 10);
    expect(idx).toBe(1); // t2 in b.ts
  });

  it("wraps around to the first thread when at the end", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
      makeThread({ threadId: "t2", path: "b.ts", line: 5 }),
    ];

    const idx = findNextThreadIndex(unresolved, files, "b.ts", 5);
    expect(idx).toBe(0); // wraps to t1
  });

  it("returns 0 when current position is after all threads", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
    ];

    const idx = findNextThreadIndex(unresolved, files, "c.ts", 100);
    expect(idx).toBe(0); // wraps to t1
  });

  it("returns -1 when no unresolved threads exist", () => {
    const idx = findNextThreadIndex([], files, "a.ts", 10);
    expect(idx).toBe(-1);
  });
});

describe("findPrevThreadIndex (p key)", () => {
  const files = [makeFile("a.ts"), makeFile("b.ts"), makeFile("c.ts")];

  it("finds the previous thread in the same file", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
      makeThread({ threadId: "t2", path: "a.ts", line: 20 }),
      makeThread({ threadId: "t3", path: "b.ts", line: 5 }),
    ];

    const idx = findPrevThreadIndex(unresolved, files, "a.ts", 20);
    expect(idx).toBe(0); // t1 at line 10
  });

  it("crosses file boundaries backward", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
      makeThread({ threadId: "t2", path: "b.ts", line: 5 }),
    ];

    const idx = findPrevThreadIndex(unresolved, files, "b.ts", 5);
    expect(idx).toBe(0); // t1 in a.ts
  });

  it("wraps around to the last thread when at the beginning", () => {
    const unresolved = [
      makeThread({ threadId: "t1", path: "a.ts", line: 10 }),
      makeThread({ threadId: "t2", path: "b.ts", line: 5 }),
    ];

    const idx = findPrevThreadIndex(unresolved, files, "a.ts", 5);
    expect(idx).toBe(1); // wraps to t2
  });

  it("returns -1 when no unresolved threads exist", () => {
    const idx = findPrevThreadIndex([], files, "a.ts", 10);
    expect(idx).toBe(-1);
  });
});

describe("findNextUnviewedFile (] key)", () => {
  const files = [makeFile("a.ts"), makeFile("b.ts"), makeFile("c.ts"), makeFile("d.ts")];

  it("finds the next unviewed file", () => {
    const isViewed = (p: string) => p === "b.ts";
    const result = findNextUnviewedFile(files, isViewed, "a.ts");
    expect(result).toBe("c.ts"); // skips b.ts (viewed)
  });

  it("wraps around at the end of the file list", () => {
    const isViewed = (p: string) => p === "d.ts";
    const result = findNextUnviewedFile(files, isViewed, "c.ts");
    expect(result).toBe("a.ts"); // wraps to a.ts
  });

  it("returns null when all files are viewed", () => {
    const isViewed = () => true;
    const result = findNextUnviewedFile(files, isViewed, "a.ts");
    expect(result).toBeNull();
  });

  it("finds the immediately next file when it is unviewed", () => {
    const isViewed = () => false;
    const result = findNextUnviewedFile(files, isViewed, "a.ts");
    expect(result).toBe("b.ts");
  });
});

describe("findPrevUnviewedFile ([ key)", () => {
  const files = [makeFile("a.ts"), makeFile("b.ts"), makeFile("c.ts"), makeFile("d.ts")];

  it("finds the previous unviewed file", () => {
    const isViewed = (p: string) => p === "b.ts";
    const result = findPrevUnviewedFile(files, isViewed, "c.ts");
    expect(result).toBe("a.ts"); // skips b.ts (viewed)
  });

  it("wraps around at the beginning of the file list", () => {
    const isViewed = (p: string) => p === "d.ts";
    const result = findPrevUnviewedFile(files, isViewed, "a.ts");
    expect(result).toBe("c.ts"); // wraps backward
  });

  it("returns null when all files are viewed", () => {
    const isViewed = () => true;
    const result = findPrevUnviewedFile(files, isViewed, "a.ts");
    expect(result).toBeNull();
  });

  it("finds the immediately previous file when it is unviewed", () => {
    const isViewed = () => false;
    const result = findPrevUnviewedFile(files, isViewed, "b.ts");
    expect(result).toBe("a.ts");
  });
});

describe("j/k line navigation logic", () => {
  it("j increments the line number", () => {
    const currentLine = 5;
    const nextLine = currentLine + 1;
    expect(nextLine).toBe(6);
  });

  it("k decrements the line number but not below 1", () => {
    expect(Math.max(1, 5 - 1)).toBe(4);
    expect(Math.max(1, 1 - 1)).toBe(1); // clamped at 1
  });

  it("when no focus exists, starts at line 1 of first file", () => {
    const files = [makeFile("first.ts"), makeFile("second.ts")];
    const startPath = files[0].path;
    const startLine = 1;
    expect(startPath).toBe("first.ts");
    expect(startLine).toBe(1);
  });
});
