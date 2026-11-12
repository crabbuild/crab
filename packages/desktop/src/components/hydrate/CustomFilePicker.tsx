import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Search } from "lucide-react";
import { useMemo, useState } from "react";
import { HydrationEntry } from "../../ipc";
import { formatBytes } from "../../lib/format";

interface Props {
  entries: HydrationEntry[];
  selected: string[];
  onChange: (next: string[]) => void;
}

/**
 * Custom file picker for the "Custom" hydrate scope. Lists every
 * tracked file and lets the user toggle individual paths. Includes a
 * search filter and a row cap so very large repos don't render
 * thousands of DOM nodes at once.
 *
 * Selection is stored by path and resolves against the current inventory.
 */
export function CustomFilePicker({ entries, selected, onChange }: Props) {
  const [query, setQuery] = useState("");
  const [stateFilter, setStateFilter] = useState<"all" | "pointer" | "hydrated">("all");

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return entries.filter((e) => {
      if (q && !e.path.toLowerCase().includes(q)) return false;
      if (stateFilter === "pointer" && e.state === "hydrated") return false;
      if (stateFilter === "hydrated" && e.state !== "hydrated") return false;
      return true;
    });
  }, [entries, query, stateFilter]);

  const MAX_ROWS = 500;
  const truncated = filtered.length > MAX_ROWS;
  const visible = truncated ? filtered.slice(0, MAX_ROWS) : filtered;

  const selSet = useMemo(() => new Set(selected), [selected]);

  const visiblePaths = useMemo(() => visible.map((e) => e.path), [visible]);
  const allVisibleSelected =
    visiblePaths.length > 0 && visiblePaths.every((p) => selSet.has(p));

  function toggleAllVisible() {
    const next = new Set(selSet);
    if (allVisibleSelected) {
      for (const p of visiblePaths) next.delete(p);
    } else {
      for (const p of visiblePaths) next.add(p);
    }
    onChange(Array.from(next));
  }

  function togglePath(path: string, checked: boolean) {
    const next = new Set(selSet);
    if (checked) next.add(path);
    else next.delete(path);
    onChange(Array.from(next));
  }

  const selection = useMemo(() => {
    let count = 0;
    let bytes = 0;
    for (const entry of entries) {
      if (selSet.has(entry.path)) {
        count += 1;
        bytes += entry.logical;
      }
    }
    return { count, bytes };
  }, [entries, selSet]);

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative flex-1 min-w-[12rem]">
          <Search
            size={12}
            className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-muted-foreground"
          />
          <Input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Filter by path…"
            className="w-full rounded border border-border bg-background py-1 pl-7 pr-2 text-xs focus-visible:outline-none focus:border-primary"
          />
        </div>
        <ToggleGroup
          type="single"
          value={stateFilter}
          onValueChange={(value) => {
            if (value) setStateFilter(value as typeof stateFilter);
          }}
          aria-label="State filter"
          className="inline-flex items-center gap-px rounded border border-border bg-card p-0.5 text-[11px]"
        >
          {(
            [
              ["all", "All states"],
              ["pointer", "Pointer-only"],
              ["hydrated", "Hydrated"],
            ] as const
          ).map(([id, label]) => (
            <ToggleGroupItem
              value={id}
              key={id}
              className={
                "rounded px-1.5 py-0.5 transition " +
                (stateFilter === id
                  ? "bg-background text-foreground"
                  : "text-muted-foreground hover:text-foreground")
              }
            >
              {label}
            </ToggleGroupItem>
          ))}
        </ToggleGroup>
      </div>

      <div className="flex items-center justify-between text-[11px] text-muted-foreground">
        <span>
          {selection.count.toLocaleString()} selected ·{" "}
          {formatBytes(selection.bytes)} · {filtered.length.toLocaleString()}{" "}
          match{filtered.length === 1 ? "" : "es"}
        </span>
        <div className="flex items-center gap-2">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="text-primary hover:underline"
            onClick={toggleAllVisible}
            disabled={visiblePaths.length === 0}
          >
            {allVisibleSelected ? "Clear visible" : "Select visible"}
          </Button>
          {selSet.size > 0 && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="text-muted-foreground hover:text-foreground hover:underline"
              onClick={() => onChange([])}
            >
              Clear all
            </Button>
          )}
        </div>
      </div>

      <div className="max-h-64 overflow-auto rounded border border-border bg-background">
        {visible.length === 0 ? (
          <div className="px-3 py-4 text-center text-xs text-muted-foreground">
            No tracked files match the current filter.
          </div>
        ) : (
          <ul className="divide-y divide-border/50 text-[11px]">
            {visible.map((e) => {
              const checked = selSet.has(e.path);
              return (
                <li key={e.path}>
                  <label
                    className={
                      "flex cursor-pointer items-center gap-2 px-2 py-1 transition " +
                      (checked ? "bg-primary/10" : "hover:bg-background")
                    }
                  >
                    <Checkbox
                      className="accent-primary"
                      checked={checked}
                      onCheckedChange={(checked) =>
                        togglePath(e.path, checked === true)
                      }
                    />
                    <span className="flex-1 truncate font-mono text-foreground">
                      {e.path}
                    </span>
                    <StateBadge state={e.state} />
                    <span className="shrink-0 text-muted-foreground">
                      {formatBytes(e.logical)}
                    </span>
                  </label>
                </li>
              );
            })}
          </ul>
        )}
        {truncated && (
          <div className="border-t border-border px-3 py-1.5 text-[11px] text-muted-foreground">
            Showing first {MAX_ROWS.toLocaleString()} of{" "}
            {filtered.length.toLocaleString()} matches. Refine the filter to
            narrow down.
          </div>
        )}
      </div>
    </div>
  );
}

function StateBadge({ state }: { state: HydrationEntry["state"] }) {
  const cls = {
    hydrated: "border-success/40 bg-success/10 text-success",
    pointer: "border-border bg-background text-muted-foreground",
    partial: "border-warning/40 bg-warning/10 text-warning",
    missing: "border-destructive/40 bg-destructive/10 text-destructive",
  }[state];
  return (
    <span
      className={`shrink-0 rounded border px-1 text-[9px] uppercase tracking-wider ${cls}`}
      title={`State: ${state}`}
    >
      {state}
    </span>
  );
}
