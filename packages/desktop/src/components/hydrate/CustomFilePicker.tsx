import { useMemo, useState } from "react";
import { Search } from "lucide-react";
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
            className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-gx-muted"
          />
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Filter by path…"
            className="w-full rounded border border-gx-border bg-gx-bg py-1 pl-7 pr-2 text-xs focus-visible:outline-none focus:border-gx-accent"
          />
        </div>
        <div
          className="inline-flex items-center gap-px rounded border border-gx-border bg-gx-surface p-0.5 text-[11px]"
          role="tablist"
          aria-label="State filter"
        >
          {(
            [
              ["all", "All states"],
              ["pointer", "Pointer-only"],
              ["hydrated", "Hydrated"],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={stateFilter === id}
              onClick={() => setStateFilter(id)}
              className={
                "rounded px-1.5 py-0.5 transition " +
                (stateFilter === id
                  ? "bg-gx-bg-sub text-gx-text"
                  : "text-gx-muted hover:text-gx-text")
              }
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      <div className="flex items-center justify-between text-[11px] text-gx-muted">
        <span>
          {selection.count.toLocaleString()} selected · {formatBytes(selection.bytes)} ·{" "}
          {filtered.length.toLocaleString()} match{filtered.length === 1 ? "" : "es"}
        </span>
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="text-gx-accent hover:underline"
            onClick={toggleAllVisible}
            disabled={visiblePaths.length === 0}
          >
            {allVisibleSelected ? "Clear visible" : "Select visible"}
          </button>
          {selSet.size > 0 && (
            <button
              type="button"
              className="text-gx-muted hover:text-gx-text hover:underline"
              onClick={() => onChange([])}
            >
              Clear all
            </button>
          )}
        </div>
      </div>

      <div className="max-h-64 overflow-auto rounded border border-gx-border bg-gx-bg">
        {visible.length === 0 ? (
          <div className="px-3 py-4 text-center text-xs text-gx-muted">
            No tracked files match the current filter.
          </div>
        ) : (
          <ul className="divide-y divide-gx-border/50 text-[11px]">
            {visible.map((e) => {
              const checked = selSet.has(e.path);
              return (
                <li key={e.path}>
                  <label
                    className={
                      "flex cursor-pointer items-center gap-2 px-2 py-1 transition " +
                      (checked ? "bg-gx-accent/10" : "hover:bg-gx-bg-sub")
                    }
                  >
                    <input
                      type="checkbox"
                      className="accent-gx-accent"
                      checked={checked}
                      onChange={(ev) => togglePath(e.path, ev.target.checked)}
                    />
                    <span className="flex-1 truncate font-mono text-gx-text">{e.path}</span>
                    <StateBadge state={e.state} />
                    <span className="shrink-0 text-gx-muted">{formatBytes(e.logical)}</span>
                  </label>
                </li>
              );
            })}
          </ul>
        )}
        {truncated && (
          <div className="border-t border-gx-border px-3 py-1.5 text-[11px] text-gx-muted">
            Showing first {MAX_ROWS.toLocaleString()} of{" "}
            {filtered.length.toLocaleString()} matches. Refine the filter to narrow down.
          </div>
        )}
      </div>
    </div>
  );
}

function StateBadge({ state }: { state: HydrationEntry["state"] }) {
  const cls = {
    hydrated: "border-gx-success/40 bg-gx-success/10 text-gx-success",
    pointer: "border-gx-border bg-gx-bg-sub text-gx-muted",
    partial: "border-gx-warn/40 bg-gx-warn/10 text-gx-warn",
    missing: "border-gx-error/40 bg-gx-error/10 text-gx-error",
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
