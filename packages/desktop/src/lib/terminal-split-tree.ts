/**
 * Terminal split tree — recursive data model for nested/mixed split layouts.
 *
 * Replaces the flat `SplitPane[]` + single `splitDirection` model with a
 * binary tree where each internal node defines its own split direction.
 * This allows arbitrary nesting: split a pane horizontally, then split one
 * of the children vertically, etc.
 *
 * Tree structure:
 *   SplitNode = LeafNode | BranchNode
 *   LeafNode  = { type: "leaf", id, tabs, activeTab }
 *   BranchNode = { type: "branch", id, direction, children: [SplitNode, ...], sizes: number[] }
 */

// ── Types ───────────────────────────────────────────────────────────

export type SplitDirection = "horizontal" | "vertical";

/** A leaf node represents a single terminal pane with tabs. */
export interface LeafNode {
  type: "leaf";
  id: string;
  tabs: LeafTab[];
  activeTab: string | null;
}

/** Minimal tab info stored in the tree (full TermTab lives in the view). */
export interface LeafTab {
  id: string;
  title: string;
  shell: string;
  cwd?: string;
  startupCommand?: string;
  sshRemote?: {
    profileId: string;
    hostname: string;
    colour?: string;
  };
}

/** A branch node splits its children in a given direction. */
export interface BranchNode {
  type: "branch";
  id: string;
  direction: SplitDirection;
  children: SplitNode[];
  /** Panel sizes as percentages (must sum to ~100). Length matches children. */
  sizes: number[];
}

export type SplitNode = LeafNode | BranchNode;

// ── Factory helpers ─────────────────────────────────────────────────

let nodeCounter = 0;

function nextId(prefix: string): string {
  return `${prefix}-${Date.now()}-${++nodeCounter}`;
}

export function createLeaf(tabs: LeafTab[] = [], activeTab: string | null = null): LeafNode {
  return { type: "leaf", id: nextId("leaf"), tabs, activeTab };
}

export function createBranch(
  direction: SplitDirection,
  children: SplitNode[],
  sizes?: number[],
): BranchNode {
  const equalSize = 100 / children.length;
  return {
    type: "branch",
    id: nextId("branch"),
    direction,
    children,
    sizes: sizes ?? children.map(() => equalSize),
  };
}

// ── Tree operations ─────────────────────────────────────────────────

/**
 * Find a leaf node by its ID.
 */
export function findLeaf(root: SplitNode, leafId: string): LeafNode | null {
  if (root.type === "leaf") {
    return root.id === leafId ? root : null;
  }
  for (const child of root.children) {
    const found = findLeaf(child, leafId);
    if (found) return found;
  }
  return null;
}

/**
 * Find the leaf that contains a given tab ID.
 */
export function findLeafByTab(root: SplitNode, tabId: string): LeafNode | null {
  if (root.type === "leaf") {
    return root.tabs.some((t) => t.id === tabId) ? root : null;
  }
  for (const child of root.children) {
    const found = findLeafByTab(child, tabId);
    if (found) return found;
  }
  return null;
}

/**
 * Get all leaf nodes in the tree (in-order traversal).
 */
export function getAllLeaves(root: SplitNode): LeafNode[] {
  if (root.type === "leaf") return [root];
  return root.children.flatMap(getAllLeaves);
}

/**
 * Get all tab IDs across the entire tree.
 */
export function getAllTabIds(root: SplitNode): string[] {
  return getAllLeaves(root).flatMap((leaf) => leaf.tabs.map((t) => t.id));
}

/**
 * Update a leaf node in the tree (immutable — returns a new tree).
 */
export function updateLeaf(
  root: SplitNode,
  leafId: string,
  updater: (leaf: LeafNode) => LeafNode,
): SplitNode {
  if (root.type === "leaf") {
    return root.id === leafId ? updater(root) : root;
  }
  return {
    ...root,
    children: root.children.map((child) => updateLeaf(child, leafId, updater)),
  };
}

/**
 * Split a leaf node in the given direction. The existing leaf becomes the
 * first child, and a new empty leaf becomes the second child.
 *
 * Returns the new tree root and the ID of the newly created leaf.
 */
export function splitLeaf(
  root: SplitNode,
  leafId: string,
  direction: SplitDirection,
): { root: SplitNode; newLeafId: string } {
  const newLeaf = createLeaf();
  const newRoot = splitLeafInner(root, leafId, direction, newLeaf);
  return { root: newRoot, newLeafId: newLeaf.id };
}

/**
 * Like splitLeaf but accepts a pre-created leaf node. Useful when the
 * caller needs to know the new leaf's ID before the tree update commits
 * (e.g., to set it as the active pane in the same render batch).
 */
export function splitLeafInPlace(
  root: SplitNode,
  leafId: string,
  direction: SplitDirection,
  newLeaf: LeafNode,
): SplitNode {
  return splitLeafInner(root, leafId, direction, newLeaf);
}

function splitLeafInner(
  node: SplitNode,
  leafId: string,
  direction: SplitDirection,
  newLeaf: LeafNode,
): SplitNode {
  if (node.type === "leaf") {
    if (node.id !== leafId) return node;
    // Replace this leaf with a branch containing [this leaf, new leaf]
    return createBranch(direction, [node, newLeaf], [50, 50]);
  }

  // Check if the target leaf is a direct child and the direction matches.
  // In that case, we can just add the new leaf as a sibling instead of
  // nesting another branch (keeps the tree flatter).
  if (node.direction === direction) {
    const idx = node.children.findIndex(
      (child) => child.type === "leaf" && child.id === leafId,
    );
    if (idx !== -1) {
      // Insert new leaf after the target
      const newChildren = [...node.children];
      newChildren.splice(idx + 1, 0, newLeaf);
      // Redistribute sizes equally
      const equalSize = 100 / newChildren.length;
      return { ...node, children: newChildren, sizes: newChildren.map(() => equalSize) };
    }
  }

  // Recurse into children
  return {
    ...node,
    children: node.children.map((child) =>
      splitLeafInner(child, leafId, direction, newLeaf),
    ),
  };
}

/**
 * Remove a leaf from the tree. If removing it leaves a branch with a
 * single child, collapse the branch (promote the remaining child).
 *
 * Returns null if the tree becomes empty.
 */
export function removeLeaf(root: SplitNode, leafId: string): SplitNode | null {
  if (root.type === "leaf") {
    return root.id === leafId ? null : root;
  }

  const newChildren: SplitNode[] = [];
  const newSizes: number[] = [];

  for (let i = 0; i < root.children.length; i++) {
    const child = root.children[i];
    if (child.type === "leaf" && child.id === leafId) {
      // Skip this child (remove it)
      continue;
    }
    const updated = removeLeaf(child, leafId);
    if (updated) {
      newChildren.push(updated);
      newSizes.push(root.sizes[i]);
    }
  }

  if (newChildren.length === 0) return null;
  if (newChildren.length === 1) return newChildren[0]; // Collapse single-child branch

  // Normalize sizes to sum to 100
  const total = newSizes.reduce((a, b) => a + b, 0);
  const normalizedSizes = newSizes.map((s) => (s / total) * 100);

  return { ...root, children: newChildren, sizes: normalizedSizes };
}

/**
 * Update panel sizes for a branch node.
 */
export function updateBranchSizes(
  root: SplitNode,
  branchId: string,
  sizes: number[],
): SplitNode {
  if (root.type === "leaf") return root;
  if (root.id === branchId) {
    if (root.sizes.length === sizes.length && root.sizes.every((size, index) => size === sizes[index])) return root;
    return { ...root, sizes };
  }
  const children = root.children.map((child) => updateBranchSizes(child, branchId, sizes));
  return children.every((child, index) => child === root.children[index]) ? root : { ...root, children };
}

// ── Serialization (compatible with terminal-persistence.ts) ─────────

import type { SplitArrangement } from "./terminal-persistence";

/**
 * Serialize the split tree into the persistence format.
 */
export function serializeTree(root: SplitNode): SplitArrangement | null {
  if (root.type === "leaf") {
    // A single leaf doesn't need a split arrangement
    return null;
  }
  return {
    direction: root.direction,
    sizes: root.sizes,
    children: root.children.map((child) => {
      if (child.type === "leaf") return child.id;
      return serializeTree(child)!;
    }),
  };
}

/**
 * Deserialize a SplitArrangement back into a tree, given a map of
 * leaf IDs to their tab data.
 */
export function deserializeTree(
  arrangement: SplitArrangement | null,
  leaves: Map<string, { tabs: LeafTab[]; activeTab: string | null }>,
): SplitNode {
  if (!arrangement) {
    // Single pane — return the first leaf
    const first = leaves.entries().next().value;
    if (first) {
      return { type: "leaf", id: first[0], tabs: first[1].tabs, activeTab: first[1].activeTab };
    }
    return createLeaf();
  }

  const children: SplitNode[] = arrangement.children.map((child) => {
    if (typeof child === "string") {
      const data = leaves.get(child);
      if (data) {
        return { type: "leaf" as const, id: child, tabs: data.tabs, activeTab: data.activeTab };
      }
      return createLeaf();
    }
    return deserializeTree(child, leaves);
  });

  return {
    type: "branch",
    id: nextId("branch"),
    direction: arrangement.direction,
    children,
    sizes: arrangement.sizes,
  };
}

/**
 * Get the next leaf in traversal order (for focus cycling).
 */
export function getNextLeaf(root: SplitNode, currentLeafId: string): LeafNode | null {
  const leaves = getAllLeaves(root);
  const idx = leaves.findIndex((l) => l.id === currentLeafId);
  if (idx === -1 || leaves.length <= 1) return null;
  return leaves[(idx + 1) % leaves.length];
}

/**
 * Get the previous leaf in traversal order (for focus cycling).
 */
export function getPrevLeaf(root: SplitNode, currentLeafId: string): LeafNode | null {
  const leaves = getAllLeaves(root);
  const idx = leaves.findIndex((l) => l.id === currentLeafId);
  if (idx === -1 || leaves.length <= 1) return null;
  return leaves[(idx - 1 + leaves.length) % leaves.length];
}

// ── Preset layouts ──────────────────────────────────────────────────

export interface PresetLayout {
  id: string;
  label: string;
  description: string;
  /** Number of columns × rows, e.g. "2×1" means 2 columns, 1 row */
  cols: number;
  rows: number;
}

export const PRESET_LAYOUTS: PresetLayout[] = [
  { id: "1x2", label: "2 Columns", description: "1×2", cols: 2, rows: 1 },
  { id: "1x3", label: "3 Columns", description: "1×3", cols: 3, rows: 1 },
  { id: "1x4", label: "4 Columns", description: "1×4", cols: 4, rows: 1 },
  { id: "2x2", label: "2×2 Grid", description: "2×2", cols: 2, rows: 2 },
  { id: "2x3", label: "2×3 Grid", description: "2×3", cols: 3, rows: 2 },
  { id: "2x4", label: "2×4 Grid", description: "2×4", cols: 4, rows: 2 },
];

/**
 * Create a tree structure for a preset grid layout.
 * Returns the root node and a flat list of all leaf IDs (for spawning terminals).
 */
export function createPresetLayout(cols: number, rows: number): { root: SplitNode; leafIds: string[] } {
  const leafIds: string[] = [];

  if (rows === 1) {
    // Single row: just horizontal columns
    const leaves: LeafNode[] = [];
    for (let c = 0; c < cols; c++) {
      const leaf = createLeaf();
      leaves.push(leaf);
      leafIds.push(leaf.id);
    }
    if (leaves.length === 1) {
      return { root: leaves[0], leafIds };
    }
    return { root: createBranch("horizontal", leaves), leafIds };
  }

  // Multiple rows: vertical split at top level, each row is a horizontal split
  const rowNodes: SplitNode[] = [];
  for (let r = 0; r < rows; r++) {
    const rowLeaves: LeafNode[] = [];
    for (let c = 0; c < cols; c++) {
      const leaf = createLeaf();
      rowLeaves.push(leaf);
      leafIds.push(leaf.id);
    }
    if (rowLeaves.length === 1) {
      rowNodes.push(rowLeaves[0]);
    } else {
      rowNodes.push(createBranch("horizontal", rowLeaves));
    }
  }

  if (rowNodes.length === 1) {
    return { root: rowNodes[0], leafIds };
  }
  return { root: createBranch("vertical", rowNodes), leafIds };
}
