import { useMemo } from "react";
import { ChevronRight } from "lucide-react";
import { getAncestorChain, SymbolNode } from "../lib/symbol-tree";
import { SymbolKindIcon } from "./SymbolKindIcon";

export interface BreadcrumbBarProps {
  path: string;
  symbols?: SymbolNode[];
  currentLine?: number;
  onNavigateToSymbol?: (symbol: SymbolNode) => void;
}

/**
 * Horizontal breadcrumb bar rendered above the editor content.
 *
 * Shows the file path segments as clickable breadcrumbs and, when a symbol
 * tree is available, appends the current symbol ancestor chain based on
 * the cursor line.
 */
export function BreadcrumbBar({
  path,
  symbols,
  currentLine,
  onNavigateToSymbol,
}: BreadcrumbBarProps) {
  const pathSegments = useMemo(() => path.split("/").filter(Boolean), [path]);

  const symbolChain = useMemo(() => {
    if (!symbols || currentLine == null) return [];
    return getAncestorChain(symbols, currentLine);
  }, [symbols, currentLine]);

  return (
    <nav
      className="flex shrink-0 items-center gap-0.5 overflow-x-auto border-b border-gx-border bg-gx-surface px-3 py-1 text-[11px]"
      aria-label="Breadcrumb"
    >
      {pathSegments.map((segment, idx) => (
        <span key={`path-${idx}`} className="flex items-center gap-0.5">
          {idx > 0 && (
            <ChevronRight size={10} className="shrink-0 text-gx-muted" />
          )}
          <button
            type="button"
            className="rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition truncate max-w-[160px]"
            title={segment}
          >
            {segment}
          </button>
        </span>
      ))}

      {symbolChain.map((symbol, idx) => (
        <span key={`sym-${idx}`} className="flex items-center gap-0.5">
          <ChevronRight size={10} className="shrink-0 text-gx-muted" />
          <button
            type="button"
            className="flex items-center gap-1 rounded px-1 py-0.5 text-gx-text hover:bg-gx-surface-hover transition truncate max-w-[160px]"
            title={`${symbol.kind}: ${symbol.name} (line ${symbol.range.startLine + 1})`}
            onClick={() => onNavigateToSymbol?.(symbol)}
          >
            <SymbolKindIcon kind={symbol.kind} size={11} />
            <span>{symbol.name}</span>
          </button>
        </span>
      ))}
    </nav>
  );
}
