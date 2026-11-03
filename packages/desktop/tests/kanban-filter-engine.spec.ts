import { describe, it, expect } from "vitest";
import {
  filterCards,
  extractFilterOptions,
  fuzzyMatchOptions,
} from "../src/lib/kanban/filter-engine";
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

describe("filterCards", () => {
  it("returns all cards when no filters are active", () => {
    const cards = [makeCard({ id: "a" }), makeCard({ id: "b" })];
    const filters = { assignees: [], labels: [], priorities: [], pipelineStates: [] };
    expect(filterCards(cards, filters)).toEqual(cards);
  });

  it("filters by assignee (OR within category)", () => {
    const cards = [
      makeCard({ id: "a", assignee: "alice" }),
      makeCard({ id: "b", assignee: "bob" }),
      makeCard({ id: "c", assignee: "charlie" }),
    ];
    const filters = { assignees: ["alice", "bob"], labels: [], priorities: [], pipelineStates: [] };
    const result = filterCards(cards, filters);
    expect(result.map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("excludes cards with no assignee when assignee filter is active", () => {
    const cards = [
      makeCard({ id: "a", assignee: "alice" }),
      makeCard({ id: "b", assignee: undefined }),
    ];
    const filters = { assignees: ["alice"], labels: [], priorities: [], pipelineStates: [] };
    const result = filterCards(cards, filters);
    expect(result.map((c) => c.id)).toEqual(["a"]);
  });

  it("filters by labels (OR within category)", () => {
    const cards = [
      makeCard({ id: "a", labels: ["bug", "urgent"] }),
      makeCard({ id: "b", labels: ["feature"] }),
      makeCard({ id: "c", labels: ["bug"] }),
    ];
    const filters = { assignees: [], labels: ["bug"], priorities: [], pipelineStates: [] };
    const result = filterCards(cards, filters);
    expect(result.map((c) => c.id)).toEqual(["a", "c"]);
  });

  it("applies AND logic across filter types", () => {
    const cards = [
      makeCard({ id: "a", assignee: "alice", priority: "high" }),
      makeCard({ id: "b", assignee: "alice", priority: "low" }),
      makeCard({ id: "c", assignee: "bob", priority: "high" }),
    ];
    const filters = { assignees: ["alice"], labels: [], priorities: ["high"], pipelineStates: [] };
    const result = filterCards(cards, filters);
    expect(result.map((c) => c.id)).toEqual(["a"]);
  });

  it("returns empty array when no cards match", () => {
    const cards = [makeCard({ id: "a", priority: "low" })];
    const filters = { assignees: [], labels: [], priorities: ["high"], pipelineStates: [] };
    expect(filterCards(cards, filters)).toEqual([]);
  });
});

describe("extractFilterOptions", () => {
  it("extracts unique assignees, labels, priorities, and pipeline states", () => {
    const cards = [
      makeCard({ assignee: "alice", labels: ["bug", "urgent"], priority: "high", pipelineState: "idle" }),
      makeCard({ assignee: "bob", labels: ["bug"], priority: "medium", pipelineState: "done" }),
      makeCard({ assignee: "alice", labels: ["feature"], priority: "high", pipelineState: "idle" }),
    ];
    const options = extractFilterOptions(cards);
    expect(options.assignees).toEqual(["alice", "bob"]);
    expect(options.labels).toEqual(["bug", "feature", "urgent"]);
    expect(options.priorities).toContain("high");
    expect(options.priorities).toContain("medium");
    expect(options.pipelineStates).toContain("idle");
    expect(options.pipelineStates).toContain("done");
  });

  it("returns empty arrays for empty card set", () => {
    const options = extractFilterOptions([]);
    expect(options.assignees).toEqual([]);
    expect(options.labels).toEqual([]);
    expect(options.priorities).toEqual([]);
    expect(options.pipelineStates).toEqual([]);
  });

  it("excludes undefined assignees", () => {
    const cards = [makeCard({ assignee: undefined }), makeCard({ assignee: "alice" })];
    const options = extractFilterOptions(cards);
    expect(options.assignees).toEqual(["alice"]);
  });
});

describe("fuzzyMatchOptions", () => {
  it("returns all options when query is empty", () => {
    const options = ["alpha", "beta", "gamma"];
    expect(fuzzyMatchOptions("", options)).toEqual(options);
  });

  it("ranks exact prefix matches highest", () => {
    const options = ["react", "preact", "react-dom"];
    const result = fuzzyMatchOptions("react", options);
    expect(result[0]).toBe("react");
    expect(result[1]).toBe("react-dom");
  });

  it("matches case-insensitively", () => {
    const options = ["Alice", "Bob", "ALICE"];
    const result = fuzzyMatchOptions("ali", options);
    expect(result).toContain("Alice");
    expect(result).toContain("ALICE");
    expect(result).not.toContain("Bob");
  });

  it("supports subsequence matching", () => {
    const options = ["frontend", "backend", "fullstack"];
    const result = fuzzyMatchOptions("fnd", options);
    expect(result).toContain("frontend");
  });

  it("returns empty array when nothing matches", () => {
    const options = ["alpha", "beta", "gamma"];
    expect(fuzzyMatchOptions("xyz", options)).toEqual([]);
  });
});
