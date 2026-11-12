import {
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  Command as CommandRoot,
} from "@/components/ui/command";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
/**
 * WorkspaceSymbolPalette — Cmd/Ctrl+T style fuzzy finder for symbols
 * across the whole workspace.
 *
 * Queries every running LSP client that supports `workspaceSymbolProvider`
 * in parallel. Debounced to avoid flooding servers; cancels in-flight
 * requests when the query changes.
 *
 * Keyboard:
 *   Arrow up/down — move selection
 *   Enter         — navigate to symbol
 *   Escape        — close palette
 */

import clsx from "clsx";
import { Loader2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { lspSymbolKindToSymbolKind } from "../lib/lsp-symbol-kind";
import {
  searchWorkspaceSymbols,
  type WorkspaceSymbolResult,
} from "../lib/lsp-workspace-symbols";
import { SymbolKindIcon } from "./SymbolKindIcon";

interface Props {
  open: boolean;
  onClose: () => void;
  /** Called when the user picks a symbol. URI is file:// form. */
  onNavigate: (uri: string, line: number, column: number) => void;
}

const DEBOUNCE_MS = 200;
const MAX_RESULTS = 200;

export function WorkspaceSymbolPalette({ open, onClose, onNavigate }: Props) {
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<WorkspaceSymbolResult[]>([]);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [loading, setLoading] = useState(false);
  const [lastError, setLastError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  // Increments on every search; old completions compare and bail if stale.
  const searchTokenRef = useRef(0);
  const debounceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Reset state on open
  useEffect(() => {
    if (open) {
      setQuery("");
      setResults([]);
      setSelectedIndex(0);
      setLoading(false);
      setLastError(null);
      queueMicrotask(() => inputRef.current?.focus());
    }
    return () => {
      if (debounceTimerRef.current) {
        clearTimeout(debounceTimerRef.current);
        debounceTimerRef.current = null;
      }
    };
  }, [open]);

  // Run search on query change (debounced).
  useEffect(() => {
    if (!open) return;
    if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);

    const trimmed = query.trim();
    if (trimmed.length === 0) {
      setResults([]);
      setLoading(false);
      return;
    }

    debounceTimerRef.current = setTimeout(() => {
      const token = ++searchTokenRef.current;
      setLoading(true);
      setLastError(null);

      searchWorkspaceSymbols(trimmed, MAX_RESULTS)
        .then((items) => {
          if (token !== searchTokenRef.current) return; // Stale
          setResults(items);
          setSelectedIndex(0);
          setLoading(false);
        })
        .catch((err) => {
          if (token !== searchTokenRef.current) return;
          setLastError(err instanceof Error ? err.message : String(err));
          setResults([]);
          setLoading(false);
        });
    }, DEBOUNCE_MS);

    return () => {
      if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);
    };
  }, [query, open]);

  // Keep selected item in view as the user navigates.
  useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const item = list.querySelector<HTMLButtonElement>(`[data-index="${selectedIndex}"]`);
    item?.scrollIntoView({ block: "nearest" });
  }, [selectedIndex, results]);

  if (!open) return null;

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 flex w-[min(720px,92vw)] max-h-[70vh] flex-col overflow-hidden rounded-lg border border-border bg-card shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <CommandRoot
          shouldFilter={false}
          value={String(selectedIndex)}
          onValueChange={(value) => setSelectedIndex(Number(value))}
        >
          <DialogTitle className="sr-only">
            {"Workspace Symbol Palette"}
          </DialogTitle>
          {/* Search input */}
          <div className="flex items-center gap-2 border-b border-border px-3 py-2">
            <CommandInput
              ref={inputRef}
              value={query}
              onValueChange={(value) => setQuery(value)}
              placeholder="Search workspace symbols..."
              className="flex-1 bg-transparent text-sm text-foreground placeholder:text-muted-foreground/50 outline-none"
              aria-label="Search workspace symbols"
              autoComplete="off"
              spellCheck={false}
            />
            {loading && (
              <Loader2 size={12} className="animate-spin text-muted-foreground" />
            )}
          </div>

          {/* Results */}
          <CommandList
            ref={listRef} className="min-h-0 flex-1 overflow-y-auto scrollbar-thin"
          >
            <CommandGroup>
              {lastError && (
                <div className="px-3 py-4 text-xs text-destructive">
                  Search failed: {lastError}
                </div>
              )}
              {!lastError && !loading && query.trim() && results.length === 0 && (
                  <div className="px-3 py-4 text-xs text-muted-foreground">
                    No symbols found for "{query.trim()}"
                  </div>
                )}
              {!query.trim() && (
                <div className="px-3 py-4 text-xs text-muted-foreground">
                  Start typing to search across functions, classes, and methods
                  in your workspace.
                </div>
              )}
              {results.map((symbol, idx) => (
                <CommandItem
                  value={String(idx)}
                  key={`${symbol.language}-${symbol.uri}-${symbol.range.startLineNumber}-${symbol.name}-${idx}`}
                  data-index={idx}
                  onSelect={() => {
                setSelectedIndex(idx);
                onNavigate(symbol.uri, symbol.range.startLineNumber, symbol.range.startColumn);
                onClose();
              }}
                  className={clsx(
                "flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition",
                idx === selectedIndex
                  ? "bg-primary/10 text-foreground"
                  : "text-foreground/90 hover:bg-accent",
              )}
                >
                  <SymbolKindIcon
                kind={lspSymbolKindToSymbolKind(symbol.kind)}
                size={12}
              />
                  <span className="truncate font-medium">{symbol.name}</span>
                  {symbol.containerName && (
                    <span className="shrink-0 truncate text-[10px] text-muted-foreground">
                      in {symbol.containerName}
                    </span>
                  )}
                  <span className="ml-auto shrink-0 truncate text-[10px] text-muted-foreground/70">
                    {displayPath(symbol.uri)}:{symbol.range.startLineNumber}
                  </span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>

          {/* Footer */}
          <div className="flex items-center justify-between border-t border-border bg-background px-3 py-1 text-[10px] text-muted-foreground">
            <span>
              {results.length > 0
              ? `${results.length} result${results.length === 1 ? "" : "s"}`
              : ""}
            </span>
            <span>
              <kbd className="font-sans">↑↓</kbd> navigate{" "}
              <kbd className="font-sans ml-2">↵</kbd> open{" "}
              <kbd className="font-sans ml-2">esc</kbd> close
            </span>
          </div>
        </CommandRoot>
      </DialogContent>
    </Dialog>
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

// Memo helper not needed here: results are small and component is modal.
// Keeping this exported symbol list small.
export type { WorkspaceSymbolResult };

void useMemo;
