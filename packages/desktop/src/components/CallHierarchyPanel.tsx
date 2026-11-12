import { Button } from "@/components/ui/button";
/**
 * CallHierarchyPanel — incoming / outgoing calls for a function.
 *
 * Shows a lazy-expanding tree. Each node can be expanded to reveal
 * its own callers (or callees, depending on direction). Nodes cache
 * their children so re-expanding is instant.
 *
 * Wire this up with:
 *   1. Call `openCallHierarchy(client, uri, line, character)` to build
 *      the root items.
 *   2. Render <CallHierarchyPanel> with the result + client reference.
 *   3. Users can toggle direction (incoming ↔ outgoing) in the panel.
 */

import clsx from "clsx";
import { ArrowDownCircle, ArrowUpCircle, ChevronDown, ChevronRight, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  callHierarchyItemKey,
  dedupCallHierarchyCycles,
  mapCallHierarchyTree,
  type CallHierarchyNode as NodeState,
} from "../lib/call-hierarchy-tree";
import type { LspClient } from "../lib/lsp-client";
import { lspSymbolKindToSymbolKind } from "../lib/lsp-symbol-kind";
import type {
  LspCallHierarchyIncomingCall,
  LspCallHierarchyItem,
  LspCallHierarchyOutgoingCall,
} from "../lib/lsp-types";
import { SymbolKindIcon } from "./SymbolKindIcon";

export type CallHierarchyDirection = "incoming" | "outgoing";

interface Props {
  /** The root items returned by `client.prepareCallHierarchy()`. */
  roots: LspCallHierarchyItem[];
  /** LSP client used to fetch children lazily. */
  client: LspClient;
  /** Direction the tree walks: who calls THIS (incoming) vs. what THIS calls (outgoing). */
  direction: CallHierarchyDirection;
  /** Swap direction without losing the roots. */
  onDirectionChange: (direction: CallHierarchyDirection) => void;
  /** Called when the user clicks a call site or symbol. */
  onNavigate: (uri: string, line: number, column: number) => void;
  /** Close the panel. */
  onClose: () => void;
}

/**
 * Kick off a call hierarchy session. Returns the roots (usually one
 * item) or null when the server returns no result.
 */
export async function openCallHierarchy(
  client: LspClient,
  uri: string,
  line: number,
  character: number,
): Promise<LspCallHierarchyItem[] | null> {
  const items = await client.prepareCallHierarchy(uri, line, character);
  return items && items.length > 0 ? items : null;
}

// ── Node state ──────────────────────────────────────────────────────

function makeNode(
  item: LspCallHierarchyItem,
  fromRanges?: LspCallHierarchyIncomingCall["fromRanges"] | LspCallHierarchyOutgoingCall["fromRanges"],
  path: string = "",
): NodeState {
  return {
    item,
    fromRanges: fromRanges?.map((r) => ({
      startLine: r.start.line,
      startChar: r.start.character,
    })),
    expanded: false,
    loading: false,
    children: [],
    path,
  };
}

// ── Component ───────────────────────────────────────────────────────

export function CallHierarchyPanel({
  roots,
  client,
  direction,
  onDirectionChange,
  onNavigate,
  onClose,
}: Props) {
  const [nodes, setNodes] = useState<NodeState[]>(() =>
    roots.map((item) => makeNode(item, undefined, callHierarchyItemKey(item))),
  );

  // When roots change (user invokes call hierarchy on a new symbol),
  // reset the tree.
  useEffect(() => {
    setNodes(roots.map((item) => makeNode(item, undefined, callHierarchyItemKey(item))));
  }, [roots]);

  // When direction changes, collapse all children to force re-fetch.
  useEffect(() => {
    setNodes((prev) =>
      prev.map((node) => ({
        ...node,
        expanded: false,
        children: [],
      })),
    );
  }, [direction]);

  // Update a node in the tree by its path.
  const updateNode = useCallback(
    (path: string, mutator: (node: NodeState) => NodeState) => {
      setNodes((prev) => mapCallHierarchyTree(prev, path, mutator));
    },
    [],
  );

  const toggleNode = useCallback(
    async (node: NodeState) => {
      if (node.loading) return;

      if (node.expanded) {
        updateNode(node.path, (n) => ({ ...n, expanded: false }));
        return;
      }

      // Lazy-fetch children on first expansion.
      if (node.children.length === 0) {
        updateNode(node.path, (n) => ({ ...n, loading: true }));
        try {
          let childNodes: NodeState[];
          if (direction === "incoming") {
            const calls = await client.callHierarchyIncoming(node.item);
            childNodes = dedupCallHierarchyCycles(
              calls.map((c) => {
                const childPath = `${node.path}>${callHierarchyItemKey(c.from)}`;
                return makeNode(c.from, c.fromRanges, childPath);
              }),
              node.path,
            );
          } else {
            const calls = await client.callHierarchyOutgoing(node.item);
            childNodes = dedupCallHierarchyCycles(
              calls.map((c) => {
                const childPath = `${node.path}>${callHierarchyItemKey(c.to)}`;
                return makeNode(c.to, c.fromRanges, childPath);
              }),
              node.path,
            );
          }

          updateNode(node.path, (n) => ({
            ...n,
            expanded: true,
            loading: false,
            children: childNodes,
          }));
        } catch {
          updateNode(node.path, (n) => ({ ...n, loading: false, expanded: true }));
        }
      } else {
        updateNode(node.path, (n) => ({ ...n, expanded: true }));
      }
    },
    [client, direction, updateNode],
  );

  const title = useMemo(() => {
    if (roots.length === 0) return "Call Hierarchy";
    const first = roots[0];
    return `${first.name} — ${direction === "incoming" ? "Incoming Calls" : "Outgoing Calls"}`;
  }, [roots, direction]);

  return (
    <div className="flex flex-col border-t border-border bg-card max-h-[320px]">
      {/* Header */}
      <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-1.5">
        <span className="truncate text-[11px] font-medium text-foreground" title={title}>
          {title}
        </span>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => onDirectionChange(direction === "incoming" ? "outgoing" : "incoming")}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
            title={`Switch to ${direction === "incoming" ? "outgoing" : "incoming"} calls`}
          >
            {direction === "incoming" ? (
              <ArrowUpCircle size={10} />
            ) : (
              <ArrowDownCircle size={10} />
            )}
            {direction === "incoming" ? "Incoming" : "Outgoing"}
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={onClose}
            className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
            aria-label="Close call hierarchy panel"
          >
            <X size={12} />
          </Button>
        </div>
      </div>

      {/* Tree */}
      <div className="min-h-0 flex-1 overflow-y-auto scrollbar-thin">
        {nodes.length === 0 ? (
          <div className="px-3 py-4 text-xs text-muted-foreground">
            No results.
          </div>
        ) : (
          <TreeNodes
            nodes={nodes}
            depth={0}
            onToggle={toggleNode}
            onNavigate={onNavigate}
            direction={direction}
          />
        )}
      </div>
    </div>
  );
}

// ── Tree rendering ──────────────────────────────────────────────────

function TreeNodes({
  nodes,
  depth,
  onToggle,
  onNavigate,
  direction,
}: {
  nodes: NodeState[];
  depth: number;
  onToggle: (node: NodeState) => void;
  onNavigate: (uri: string, line: number, column: number) => void;
  direction: CallHierarchyDirection;
}) {
  return (
    <>
      {nodes.map((node) => (
        <TreeNode
          key={node.path}
          node={node}
          depth={depth}
          onToggle={onToggle}
          onNavigate={onNavigate}
          direction={direction}
        />
      ))}
    </>
  );
}

function TreeNode({
  node,
  depth,
  onToggle,
  onNavigate,
  direction,
}: {
  node: NodeState;
  depth: number;
  onToggle: (node: NodeState) => void;
  onNavigate: (uri: string, line: number, column: number) => void;
  direction: CallHierarchyDirection;
}) {
  const { item, expanded, loading, children, fromRanges } = node;
  const hasChildrenPotentially = true; // We don't know until we fetch
  const callSites = fromRanges ?? [];

  return (
    <>
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={(e) => {
          // Shift-click jumps directly; plain click toggles.
          if (e.shiftKey) {
            onNavigate(item.uri, item.selectionRange.start.line + 1, item.selectionRange.start.character + 1);
          } else {
            onToggle(node);
          }
        }}
        onDoubleClick={() => onNavigate(item.uri, item.selectionRange.start.line + 1, item.selectionRange.start.character + 1)}
        className="justify-start flex w-full items-center gap-1 px-3 py-1 text-left text-xs hover:bg-accent"
        style={{ paddingLeft: `${12 + depth * 12}px` }}
      >
        <span className="shrink-0 text-muted-foreground">
          {loading ? (
            <Loader2 size={10} className="animate-spin" />
          ) : expanded ? (
            <ChevronDown size={10} />
          ) : hasChildrenPotentially ? (
            <ChevronRight size={10} />
          ) : (
            <span className="inline-block w-[10px]" />
          )}
        </span>
        <SymbolKindIcon kind={lspSymbolKindToSymbolKind(item.kind)} size={11} />
        <span className="truncate font-medium text-foreground">
          {item.name}
        </span>
        {item.detail && (
          <span className="shrink-0 truncate text-[10px] text-muted-foreground">
            {item.detail}
          </span>
        )}
        <span className="ml-auto shrink-0 truncate text-[10px] text-muted-foreground/70">
          {displayPath(item.uri)}
        </span>
      </Button>

      {/* Call sites (the specific lines where THIS item appears in the caller). */}
      {expanded && callSites.length > 0 && (
        <div>
          {callSites.map((site, idx) => (
            <Button
              variant="ghost"
              size="compact"
              key={`${node.path}-site-${idx}`}
              type="button"
              onClick={() =>
                onNavigate(item.uri, site.startLine + 1, site.startChar + 1)
              }
              className={clsx(
                "flex w-full items-center gap-1 py-0.5 text-left text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground",
              )}
              style={{ paddingLeft: `${36 + depth * 12}px` }}
            >
              <span>
                {direction === "incoming" ? "called at" : "calls at"} line{" "}
                {site.startLine + 1}
              </span>
            </Button>
          ))}
        </div>
      )}

      {/* Recursive children */}
      {expanded && children.length > 0 && (
        <TreeNodes
          nodes={children}
          depth={depth + 1}
          onToggle={onToggle}
          onNavigate={onNavigate}
          direction={direction}
        />
      )}
    </>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function displayPath(uri: string): string {
  const withoutScheme = uri.replace(/^file:\/\//, "");
  const segments = withoutScheme.split("/");
  if (segments.length > 3) {
    return "…/" + segments.slice(-3).join("/");
  }
  return withoutScheme;
}
