import { describe, it, expect } from "vitest";
import { searchCards, findMatchRanges } from "../src/lib/kanban/search-engine";
import type { KanbanCardInfo } from "../src/ipc";

function makeCard(overrides: Partial<KanbanCardInfo> = {}): KanbanCardInfo {
  return {
    id: "card-1",
    source: "local",
    externalId: "",
    externalUrl: "",
    title: "Test card",
    description: "",
    columnId: "col-1",
    positionIndex: 0,
    priority: "medium",
    labels: [],
    pipelineState: "idle",
    iterationCount: 0,
    createdAt: Date.now(),
    updatedAt: Date.now(),
    ...overrides,
  };
}

describe("searchCards", () => {
  it("returns all cards when query is empty", () => {
    const cards = [makeCard({ id: "a" }), makeCard({ id: "b" })];
    expect(searchCards(cards, "")).toEqual(cards);
  });

  it("returns all cards when query is whitespace only", () => {
    const cards = [makeCard({ id: "a" }), makeCard({ id: "b" })];
    expect(searchCards(cards, "   ")).toEqual(cards);
  });

  it("matches case-insensitively on title", () => {
    const cards = [
      makeCard({ id: "a", title: "Fix Authentication Bug" }),
      makeCard({ id: "b", title: "Add new feature" }),
    ];
    const result = searchCards(cards, "auth");
    expect(result.map((c) => c.id)).toEqual(["a"]);
  });

  it("matches case-insensitively on description", () => {
    const cards = [
      makeCard({ id: "a", title: "Task A", description: "Refactor the login module" }),
      makeCard({ id: "b", title: "Task B", description: "Update docs" }),
    ];
    const result = searchCards(cards, "login");
    expect(result.map((c) => c.id)).toEqual(["a"]);
  });

  it("matches on either title or description", () => {
    const cards = [
      makeCard({ id: "a", title: "Login fix", description: "" }),
      makeCard({ id: "b", title: "Other", description: "login related" }),
      makeCard({ id: "c", title: "Unrelated", description: "nothing here" }),
    ];
    const result = searchCards(cards, "login");
    expect(result.map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("handles regex special characters in query safely", () => {
    const cards = [
      makeCard({ id: "a", title: "Fix bug (critical)" }),
      makeCard({ id: "b", title: "Normal task" }),
    ];
    const result = searchCards(cards, "(critical)");
    expect(result.map((c) => c.id)).toEqual(["a"]);
  });

  it("returns empty array when no cards match", () => {
    const cards = [makeCard({ id: "a", title: "Hello", description: "World" })];
    expect(searchCards(cards, "xyz")).toEqual([]);
  });

  it("handles query with dots and asterisks", () => {
    const cards = [
      makeCard({ id: "a", title: "file.txt upload" }),
      makeCard({ id: "b", title: "filetxt upload" }),
    ];
    const result = searchCards(cards, "file.txt");
    expect(result.map((c) => c.id)).toEqual(["a"]);
  });
});

describe("findMatchRanges", () => {
  it("returns empty array when query is empty", () => {
    expect(findMatchRanges("hello world", "")).toEqual([]);
  });

  it("returns empty array when text is empty", () => {
    expect(findMatchRanges("", "hello")).toEqual([]);
  });

  it("returns empty array when query is not found", () => {
    expect(findMatchRanges("hello world", "xyz")).toEqual([]);
  });

  it("finds a single match", () => {
    const ranges = findMatchRanges("hello world", "world");
    expect(ranges).toEqual([{ start: 6, end: 11 }]);
  });

  it("finds multiple non-overlapping matches", () => {
    const ranges = findMatchRanges("foo bar foo baz foo", "foo");
    expect(ranges).toEqual([
      { start: 0, end: 3 },
      { start: 8, end: 11 },
      { start: 16, end: 19 },
    ]);
  });

  it("matches case-insensitively", () => {
    const ranges = findMatchRanges("Hello HELLO hello", "hello");
    expect(ranges).toEqual([
      { start: 0, end: 5 },
      { start: 6, end: 11 },
      { start: 12, end: 17 },
    ]);
  });

  it("handles regex special characters in query", () => {
    const ranges = findMatchRanges("price is $10.00 (USD)", "$10.00");
    expect(ranges).toEqual([{ start: 9, end: 15 }]);
  });

  it("returns ranges within text bounds", () => {
    const text = "abcdef";
    const ranges = findMatchRanges(text, "cd");
    for (const range of ranges) {
      expect(range.start).toBeGreaterThanOrEqual(0);
      expect(range.end).toBeLessThanOrEqual(text.length);
      expect(range.start).toBeLessThan(range.end);
    }
  });

  it("returns non-overlapping ranges", () => {
    const ranges = findMatchRanges("aaaa", "aa");
    // "aa" at index 0 and 2 (non-overlapping due to global regex advancing)
    expect(ranges).toEqual([
      { start: 0, end: 2 },
      { start: 2, end: 4 },
    ]);
  });
});
