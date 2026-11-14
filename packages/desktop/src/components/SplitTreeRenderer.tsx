/**
 * SplitTreeRenderer — recursively renders a nested split tree layout.
 *
 * Each BranchNode becomes a `PanelGroup` with its own direction, and each
 * LeafNode renders a `TerminalPane`. This allows arbitrary nesting of
 * horizontal and vertical splits.
 */

import { Panel, PanelGroup } from "react-resizable-panels";
import { ResizableHandle } from "@/components/ui/resizable";
import type { SplitNode, BranchNode, LeafNode } from "../lib/terminal-split-tree";

// ── Types ───────────────────────────────────────────────────────────

export interface SplitTreeRendererProps {
  root: SplitNode;
  /** Render function for leaf nodes — the parent provides the TerminalPane. */
  renderLeaf: (leaf: LeafNode) => React.ReactNode;
  onResize: (branchId: string, sizes: number[]) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function SplitTreeRenderer({ root, renderLeaf, onResize }: SplitTreeRendererProps) {
  return <NodeRenderer node={root} renderLeaf={renderLeaf} onResize={onResize} />;
}

function NodeRenderer({
  node,
  renderLeaf,
  onResize,
}: {
  node: SplitNode;
  renderLeaf: (leaf: LeafNode) => React.ReactNode;
  onResize: SplitTreeRendererProps["onResize"];
}) {
  if (node.type === "leaf") {
    return <>{renderLeaf(node)}</>;
  }

  return (
    <BranchRenderer
      branch={node}
      renderLeaf={renderLeaf}
      onResize={onResize}
    />
  );
}

function BranchRenderer({
  branch,
  renderLeaf,
  onResize,
}: {
  branch: BranchNode;
  renderLeaf: (leaf: LeafNode) => React.ReactNode;
  onResize: SplitTreeRendererProps["onResize"];
}) {
  return (
    <PanelGroup
      direction={branch.direction}
      onLayout={sizes => onResize(branch.id, sizes)}
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
            onResize={onResize}
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
      {!isFirst && <ResizableHandle aria-label="Resize terminal panes" aria-orientation={direction === "horizontal" ? "vertical" : "horizontal"} />}
      <Panel id={panelId} minSize={10} defaultSize={defaultSize} className="min-w-0 min-h-0">
        {children}
      </Panel>
    </>
  );
}
