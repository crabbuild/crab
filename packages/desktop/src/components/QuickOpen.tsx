import {
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  Command as CommandRoot,
} from "@/components/ui/command";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import clsx from "clsx";
import { useEffect, useMemo, useRef, useState } from "react";
import { DirEntry } from "../ipc";
import { FileIcon } from "./FileIcon";

interface Props {
  entries: DirEntry[] | null;
  open: boolean;
  onClose: () => void;
  onPick: (entry: DirEntry) => void;
}

// Cmd-P style fuzzy finder. Subsequence match on the lowercased path
// plus a position-based score: matches that hug the basename score
// higher than matches deep in the path. Good enough for tens of
// thousands of files without a real fuzzy library.
export function QuickOpen({ entries, open, onClose, onPick }: Props) {
  const [query, setQuery] = useState("");
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    if (open) {
      setQuery("");
      setSel(0);
      // Defer focus to the next tick so the dialog is mounted first.
      queueMicrotask(() => inputRef.current?.focus());
    }
  }, [open]);

  const matches = useMemo(() => {
    if (!entries) return [];
    const pool = entries.filter((e) => e.kind !== "dir" && e.kind !== "submodule");
    if (!query.trim()) return pool.slice(0, 40);
    return fuzzyRank(pool, query.trim().toLowerCase()).slice(0, 40);
  }, [entries, query]);

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
        className="block gap-0 p-0 w-[min(640px,92vw)] overflow-hidden rounded-lg border border-border bg-card shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <CommandRoot
          shouldFilter={false}
          value={String(sel)}
          onValueChange={(value) => setSel(Number(value))}
        >
          <DialogTitle className="sr-only">{"Quick Open"}</DialogTitle>
          <CommandInput
            ref={inputRef}
            value={query}
            onValueChange={(value) => {
            setQuery(value);
            setSel(0);
            }}
          className="w-full bg-background px-4 py-3 font-mono text-sm focus-visible:outline-none"
          placeholder="Go to file…"
          aria-label="Go to file"
        />
        <CommandList className="max-h-[60vh] overflow-auto border-t border-border">
            <CommandGroup>
              {matches.map((m, i) => (
            <CommandItem
                  value={String(i)}
                  key={m.path}
              className={clsx(
                "flex cursor-pointer items-center gap-2 px-3 py-2 font-mono text-[12.5px]",
                i === sel ? "bg-primary/25 text-foreground" : "text-foreground hover:bg-accent",
              )}
                  onSelect={() => {
                onPick(m);
                onClose();
              }}
            >
              <FileIcon path={m.path} kind={m.kind} />
              <span className="truncate">{m.path.split("/").pop()}</span>
              <span className="ml-auto truncate text-[11px] text-muted-foreground" title={m.path}>
                {m.path}
              </span>
            </CommandItem>
          ))}
          {matches.length === 0 && (
            <li className="px-3 py-4 text-center text-xs text-muted-foreground">
                  No matches
                </li>
          )}
        </CommandGroup>
          </CommandList>
        </CommandRoot>
      </DialogContent>
    </Dialog>
  );
}

// Subsequence match with a score that favours:
//  - matches in the basename
//  - consecutive matching characters
//  - earlier match positions
//
// Returns ranked entries with the best match first.
function fuzzyRank(entries: DirEntry[], q: string): DirEntry[] {
  const scored: Array<{ e: DirEntry; score: number }> = [];
  for (const e of entries) {
    const path = e.path.toLowerCase();
    const base = path.slice(path.lastIndexOf("/") + 1);
    const score = Math.max(matchScore(base, q) + 500, matchScore(path, q));
    if (score > 0) scored.push({ e, score });
  }
  scored.sort((a, b) => b.score - a.score);
  return scored.map((s) => s.e);
}

function matchScore(hay: string, q: string): number {
  let hi = 0;
  let qi = 0;
  let score = 0;
  let prevHit = -2;
  while (hi < hay.length && qi < q.length) {
    if (hay[hi] === q[qi]) {
      // Base hit credit, bonus for consecutive, small penalty for
      // hits at the end of the string.
      score += 10;
      if (prevHit === hi - 1) score += 15;
      prevHit = hi;
      qi++;
    }
    hi++;
  }
  return qi === q.length ? score - hi : 0;
}
