import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Grep/Glob/search tool calls.
 *
 * Shows the search pattern and path, with results rendered as a
 * compact list of matches with syntax-highlighted code snippets.
 *
 * Agent-agnostic: works with any adapter that provides:
 *   - input.pattern / input.regex / input.glob
 *   - input.path / input.directory
 *   - detail (search results from tool_result)
 */

import { useMemo } from "react";
import clsx from "clsx";
import { ChevronDown, ChevronRight, Search, FileSearch, FolderSearch } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { ShikiPreview } from "./ShikiPreview";

// ── Helpers ─────────────────────────────────────────────────────────

interface SearchMatch {
  file: string;
  line?: number;
  content: string;
}

/**
 * Parse search results from the tool output.
 * Handles common formats:
 *   - "path/to/file:linenum:content"
 *   - "path/to/file" (glob results)
 */
function parseSearchResults(output: string): SearchMatch[] {
  if (!output.trim()) return [];

  const lines = output.split("\n").filter((l) => l.trim());
  const matches: SearchMatch[] = [];

  for (const line of lines) {
    // Try "file:line:content" format (grep-style).
    const grepMatch = line.match(/^(.+?):(\d+):(.*)$/);
    if (grepMatch) {
      matches.push({
        file: grepMatch[1],
        line: Number(grepMatch[2]),
        content: grepMatch[3],
      });
      continue;
    }

    // Try "file:content" format (no line number).
    const simpleMatch = line.match(/^(.+?):(.+)$/);
    if (simpleMatch && simpleMatch[1].includes("/")) {
      matches.push({
        file: simpleMatch[1],
        content: simpleMatch[2],
      });
      continue;
    }

    // Plain file path (glob results).
    if (line.includes("/") || line.includes(".")) {
      matches.push({ file: line, content: "" });
    }
  }

  return matches;
}

// ── Component ───────────────────────────────────────────────────────

export function SearchCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const input = event.input as Record<string, unknown> | undefined;
  const pattern = String(input?.pattern ?? input?.regex ?? input?.glob ?? "");
  const searchPath = String(input?.path ?? input?.directory ?? "");
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  const isGlob = event.tool === "Glob";
  const Icon = isGlob ? FolderSearch : (searchPath ? FileSearch : Search);
  const label = event.tool ?? "Search";

  const matches = useMemo(() => parseSearchResults(output), [output]);
  const matchCount = matches.length;

  // Group matches by file for compact display.
  const fileGroups = useMemo(() => {
    const groups = new Map<string, SearchMatch[]>();
    for (const m of matches) {
      const existing = groups.get(m.file) ?? [];
      existing.push(m);
      groups.set(m.file, existing);
    }
    return groups;
  }, [matches]);

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const icon = <Icon size={11} className="shrink-0 text-gx-muted" />;
    const defaultSummary = (
      <>
        {icon}
        <span className="font-medium text-gx-text/80 truncate">{label}</span>
        <code className="rounded bg-gx-accent/10 px-1 py-0.5 text-[9px] text-gx-accent font-mono truncate">
          {pattern || searchPath}
        </code>
        {matchCount > 0 && (
          <span className="shrink-0 text-[9px] text-gx-accent">{matchCount} {matchCount === 1 ? "match" : "matches"}</span>
        )}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`${label}: ${pattern || searchPath}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      {/* Header */}
      <button
        type="button"
        onClick={() => hasOutput && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasOutput && "hover:bg-gx-surface-hover cursor-pointer",
          !hasOutput && "cursor-default",
        )}
      >
        <Icon size={11} className="text-gx-muted shrink-0" />
        <span className="shrink-0 font-medium text-gx-muted">{label}</span>

        {/* Pattern */}
        <span className="min-w-0 flex-1 flex items-center gap-1.5 truncate">
          <code className="rounded bg-gx-accent/10 px-1 py-0.5 text-[9px] text-gx-accent font-mono truncate">
            {pattern || searchPath}
          </code>
          {searchPath && pattern && (
            <span className="text-[9px] text-gx-muted/60 truncate">in {searchPath}</span>
          )}
        </span>

        {/* Match count */}
        {matchCount > 0 && (
          <span className="shrink-0 rounded bg-gx-accent/10 px-1 py-0.5 text-[8px] font-medium text-gx-accent border border-gx-accent/20">
            {matchCount} {matchCount === 1 ? "match" : "matches"}
          </span>
        )}

        {hasOutput && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>

      {/* Expanded: search results */}
      {expanded && hasOutput && (
        <div className="border-t border-gx-border/30 max-h-[250px] overflow-auto">
          {fileGroups.size > 0 ? (
            <SearchResults fileGroups={fileGroups} />
          ) : (
            <pre className="px-2.5 py-2 font-mono text-[10px] leading-[1.5] text-gx-text/60 whitespace-pre-wrap">
              {output.length > 5000 ? output.slice(0, 5000) + "\n… (truncated)" : output}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

// ── Search results renderer ─────────────────────────────────────────

function SearchResults({ fileGroups }: { fileGroups: Map<string, SearchMatch[]> }) {
  const MAX_FILES = 15;
  const entries = Array.from(fileGroups.entries());
  const visible = entries.slice(0, MAX_FILES);
  const hasMore = entries.length > MAX_FILES;

  return (
    <div className="py-1">
      {visible.map(([file, matches]) => {
        // Collect match lines into a single snippet for syntax highlighting.
        const contentLines = matches
          .slice(0, 5)
          .filter((m) => m.content)
          .map((m) => (m.line ? `${m.content.trim()}` : m.content.trim()));
        const snippet = contentLines.join("\n");

        return (
          <div key={file} className="px-2.5 py-0.5">
            {/* File header */}
            <div className="flex items-center gap-1.5 text-[9px]">
              <span className="text-gx-text/80 font-medium truncate">{file}</span>
              {matches.length > 1 && (
                <span className="text-gx-muted/60">({matches.length})</span>
              )}
            </div>
            {/* Match lines — syntax highlighted when possible */}
            {snippet ? (
              <div className="pl-2 mt-0.5">
                <ShikiPreview code={snippet} path={file} maxLines={5} className="[&_pre]:!py-1 [&_pre]:!px-1.5" />
              </div>
            ) : (
              matches.slice(0, 3).map((m, idx) => (
                m.content ? (
                  <div key={idx} className="flex items-baseline gap-1.5 pl-2 text-[9px]">
                    {m.line && (
                      <span className="shrink-0 text-gx-muted/50 tabular-nums font-mono w-5 text-right">
                        {m.line}
                      </span>
                    )}
                    <span className="font-mono text-gx-text/60 truncate">{m.content.trim()}</span>
                  </div>
                ) : null
              ))
            )}
            {matches.length > 5 && (
              <div className="pl-2 text-[8px] text-gx-muted/50 italic">
                +{matches.length - 5} more
              </div>
            )}
          </div>
        );
      })}
      {hasMore && (
        <div className="px-2.5 py-1 text-[9px] text-gx-muted italic">
          +{entries.length - MAX_FILES} more files…
        </div>
      )}
    </div>
  );
}
