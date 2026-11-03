/**
 * SplitTreeRenderer — recursively renders a nested split tree layout.
 *
 * Each BranchNode becomes a `PanelGroup` with its own direction, and each
 * LeafNode renders a `TerminalPane`. This allows arbitrary nesting of
 * horizontal and vertical splits.
 */

import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import clsx from "clsx";
import type { SplitNode, BranchNode, LeafNode } from "../lib/terminal-split-tree";

// ── Types ───────────────────────────────────────────────────────────

export interface SplitTreeRendererProps {
  root: SplitNode;
  /** Render function for leaf nodes — the parent provides the TerminalPane. */
  renderLeaf: (leaf: LeafNode) => React.ReactNode;
}

// ── Component ───────────────────────────────────────────────────────

export function SplitTreeRenderer({ root, renderLeaf }: SplitTreeRendererProps) {
  return <NodeRenderer node={root} renderLeaf={renderLeaf} />;
}

function NodeRenderer({
  node,
  renderLeaf,
}: {
  node: SplitNode;
  renderLeaf: (leaf: LeafNode) => React.ReactNode;
}) {
  if (node.type === "leaf") {
    return <>{renderLeaf(node)}</>;
  }

  return (
    <BranchRenderer
      branch={node}
      renderLeaf={renderLeaf}
    />
  );
}

function BranchRenderer({
  branch,
  renderLeaf,
}: {
  branch: BranchNode;
  renderLeaf: (leaf: LeafNode) => React.ReactNode;
}) {
  return (
    <PanelGroup
      direction={branch.direction}
      className="min-h-0 min-w-0 w-full h-full flex-1"
    >
      {branch.children.map((child, idx) => (
        <ChildWithHandle
          key={child.id}
          panelId={`${branch.id}-${child.id}`}
          isFirst={idx === 0}
          direction={branch.direction}
          defaultSize={branch.sizes[idx]}
        >
          <NodeRenderer
            node={child}
            renderLeaf={renderLeaf}
          />
        </ChildWithHandle>
      ))}
    </PanelGroup>
  );
}

// ── Resize handle + panel wrapper ───────────────────────────────────

function ChildWithHandle({
  children,
  panelId,
  isFirst,
  direction,
  defaultSize,
}: {
  children: React.ReactNode;
  panelId: string;
  isFirst: boolean;
  direction: "horizontal" | "vertical";
  defaultSize: number;
}) {
  return (
    <>
      {!isFirst && (
        <PanelResizeHandle
          className={clsx(
            "bg-gx-border hover:bg-gx-accent active:bg-gx-accent transition-colors relative group",
            direction === "horizontal" ? "w-1 hover:w-1.5" : "h-1 hover:h-1.5",
          )}
        >
          {/* Drag affordance dot */}
          <div
            className={clsx(
              "absolute opacity-0 group-hover:opacity-100 transition-opacity bg-gx-accent rounded-full",
              direction === "horizontal"
                ? "top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-1 h-8"
                : "top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 h-1 w-8",
            )}
          />
        </PanelResizeHandle>
      )}
      <Panel id={panelId} minSize={10} defaultSize={defaultSize} className="min-w-0 min-h-0">
        {children}
      </Panel>
    </>
  );
}
