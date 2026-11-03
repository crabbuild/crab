/**
 * ProblemsPanel — bottom drawer listing all LSP diagnostics across open files.
 *
 * Diagnostics are grouped by file. Clicking a problem navigates to the
 * file and line. Shows error/warning/info counts in the header.
 */

import { useCallback, useMemo } from "react";
import {
  AlertCircle,
  AlertTriangle,
  ChevronDown,
  Info,
  X,
} from "lucide-react";
import {
  useAllDiagnostics,
  useTotalDiagnosticCounts,
  type DiagnosticEntry,
} from "../lib/lsp-diagnostics-store";

interface Props {
  /** Called when the user clicks a diagnostic to navigate to it. */
  onNavigate: (uri: string, line: number, column: number) => void;
  /** Called when the panel is dismissed. */
  onClose: () => void;
}

export function ProblemsPanel({ onNavigate, onClose }: Props) {
  const allDiagnostics = useAllDiagnostics();
  const totalCounts = useTotalDiagnosticCounts();

  const sortedFiles = useMemo(() => {
    const entries = Array.from(allDiagnostics.entries());
    // Sort files with errors first, then by path
    return entries.sort(([, a], [, b]) => {
      const aHasErrors = a.some((d) => d.severity === 8);
      const bHasErrors = b.some((d) => d.severity === 8);
      if (aHasErrors && !bHasErrors) return -1;
      if (!aHasErrors && bHasErrors) return 1;
      return 0;
    });
  }, [allDiagnostics]);

  const handleClick = useCallback(
    (entry: DiagnosticEntry) => {
      onNavigate(entry.uri, entry.startLineNumber, entry.startColumn);
    },
    [onNavigate],
  );

  const totalCount = totalCounts.errors + totalCounts.warnings + totalCounts.infos + totalCounts.hints;

  return (
    <div className="flex flex-col border-t border-gx-border bg-gx-surface max-h-[280px]">
      {/* Header */}
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-1.5">
        <div className="flex items-center gap-3">
          <span className="text-[11px] font-medium text-gx-text">Problems</span>
          <div className="flex items-center gap-2 text-[10px]">
            {totalCounts.errors > 0 && (
              <span className="flex items-center gap-0.5 text-gx-error">
                <AlertCircle size={10} />
                {totalCounts.errors}
              </span>
            )}
            {totalCounts.warnings > 0 && (
              <span className="flex items-center gap-0.5 text-gx-warn">
                <AlertTriangle size={10} />
                {totalCounts.warnings}
              </span>
            )}
            {totalCounts.infos > 0 && (
              <span className="flex items-center gap-0.5 text-gx-muted">
                <Info size={10} />
                {totalCounts.infos}
              </span>
            )}
          </div>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="rounded p-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
          aria-label="Close problems panel"
        >
          <X size={12} />
        </button>
      </div>

      {/* Content */}
      {totalCount === 0 ? (
        <div className="flex items-center justify-center py-6 text-xs text-gx-muted">
          No problems detected
        </div>
      ) : (
        <div className="flex-1 overflow-y-auto scrollbar-thin">
          {sortedFiles.map(([uri, diagnostics]) => (
            <FileGroup
              key={uri}
              uri={uri}
              diagnostics={diagnostics}
              onClick={handleClick}
            />
          ))}
        </div>
      )}
    </div>
  );
}

// ── File Group ──────────────────────────────────────────────────────

function FileGroup({
  uri,
  diagnostics,
  onClick,
}: {
  uri: string;
  diagnostics: DiagnosticEntry[];
  onClick: (entry: DiagnosticEntry) => void;
}) {
  const displayPath = uriToDisplayPath(uri);
  const errorCount = diagnostics.filter((d) => d.severity === 8).length;
  const warnCount = diagnostics.filter((d) => d.severity === 4).length;

  // Sort: errors first, then warnings, then by line
  const sorted = useMemo(
    () =>
      [...diagnostics].sort((a, b) => {
        if (a.severity !== b.severity) return b.severity - a.severity;
        return a.startLineNumber - b.startLineNumber;
      }),
    [diagnostics],
  );

  return (
    <div>
      {/* File header */}
      <div className="flex items-center gap-1.5 px-3 py-1 bg-gx-bg-sub sticky top-0 z-10">
        <ChevronDown size={10} className="shrink-0 text-gx-muted" />
        <span className="text-[11px] text-gx-text truncate flex-1" title={displayPath}>
          {displayPath}
        </span>
        <div className="flex items-center gap-1.5 shrink-0 text-[10px]">
          {errorCount > 0 && (
            <span className="text-gx-error">{errorCount}</span>
          )}
          {warnCount > 0 && (
            <span className="text-gx-warn">{warnCount}</span>
          )}
        </div>
      </div>

      {/* Diagnostics */}
      {sorted.map((entry, idx) => (
        <button
          key={`${entry.startLineNumber}-${entry.startColumn}-${idx}`}
          type="button"
          onClick={() => onClick(entry)}
          className="flex w-full items-start gap-2 px-3 py-0.5 pl-6 text-left hover:bg-gx-surface-hover transition text-[11px]"
        >
          <SeverityIcon severity={entry.severity} />
          <span className="flex-1 text-gx-text truncate">{entry.message}</span>
          <span className="shrink-0 text-gx-muted tabular-nums text-[10px]">
            [{entry.startLineNumber}:{entry.startColumn}]
          </span>
          {entry.source && (
            <span className="shrink-0 text-gx-muted/60 text-[10px]">
              {entry.source}
            </span>
          )}
        </button>
      ))}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function SeverityIcon({ severity }: { severity: number }) {
  switch (severity) {
    case 8:
      return <AlertCircle size={11} className="shrink-0 text-gx-error mt-px" />;
    case 4:
      return <AlertTriangle size={11} className="shrink-0 text-gx-warn mt-px" />;
    case 2:
      return <Info size={11} className="shrink-0 text-gx-muted mt-px" />;
    default:
      return <Info size={11} className="shrink-0 text-gx-muted/50 mt-px" />;
  }
}

function uriToDisplayPath(uri: string): string {
  const path = uri.replace(/^file:\/\//, "");
  const segments = path.split("/");
  if (segments.length > 3) {
    return "…/" + segments.slice(-3).join("/");
  }
  return path;
}
