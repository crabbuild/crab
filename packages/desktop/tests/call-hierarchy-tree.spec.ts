/**
 * Tests for the call hierarchy tree helpers.
 *
 * Verifies:
 * - path-based node updates preserve structural sharing
 * - cycle detection drops nodes whose key appears in ancestor chain
 * - item key generation is deterministic
 */

import { describe, it, expect } from "vitest";
import {
  callHierarchyItemKey,
  mapCallHierarchyTree,
  dedupCallHierarchyCycles,
  type CallHierarchyNode,
} from "../src/lib/call-hierarchy-tree";
import type { LspCallHierarchyItem } from "../src/lib/lsp-types";

// ── Fixtures ───────────────────────────────────────────────────────

function makeItem(overrides: Partial<LspCallHierarchyItem> = {}): LspCallHierarchyItem {
  return {
    name: "foo",
    kind: 12,
    uri: "file:///test.ts",
    range: {
      start: { line: 0, character: 0 },
      end: { line: 0, character: 3 },
    },
    selectionRange: {
      start: { line: 0, character: 0 },
      end: { line: 0, character: 3 },
    },
    ...overrides,
  };
}

function makeNode(item: LspCallHierarchyItem, path: string, children: CallHierarchyNode[] = []): CallHierarchyNode {
  return {
    item,
    expanded: false,
    loading: false,
    children,
    path,
  };
}

// ── Item Key ────────────────────────────────────────────────────────

describe("callHierarchyItemKey", () => {
  it("produces deterministic keys", () => {
    const item = makeItem({ name: "main" });
    const k1 = callHierarchyItemKey(item);
    const k2 = callHierarchyItemKey(item);
    expect(k1).toBe(k2);
  });

  it("differentiates items at different positions", () => {
    const a = makeItem({ name: "foo", range: { start: { line: 0, character: 0 }, end: { line: 0, character: 3 } }, selectionRange: { start: { line: 0, character: 0 }, end: { line: 0, character: 3 } } });
    const b = makeItem({ name: "foo", range: { start: { line: 5, character: 0 }, end: { line: 5, character: 3 } }, selectionRange: { start: { line: 5, character: 0 }, end: { line: 5, character: 3 } } });
    expect(callHierarchyItemKey(a)).not.toBe(callHierarchyItemKey(b));
  });

  it("differentiates items with different names", () => {
    const foo = makeItem({ name: "foo" });
    const bar = makeItem({ name: "bar" });
    expect(callHierarchyItemKey(foo)).not.toBe(callHierarchyItemKey(bar));
  });

  it("differentiates items in different files", () => {
    const a = makeItem({ uri: "file:///a.ts" });
    const b = makeItem({ uri: "file:///b.ts" });
    expect(callHierarchyItemKey(a)).not.toBe(callHierarchyItemKey(b));
  });
});

// ── Tree Map ────────────────────────────────────────────────────────

describe("mapCallHierarchyTree", () => {
  it("updates the matching root node", () => {
    const tree = [
      makeNode(makeItem({ name: "a" }), "a"),
      makeNode(makeItem({ name: "b" }), "b"),
    ];
    const result = mapCallHierarchyTree(tree, "b", (n) => ({ ...n, expanded: true }));
    expect(result[0].expanded).toBe(false);
    expect(result[1].expanded).toBe(true);
  });

  it("preserves structural sharing for unaffected nodes", () => {
    const nodeA = makeNode(makeItem({ name: "a" }), "a");
    const nodeB = makeNode(makeItem({ name: "b" }), "b");
    const tree = [nodeA, nodeB];
    const result = mapCallHierarchyTree(tree, "b", (n) => ({ ...n, expanded: true }));
    // Node A should not be re-created.
    expect(result[0]).toBe(nodeA);
  });

  it("recurses into children when path matches descendant", () => {
    const child = makeNode(makeItem({ name: "child" }), "a>child");
    const root = makeNode(makeItem({ name: "a" }), "a", [child]);
    const tree = [root];
    const result = mapCallHierarchyTree(tree, "a>child", (n) => ({ ...n, loading: true }));
    expect(result[0].children[0].loading).toBe(true);
    // The parent should be a new object (its children changed), but its
    // siblings and unaffected descendants are untouched.
    expect(result[0]).not.toBe(root);
  });

  it("returns a new array with the same length", () => {
    const tree = [
      makeNode(makeItem({ name: "a" }), "a"),
      makeNode(makeItem({ name: "b" }), "b"),
      makeNode(makeItem({ name: "c" }), "c"),
    ];
    const result = mapCallHierarchyTree(tree, "b", (n) => n);
    expect(result).toHaveLength(3);
  });

  it("leaves tree unchanged when path is not found", () => {
    const tree = [
      makeNode(makeItem({ name: "a" }), "a"),
    ];
    const result = mapCallHierarchyTree(tree, "nonexistent", (n) => ({ ...n, expanded: true }));
    expect(result[0].expanded).toBe(false);
  });

  it("handles deeply nested paths", () => {
    const grandchild = makeNode(makeItem({ name: "gc" }), "root>child>gc");
    const child = makeNode(makeItem({ name: "child" }), "root>child", [grandchild]);
    const root = makeNode(makeItem({ name: "root" }), "root", [child]);
    const result = mapCallHierarchyTree([root], "root>child>gc", (n) => ({ ...n, expanded: true }));
    expect(result[0].children[0].children[0].expanded).toBe(true);
  });
});

// ── Cycle Detection ────────────────────────────────────────────────

describe("dedupCallHierarchyCycles", () => {
  it("drops nodes whose key is an ancestor", () => {
    // Parent path: "root>A". Child with key "A" would form a cycle.
    const child = makeNode(makeItem({ name: "A" }), "root>A>A");
    const other = makeNode(makeItem({ name: "B" }), "root>A>B");
    const result = dedupCallHierarchyCycles([child, other], "root>A");
    expect(result).toHaveLength(1);
    expect(result[0].item.name).toBe("B");
  });

  it("keeps all children when no cycle exists", () => {
    const parentPath = "root>A";
    const children = [
      makeNode(makeItem({ name: "B" }), "root>A>B"),
      makeNode(makeItem({ name: "C" }), "root>A>C"),
    ];
    const result = dedupCallHierarchyCycles(children, parentPath);
    expect(result).toHaveLength(2);
  });

  it("handles empty children list", () => {
    expect(dedupCallHierarchyCycles([], "root")).toEqual([]);
  });

  it("prevents self-recursion loops", () => {
    // A calls A (direct recursion)
    const selfChild = makeNode(makeItem({ name: "A" }), "A>A");
    const result = dedupCallHierarchyCycles([selfChild], "A");
    expect(result).toHaveLength(0);
  });

  it("prevents mutual-recursion loops", () => {
    // A → B → A (mutual recursion)
    // When expanding B, we don't want to show A again as B's child.
    const backToA = makeNode(makeItem({ name: "A" }), "A>B>A");
    const result = dedupCallHierarchyCycles([backToA], "A>B");
    expect(result).toHaveLength(0);
  });

  it("detects cycles even when intermediate nodes differ", () => {
    // A → X → Y → A (should drop A)
    const backToA = makeNode(makeItem({ name: "A" }), "A>X>Y>A");
    const result = dedupCallHierarchyCycles([backToA], "A>X>Y");
    expect(result).toHaveLength(0);
  });
});
