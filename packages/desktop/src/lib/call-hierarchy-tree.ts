/**
 * Pure tree helpers for the call hierarchy panel.
 *
 * Kept in a standalone module so they can be unit-tested without
 * pulling in React / Monaco.
 */

import type { LspCallHierarchyItem } from "./lsp-types";

export interface CallHierarchyNode {
  item: LspCallHierarchyItem;
  fromRanges?: Array<{ startLine: number; startChar: number }>;
  expanded: boolean;
  loading: boolean;
  children: CallHierarchyNode[];
  /** Dot-delimited path from the root; unique per-node. */
  path: string;
}

/**
 * A stable, unique key for an LSP call hierarchy item based on its
 * URI + source position + name. Two different calls to the same
 * function share this key.
 */
export function callHierarchyItemKey(item: LspCallHierarchyItem): string {
  return `${item.uri}::${item.range.start.line}:${item.range.start.character}::${item.name}`;
}

/**
 * Immutably update the node at `path`. Preserves structural sharing
 * for unaffected branches so React re-renders are minimal.
 */
export function mapCallHierarchyTree(
  nodes: CallHierarchyNode[],
  path: string,
  mutator: (node: CallHierarchyNode) => CallHierarchyNode,
): CallHierarchyNode[] {
  return nodes.map((node) => {
    if (node.path === path) {
      return mutator(node);
    }
    if (path.startsWith(node.path + ">")) {
      return { ...node, children: mapCallHierarchyTree(node.children, path, mutator) };
    }
    return node;
  });
}

/**
 * Filter a list of child nodes to remove entries whose item key is
 * already an ancestor in the parent's path. Prevents the panel from
 * rendering infinite cycles on mutually-recursive functions.
 */
export function dedupCallHierarchyCycles(
  children: CallHierarchyNode[],
  parentPath: string,
): CallHierarchyNode[] {
  const ancestors = new Set(parentPath.split(">"));
  return children.filter((node) => {
    const lastSegment = node.path.split(">").pop() ?? "";
    return !ancestors.has(lastSegment);
  });
}
