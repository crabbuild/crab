/**
 * ReferencesPanel — bottom drawer component listing LSP reference results.
 *
 * Shows file path, line number, and a context snippet for each reference.
 * Clicking a reference navigates to that file and line.
 */

import { useCallback, useMemo } from "react";
import { FileText, X } from "lucide-react";

export interface ReferenceItem {
  uri: string;
  line: number;
  character: number;
  /** Preview snippet of the line content. */
  preview?: string;
}

interface Props {
  references: ReferenceItem[];
  /** Called when the user clicks a reference to navigate to it. */
  onNavigate: (uri: string, line: number) => void;
  /** Called when the panel is dismissed. */
  onClose: () => void;
  /** Optional title (e.g., "3 references" or "Rename Preview"). */
  title?: string;
}

export function ReferencesPanel({ references, onNavigate, onClose, title }: Props) {
  // Group references by file
  const grouped = useMemo((): Array<[string, ReferenceItem[]]> => {
    const map = new Map<string, ReferenceItem[]>();
    for (const ref of references) {
      const existing = map.get(ref.uri) ?? [];
      existing.push(ref);
      map.set(ref.uri, existing);
    }
    return Array.from(map.entries());
  }, [references]);

  const handleClick = useCallback(
    (uri: string, line: number) => {
      onNavigate(uri, line);
    },
    [onNavigate],
  );

  if (references.length === 0) {
    return null;
  }

  const displayTitle = title ?? `${references.length} reference${references.length !== 1 ? "s" : ""}`;

  return (
    <div className="flex flex-col border-t border-gx-border bg-gx-surface max-h-[240px]">
      {/* Header */}
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-1.5">
        <span className="text-[11px] font-medium text-gx-text">{displayTitle}</span>
        <button
          type="button"
          onClick={onClose}
          className="rounded p-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
          aria-label="Close references panel"
        >
          <X size={12} />
        </button>
      </div>

      {/* Results list */}
      <div className="flex-1 overflow-y-auto scrollbar-thin">
        {grouped.map(([uri, refs]) => {
          const displayPath = uriToDisplayPath(uri);
          return (
            <div key={uri}>
              {/* File header */}
              <div className="flex items-center gap-1.5 px-3 py-1 bg-gx-bg-sub sticky top-0">
                <FileText size={11} className="shrink-0 text-gx-muted" />
                <span className="text-[11px] text-gx-muted truncate" title={displayPath}>
                  {displayPath}
                </span>
                <span className="text-[10px] text-gx-muted/60 ml-auto shrink-0">
                  {refs.length}
                </span>
              </div>

              {/* References in this file */}
              {refs.map((ref, idx) => (
                <button
                  key={`${ref.line}-${ref.character}-${idx}`}
                  type="button"
                  onClick={() => handleClick(ref.uri, ref.line)}
                  className="flex w-full items-baseline gap-2 px-3 py-0.5 text-left hover:bg-gx-surface-hover transition text-[11px]"
                >
                  <span className="shrink-0 text-gx-accent tabular-nums w-8 text-right">
                    {ref.line}
                  </span>
                  <span className="text-gx-text truncate font-mono text-[10.5px]">
                    {ref.preview ?? `Line ${ref.line}`}
                  </span>
                </button>
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** Strip file:// prefix and show a relative-ish path. */
function uriToDisplayPath(uri: string): string {
  const path = uri.replace(/^file:\/\//, "");
  // Show last 3 segments for readability
  const segments = path.split("/");
  if (segments.length > 3) {
    return "…/" + segments.slice(-3).join("/");
  }
  return path;
}
