import { useState } from "react";
import { ChevronDown, ChevronRight, Download, Upload } from "lucide-react";
import { formatBytes } from "../../lib/format";
import { HydrationEntry } from "../../ipc";
import { ResolvedScope } from "../../lib/hydrateScope";

interface Props {
  hydrate: ResolvedScope;
  dehydrate: ResolvedScope;
}

/**
 * Live preview panel for the Hydrate tab. Shows what each verb would
 * affect for the currently-selected scope, side by side. The matched
 * file list is collapsed by default — most of the time the user only
 * needs the count + total bytes; the list is there for the cases
 * where they want to spot-check before pulling the trigger.
 */
export function HydrateScopePreview({ hydrate, dehydrate }: Props) {
  const [expanded, setExpanded] = useState<"hydrate" | "dehydrate" | null>(null);
  const total = hydrate.matched.length + dehydrate.matched.length;

  return (
    <div className="rounded border border-gx-border bg-gx-bg-sub">
      <div className="grid grid-cols-2 divide-x divide-gx-border">
        <PreviewCell
          icon={<Download size={12} />}
          label="Will hydrate"
          resolved={hydrate}
          expanded={expanded === "hydrate"}
          onToggle={() => setExpanded(expanded === "hydrate" ? null : "hydrate")}
        />
        <PreviewCell
          icon={<Upload size={12} />}
          label="Will dehydrate"
          resolved={dehydrate}
          expanded={expanded === "dehydrate"}
          onToggle={() => setExpanded(expanded === "dehydrate" ? null : "dehydrate")}
        />
      </div>

      {expanded === "hydrate" && <FileList entries={hydrate.matched} skipped={hydrate.skipped} />}
      {expanded === "dehydrate" && (
        <FileList entries={dehydrate.matched} skipped={dehydrate.skipped} />
      )}

      {total === 0 && (
        <div className="border-t border-gx-border px-3 py-1.5 text-[11px] text-gx-muted">
          Nothing in the current scope is eligible. Adjust the scope above.
        </div>
      )}
    </div>
  );
}

function PreviewCell({
  icon,
  label,
  resolved,
  expanded,
  onToggle,
}: {
  icon: React.ReactNode;
  label: string;
  resolved: ResolvedScope;
  expanded: boolean;
  onToggle: () => void;
}) {
  const isEmpty = resolved.matched.length === 0;
  return (
    <button
      type="button"
      onClick={onToggle}
      disabled={isEmpty && resolved.skipped.length === 0}
      className="flex w-full items-center justify-between gap-2 px-3 py-2 text-left text-xs transition hover:bg-gx-bg/50 disabled:cursor-default disabled:hover:bg-transparent"
    >
      <span className="flex items-center gap-2 text-gx-muted">
        {icon}
        <span>{label}</span>
      </span>
      <span className="flex items-center gap-2">
        <span className={isEmpty ? "text-gx-muted" : "font-mono text-gx-text"}>
          {resolved.matched.length.toLocaleString()}{" "}
          {resolved.matched.length === 1 ? "file" : "files"}
        </span>
        <span className="font-mono text-gx-muted">{formatBytes(resolved.bytes)}</span>
        {resolved.skipped.length > 0 && (
          <span className="text-[10px] text-gx-muted" title={`${resolved.skipped.length} already in target state`}>
            ({resolved.skipped.length} skipped)
          </span>
        )}
        {!isEmpty || resolved.skipped.length > 0 ? (
          expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />
        ) : null}
      </span>
    </button>
  );
}

function FileList({
  entries,
  skipped,
}: {
  entries: HydrationEntry[];
  skipped: HydrationEntry[];
}) {
  const MAX = 200;
  const truncated = entries.length > MAX;
  const shown = truncated ? entries.slice(0, MAX) : entries;

  return (
    <div className="max-h-48 overflow-auto border-t border-gx-border bg-gx-bg px-3 py-2 font-mono text-[11px]">
      {shown.length === 0 && skipped.length === 0 ? (
        <div className="text-gx-muted">No files matched.</div>
      ) : (
        <>
          {shown.map((e) => (
            <div key={e.path} className="flex items-center justify-between gap-3 py-px">
              <span className="truncate text-gx-text">{e.path}</span>
              <span className="shrink-0 text-gx-muted">{formatBytes(e.logical)}</span>
            </div>
          ))}
          {truncated && (
            <div className="pt-1 text-gx-muted">
              … and {(entries.length - MAX).toLocaleString()} more
            </div>
          )}
          {skipped.length > 0 && (
            <div className="mt-2 border-t border-gx-border pt-1.5 text-[10px] text-gx-muted">
              {skipped.length.toLocaleString()} already in target state, will be skipped.
            </div>
          )}
        </>
      )}
    </div>
  );
}
