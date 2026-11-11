import { Input } from "@/components/ui/input";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Fuse from "fuse.js";
import clsx from "clsx";
import { GitBranch, Search } from "lucide-react";
import type { WorkspaceRepoEntry } from "../../state";

export interface FuzzySwitcherProps {
  repos: WorkspaceRepoEntry[];
  onSelect: (relativePath: string) => void;
  onClose: () => void;
}

/** Highlight matched characters in text based on Fuse.js match indices. */
function HighlightedText({
  text,
  indices,
}: {
  text: string;
  indices: ReadonlyArray<readonly [number, number]> | undefined;
}) {
  if (!indices || indices.length === 0) {
    return <span>{text}</span>;
  }

  const parts: React.ReactNode[] = [];
  let lastEnd = 0;

  for (const [start, end] of indices) {
    if (start > lastEnd) {
      parts.push(<span key={`t-${lastEnd}`}>{text.slice(lastEnd, start)}</span>);
    }
    parts.push(
      <span key={`m-${start}`} className="font-bold text-primary">
        {text.slice(start, end + 1)}
      </span>,
    );
    lastEnd = end + 1;
  }

  if (lastEnd < text.length) {
    parts.push(<span key={`t-${lastEnd}`}>{text.slice(lastEnd)}</span>);
  }

  return <>{parts}</>;
}

/**
 * Fuzzy repo switcher modal (⌘K). Uses fuse.js for fuzzy matching
 * against repo names and relative paths. Results are sorted with
 * recently-accessed repos first, then by fuse.js relevance score.
 */
export function FuzzySwitcher({ repos, onSelect, onClose }: FuzzySwitcherProps) {
  const [query, setQuery] = useState("");
  const [selectedIndex, setSelectedIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  // Auto-focus input on mount.
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Fuse.js instance for fuzzy matching.
  const fuse = useMemo(
    () =>
      new Fuse(repos, {
        keys: ["name", "relativePath"],
        includeMatches: true,
        includeScore: true,
        threshold: 0.4,
        ignoreLocation: true,
      }),
    [repos],
  );

  // Compute filtered and sorted results.
  const results = useMemo(() => {
    if (!query.trim()) {
      // No query: show all repos sorted by lastAccessedAt descending.
      return [...repos]
        .sort((a, b) => b.lastAccessedAt - a.lastAccessedAt)
        .map((repo) => ({ entry: repo, nameIndices: undefined, pathIndices: undefined }));
    }

    const fuseResults = fuse.search(query);

    // Sort: recently-accessed first, then by fuse score.
    const now = Date.now();
    const recentThreshold = 1000 * 60 * 60; // 1 hour as "recent"

    return fuseResults
      .map((result) => {
        const nameMatch = result.matches?.find((m) => m.key === "name");
        const pathMatch = result.matches?.find((m) => m.key === "relativePath");
        return {
          entry: result.item,
          score: result.score ?? 1,
          nameIndices: nameMatch?.indices as ReadonlyArray<readonly [number, number]> | undefined,
          pathIndices: pathMatch?.indices as ReadonlyArray<readonly [number, number]> | undefined,
        };
      })
      .sort((a, b) => {
        const aRecent = now - a.entry.lastAccessedAt < recentThreshold;
        const bRecent = now - b.entry.lastAccessedAt < recentThreshold;
        if (aRecent && !bRecent) return -1;
        if (!aRecent && bRecent) return 1;
        // Within same recency group, sort by fuse score (lower is better).
        return (a.score ?? 1) - (b.score ?? 1);
      });
  }, [query, repos, fuse]);

  // Reset selection when results change.
  useEffect(() => {
    setSelectedIndex(0);
  }, [results.length, query]);

  // Scroll selected item into view.
  useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const selected = list.children[selectedIndex] as HTMLElement | undefined;
    selected?.scrollIntoView({ block: "nearest" });
  }, [selectedIndex]);

  const handleSelect = useCallback(
    (relativePath: string) => {
      onSelect(relativePath);
    },
    [onSelect],
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          setSelectedIndex((i) => Math.min(i + 1, results.length - 1));
          break;
        case "ArrowUp":
          e.preventDefault();
          setSelectedIndex((i) => Math.max(i - 1, 0));
          break;
        case "Enter":
          e.preventDefault();
          if (results[selectedIndex]) {
            handleSelect(results[selectedIndex].entry.relativePath);
          }
          break;
        case "Escape":
          e.preventDefault();
          onClose();
          break;
      }
    },
    [results, selectedIndex, handleSelect, onClose],
  );

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh]"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label="Fuzzy repo switcher"
    >
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/40" />

      {/* Modal */}
      <div
        className="relative w-full max-w-md rounded-lg border border-border bg-background shadow-2xl"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={handleKeyDown}
      >
        {/* Search input */}
        <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
          <Search size={14} className="shrink-0 text-muted-foreground" />
          <Input
            ref={inputRef}
            type="text"
            className="flex-1 bg-transparent text-sm text-foreground placeholder:text-muted-foreground outline-none"
            placeholder="Search repositories…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-label="Search repositories"
            autoComplete="off"
            spellCheck={false}
          />
          <kbd className="shrink-0 rounded border border-border bg-background px-1.5 py-0.5 text-[10px] text-muted-foreground">
            esc
          </kbd>
        </div>

        {/* Results list */}
        <div
          ref={listRef}
          className="max-h-[320px] overflow-y-auto py-1"
          role="listbox"
          aria-label="Repository results"
        >
          {results.length === 0 ? (
            <div className="px-3 py-4 text-center text-xs text-muted-foreground">
              No matching repositories
            </div>
          ) : (
            results.map((result, index) => (
              <div
                key={result.entry.relativePath}
                role="option"
                aria-selected={index === selectedIndex}
                className={clsx(
                  "flex cursor-pointer items-center gap-2 px-3 py-2 mx-1 rounded transition",
                  index === selectedIndex
                    ? "bg-primary/15 text-foreground"
                    : "text-foreground hover:bg-accent",
                )}
                onClick={() => handleSelect(result.entry.relativePath)}
                onMouseEnter={() => setSelectedIndex(index)}
              >
                {/* Name and path */}
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm">
                    <HighlightedText
                      text={result.entry.name}
                      indices={result.nameIndices}
                    />
                  </div>
                  <div className="truncate text-xs text-muted-foreground">
                    <HighlightedText
                      text={result.entry.relativePath}
                      indices={result.pathIndices}
                    />
                  </div>
                </div>

                {/* Status badge: branch + dirty count (Level ≥ 1) */}
                {result.entry.level >= 1 && result.entry.branch && (
                  <div className="flex shrink-0 items-center gap-1 text-[10px] text-muted-foreground">
                    <GitBranch size={10} />
                    <span className="max-w-[80px] truncate">{result.entry.branch}</span>
                    {result.entry.dirtyCount !== null && result.entry.dirtyCount > 0 && (
                      <span className="rounded bg-warning/20 px-1 py-0.5 text-[9px] font-medium text-warning">
                        {result.entry.dirtyCount}
                      </span>
                    )}
                  </div>
                )}
              </div>
            ))
          )}
        </div>

        {/* Footer hint */}
        <div className="flex items-center justify-between border-t border-border px-3 py-1.5 text-[10px] text-muted-foreground">
          <span>↑↓ navigate · ↵ select · esc close</span>
          <span>⌘K</span>
        </div>
      </div>
    </div>
  );
}
