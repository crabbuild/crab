import { describe, it, expect } from "vitest";
import { groupCards, computeGroupFieldUpdate } from "../src/lib/kanban/group-engine";
import type { KanbanCardInfo, KanbanColumnInfo } from "../src/ipc";

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

const columns: KanbanColumnInfo[] = [
  { id: "col-1", name: "To Do", trackerStatusMapping: [] },
  { id: "col-2", name: "In Progress", trackerStatusMapping: [] },
  { id: "col-3", name: "Done", trackerStatusMapping: [] },
];

describe("groupCards", () => {
  describe("default mode", () => {
    it("returns original column structure with cards distributed", () => {
      const cards = [
        makeCard({ id: "a", columnId: "col-1" }),
        makeCard({ id: "b", columnId: "col-2" }),
        makeCard({ id: "c", columnId: "col-1" }),
      ];
      const result = groupCards(cards, columns, "default");
      expect(result).toHaveLength(3);
      expect(result[0].id).toBe("col-1");
      expect(result[0].name).toBe("To Do");
      expect(result[0].cards).toHaveLength(2);
      expect(result[1].cards).toHaveLength(1);
      expect(result[2].cards).toHaveLength(0);
    });
  });

  describe("priority mode", () => {
    it("groups cards by priority in correct order", () => {
      const cards = [
        makeCard({ id: "a", priority: "low" }),
        makeCard({ id: "b", priority: "critical" }),
        makeCard({ id: "c", priority: "high" }),
        makeCard({ id: "d", priority: "none" }),
        makeCard({ id: "e", priority: "medium" }),
      ];
      const result = groupCards(cards, columns, "priority");
      expect(result.map((c) => c.name)).toEqual([
        "Critical",
        "High",
        "Medium",
        "Low",
        "None",
      ]);
      expect(result[0].cards[0].id).toBe("b");
      expect(result[1].cards[0].id).toBe("c");
      expect(result[2].cards[0].id).toBe("e");
      expect(result[3].cards[0].id).toBe("a");
      expect(result[4].cards[0].id).toBe("d");
    });

    it("produces empty columns for unused priorities", () => {
      const cards = [makeCard({ id: "a", priority: "high" })];
      const result = groupCards(cards, columns, "priority");
      expect(result).toHaveLength(5);
      expect(result.find((c) => c.name === "Critical")!.cards).toHaveLength(0);
      expect(result.find((c) => c.name === "High")!.cards).toHaveLength(1);
    });
  });

  describe("assignee mode", () => {
    it("groups alphabetically with Unassigned last", () => {
      const cards = [
        makeCard({ id: "a", assignee: "Zara" }),
        makeCard({ id: "b", assignee: "Alice" }),
        makeCard({ id: "c", assignee: undefined }),
        makeCard({ id: "d", assignee: "Alice" }),
      ];
      const result = groupCards(cards, columns, "assignee");
      expect(result.map((c) => c.name)).toEqual([
        "Alice",
        "Zara",
        "Unassigned",
      ]);
      expect(result[0].cards).toHaveLength(2);
      expect(result[1].cards).toHaveLength(1);
      expect(result[2].cards).toHaveLength(1);
    });

    it("produces Unassigned column even when all cards are assigned", () => {
      const cards = [makeCard({ id: "a", assignee: "Bob" })];
      const result = groupCards(cards, columns, "assignee");
      expect(result[result.length - 1].name).toBe("Unassigned");
      expect(result[result.length - 1].cards).toHaveLength(0);
    });
  });

  describe("label mode", () => {
    it("groups by first label alphabetically with Unlabeled last", () => {
      const cards = [
        makeCard({ id: "a", labels: ["bug", "urgent"] }),
        makeCard({ id: "b", labels: ["feature"] }),
        makeCard({ id: "c", labels: [] }),
        makeCard({ id: "d", labels: ["bug"] }),
      ];
      const result = groupCards(cards, columns, "label");
      expect(result.map((c) => c.name)).toEqual([
        "bug",
        "feature",
        "Unlabeled",
      ]);
      expect(result[0].cards).toHaveLength(2);
      expect(result[1].cards).toHaveLength(1);
      expect(result[2].cards).toHaveLength(1);
    });

    it("produces Unlabeled column even when all cards have labels", () => {
      const cards = [makeCard({ id: "a", labels: ["fix"] })];
      const result = groupCards(cards, columns, "label");
      expect(result[result.length - 1].name).toBe("Unlabeled");
      expect(result[result.length - 1].cards).toHaveLength(0);
    });
  });
});

describe("computeGroupFieldUpdate", () => {
  const card = makeCard({ priority: "low", assignee: "Alice", labels: ["bug", "ui"] });

  it("returns columnId update for default mode", () => {
    const update = computeGroupFieldUpdate(card, "col-2", "default");
    expect(update).toEqual({ columnId: "col-2" });
  });

  it("returns priority update for priority mode", () => {
    const update = computeGroupFieldUpdate(card, "high", "priority");
    expect(update).toEqual({ priority: "high" });
  });

  it("returns assignee update for assignee mode", () => {
    const update = computeGroupFieldUpdate(card, "Bob", "assignee");
    expect(update).toEqual({ assignee: "Bob" });
  });

  it("clears assignee when dragged to Unassigned", () => {
    const update = computeGroupFieldUpdate(card, "__unassigned__", "assignee");
    expect(update).toEqual({ assignee: undefined });
  });

  it("replaces first label for label mode", () => {
    const update = computeGroupFieldUpdate(card, "feature", "label");
    expect(update).toEqual({ labels: ["feature", "ui"] });
  });

  it("sets label when card has no labels", () => {
    const unlabeled = makeCard({ labels: [] });
    const update = computeGroupFieldUpdate(unlabeled, "feature", "label");
    expect(update).toEqual({ labels: ["feature"] });
  });

  it("clears labels when dragged to Unlabeled", () => {
    const update = computeGroupFieldUpdate(card, "__unlabeled__", "label");
    expect(update).toEqual({ labels: [] });
  });
});
