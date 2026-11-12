import { Button } from "@/components/ui/button";
import clsx from "clsx";
import { ChevronDown, ChevronRight, List, Loader2 } from "lucide-react";
import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { SymbolKindIcon } from "../components/SymbolKindIcon";
import { getAncestorChain, getSymbolTree, SymbolNode } from "../lib/symbol-tree";
import { OpenRepo } from "../state";

export interface OutlineViewProps {
  repo: OpenRepo;
  path: string | null;
  source: string | null;
  currentLine?: number;
  onNavigateToLine?: (line: number) => void;
}

/**
 * Sidebar view showing the symbol tree for the currently open file.
 *
 * Renders as a collapsible tree: classes contain methods, modules contain
 * functions, etc. Clicking a symbol scrolls the editor to that symbol's
 * line. The symbol containing the current cursor position is highlighted.
 */
export const OutlineView = memo(function OutlineView({
  repo: _repo,
  path,
  source,
  currentLine,
  onNavigateToLine,
}: OutlineViewProps) {
  const [symbols, setSymbols] = useState<SymbolNode[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!path || source == null) {
      setSymbols([]);
      return;
    }

    let cancelled = false;
    setLoading(true);
    setError(null);

    getSymbolTree(source, path, Date.now(), null)
      .then((tree) => {
        if (!cancelled) {
          setSymbols(tree);
          setLoading(false);
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
          setLoading(false);
        }
      });

    return () => {
      cancelled = true;
    };
  }, [path, source]);

  // Compute the set of active (ancestor) symbol names for highlighting.
  const activeSymbols = useMemo(() => {
    if (currentLine == null || symbols.length === 0) return new Set<SymbolNode>();
    const chain = getAncestorChain(symbols, currentLine);
    return new Set(chain);
  }, [symbols, currentLine]);

  if (!path) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-center text-xs text-muted-foreground">
        <div>
          <List size={24} className="mx-auto mb-2 opacity-50" />
          <div>Open a file to see its outline</div>
        </div>
      </div>
    );
  }

  if (loading) {
    return (
      <div className="flex items-center justify-center gap-2 p-4 text-xs text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Parsing symbols…
      </div>
    );
  }

  if (error) {
    return (
      <div className="m-2 rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
        {error}
      </div>
    );
  }

  if (symbols.length === 0) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-center text-xs text-muted-foreground">
        <div>
          <List size={24} className="mx-auto mb-2 opacity-50" />
          <div>No symbols found</div>
        </div>
      </div>
    );
  }

  return (
    <div className="h-full overflow-y-auto py-1">
      <SymbolTreeList
        nodes={symbols}
        depth={0}
        activeSymbols={activeSymbols}
        onNavigateToLine={onNavigateToLine}
      />
    </div>
  );
});

// ---------------------------------------------------------------------------
// SymbolTreeList — recursive tree rendering
// ---------------------------------------------------------------------------

function SymbolTreeList({
  nodes,
  depth,
  activeSymbols,
  onNavigateToLine,
}: {
  nodes: SymbolNode[];
  depth: number;
  activeSymbols: Set<SymbolNode>;
  onNavigateToLine?: (line: number) => void;
}) {
  return (
    <ul role="tree" className="list-none">
      {nodes.map((node, idx) => (
        <SymbolTreeItem
          key={`${node.name}-${node.range.startLine}-${idx}`}
          node={node}
          depth={depth}
          activeSymbols={activeSymbols}
          onNavigateToLine={onNavigateToLine}
        />
      ))}
    </ul>
  );
}

function SymbolTreeItem({
  node,
  depth,
  activeSymbols,
  onNavigateToLine,
}: {
  node: SymbolNode;
  depth: number;
  activeSymbols: Set<SymbolNode>;
  onNavigateToLine?: (line: number) => void;
}) {
  const hasChildren = node.children.length > 0;
  const [expanded, setExpanded] = useState(true);
  const isActive = activeSymbols.has(node);

  const handleClick = useCallback(() => {
    onNavigateToLine?.(node.range.startLine);
  }, [onNavigateToLine, node.range.startLine]);

  const handleToggle = useCallback((e: React.MouseEvent) => {
    e.stopPropagation();
    setExpanded((prev) => !prev);
  }, []);

  return (
    <li role="treeitem" aria-expanded={hasChildren ? expanded : undefined}>
      <div
        className={clsx(
          "group flex items-center gap-1 rounded-sm px-2 py-0.5 text-[12px] cursor-pointer transition",
          isActive
            ? "bg-primary/10 text-foreground"
            : "text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
        style={{ paddingLeft: `${depth * 12 + 8}px` }}
        onClick={handleClick}
      >
        {/* Expand/collapse toggle */}
        {hasChildren ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="shrink-0 p-0.5 rounded hover:bg-accent"
            onClick={handleToggle}
            aria-label={expanded ? "Collapse" : "Expand"}
          >
            {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          </Button>
        ) : (
          <span className="w-[16px] shrink-0" />
        )}

        <SymbolKindIcon kind={node.kind} size={12} />
        <span className="truncate">{node.name}</span>
        <span className="ml-auto shrink-0 text-[10px] text-muted-foreground opacity-0 group-hover:opacity-100 transition">
          {node.range.startLine + 1}
        </span>
      </div>

      {hasChildren && expanded && (
        <SymbolTreeList
          nodes={node.children}
          depth={depth + 1}
          activeSymbols={activeSymbols}
          onNavigateToLine={onNavigateToLine}
        />
      )}
    </li>
  );
}
