import { useEffect, useState, useCallback, createContext, useContext } from "react";
import {
  BookOpen,
  ChevronDown,
  ChevronRight,
  Code2,
  Eye,
  EyeOff,
  FileText,
  Loader2,
  Minus,
  Plus,
  RefreshCw,
} from "lucide-react";
import { OpenRepo } from "../state";
import { rpc } from "../ipc";
import { diffNotebooks, type CellDiff, type NotebookDiff } from "../lib/nbdiff";

interface Props {
  repo: OpenRepo;
  path: string;
  base?: string | null;
  head?: string | null;
}

/** Context for global output visibility override. */
const OutputVisibilityContext = createContext<{
  globalOverride: boolean | null;
  setGlobalOverride: (v: boolean | null) => void;
}>({ globalOverride: null, setGlobalOverride: () => {} });

/**
 * Notebook-aware diff view that renders cell-level changes for
 * `.ipynb` files. Fetches both sides via `readText` at the given
 * revisions and runs `diffNotebooks` to produce a structured patch.
 */
export function NotebookDiffView({ repo, path, base, head }: Props) {
  const [diff, setDiff] = useState<NotebookDiff | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [globalOverride, setGlobalOverride] = useState<boolean | null>(null);

  useEffect(() => {
    let cancelled = false;
    setDiff(null);
    setErr(null);
    setLoading(true);

    const fetchAndDiff = async () => {
      try {
        // Fetch old side (base revision, defaults to HEAD)
        const oldRev = base ?? "HEAD";
        let oldJson = "";
        try {
          const oldRes = await rpc.readText(repo.repo_id, path, { rev: oldRev });
          oldJson = oldRes.text ?? "";
        } catch {
          // File might not exist in the base revision (new file)
          oldJson = "";
        }

        // Fetch new side (head revision, or working tree if null)
        let newJson = "";
        try {
          const newRes = await rpc.readText(repo.repo_id, path, {
            rev: head ?? undefined,
          });
          newJson = newRes.text ?? "";
        } catch {
          // File might have been deleted
          newJson = "";
        }

        if (!cancelled) {
          const result = diffNotebooks(oldJson, newJson);
          setDiff(result);
          setLoading(false);
        }
      } catch (e) {
        if (!cancelled) {
          setErr(e instanceof Error ? e.message : String(e));
          setLoading(false);
        }
      }
    };

    void fetchAndDiff();
    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, path, base, head]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Diffing notebook cells…
      </div>
    );
  }

  if (err) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
        {err}
      </div>
    );
  }

  if (!diff) return null;

  const changedCells = diff.cells.filter((c) => c.op !== "unchanged");
  const unchangedCount = diff.cells.length - changedCells.length;
  const outputChangedCount = diff.cells.filter((c) => c.outputsChanged).length;

  return (
    <OutputVisibilityContext.Provider value={{ globalOverride, setGlobalOverride }}>
      <div className="flex flex-col gap-2 p-3">
        {/* Summary bar */}
        <div className="flex items-center gap-2 text-xs text-gx-muted">
          <BookOpen size={12} />
          <span>
            {diff.cells.length} cell{diff.cells.length !== 1 ? "s" : ""} total
          </span>
          {changedCells.length > 0 && (
            <span className="text-gx-warning">
              • {changedCells.length} changed
            </span>
          )}
          {diff.metadataChanged && (
            <span className="text-gx-accent">• metadata changed</span>
          )}
          {outputChangedCount > 0 && (
            <GlobalOutputToggle outputChangedCount={outputChangedCount} />
          )}
        </div>

        {/* Cell diffs */}
        <CellDiffList cells={diff.cells} unchangedCount={unchangedCount} />
      </div>
    </OutputVisibilityContext.Provider>
  );
}

// --- Sub-components ---

function GlobalOutputToggle({ outputChangedCount }: { outputChangedCount: number }) {
  const { globalOverride, setGlobalOverride } = useContext(OutputVisibilityContext);

  const handleClick = useCallback(() => {
    if (globalOverride === true) {
      // Currently showing all → hide all
      setGlobalOverride(false);
    } else {
      // Currently hidden or mixed → show all
      setGlobalOverride(true);
    }
  }, [globalOverride, setGlobalOverride]);

  const isShowing = globalOverride === true;

  return (
    <button
      type="button"
      className="ml-auto inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium text-gx-muted hover:bg-gx-surface hover:text-gx-text"
      onClick={handleClick}
      aria-label={isShowing ? "Hide all outputs" : "Show all outputs"}
    >
      {isShowing ? <EyeOff size={10} /> : <Eye size={10} />}
      {isShowing ? "Hide all outputs" : `Show all outputs (${outputChangedCount})`}
    </button>
  );
}

function CellDiffList({
  cells,
  unchangedCount: _unchangedCount,
}: {
  cells: CellDiff[];
  unchangedCount: number;
}) {
  // Group consecutive unchanged cells into collapsed sections
  const groups: Array<CellDiff | { kind: "collapsed"; count: number }> = [];
  let consecutiveUnchanged = 0;

  for (const cell of cells) {
    if (cell.op === "unchanged") {
      consecutiveUnchanged++;
    } else {
      if (consecutiveUnchanged > 0) {
        groups.push({ kind: "collapsed", count: consecutiveUnchanged });
        consecutiveUnchanged = 0;
      }
      groups.push(cell);
    }
  }
  if (consecutiveUnchanged > 0) {
    groups.push({ kind: "collapsed", count: consecutiveUnchanged });
  }

  return (
    <div className="flex flex-col gap-2">
      {groups.map((item, i) => {
        if ("kind" in item && item.kind === "collapsed") {
          return <CollapsedCells key={`collapsed-${i}`} count={item.count} />;
        }
        const cell = item as CellDiff;
        return <CellDiffCard key={`${cell.op}-${cell.index}-${i}`} entry={cell} />;
      })}
    </div>
  );
}

function CollapsedCells({ count }: { count: number }) {
  return (
    <div className="flex items-center justify-center gap-1.5 rounded border border-gx-border/50 bg-gx-surface/50 px-3 py-1.5 text-[11px] text-gx-muted">
      <RefreshCw size={10} />
      {count} unchanged cell{count !== 1 ? "s" : ""}
    </div>
  );
}

function CellDiffCard({ entry }: { entry: CellDiff }) {
  const [expanded, setExpanded] = useState(true);

  const borderColor = {
    added: "border-gx-success/50",
    removed: "border-gx-error/50",
    modified: "border-gx-warn/50",
    unchanged: "border-gx-border",
  }[entry.op];

  const bgColor = {
    added: "bg-gx-success/5",
    removed: "bg-gx-error/5",
    modified: "bg-gx-warn/5",
    unchanged: "bg-gx-surface",
  }[entry.op];

  const opIcon = {
    added: <Plus size={11} className="text-gx-success" />,
    removed: <Minus size={11} className="text-gx-error" />,
    modified: <RefreshCw size={11} className="text-gx-warn" />,
    unchanged: null,
  }[entry.op];

  const cellTypeIcon =
    entry.cell.cell_type === "code" ? (
      <Code2 size={11} />
    ) : (
      <FileText size={11} />
    );

  return (
    <div className={`rounded border ${borderColor} ${bgColor} overflow-hidden`}>
      {/* Header */}
      <button
        type="button"
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[11px] hover:bg-gx-surface/50"
        onClick={() => setExpanded(!expanded)}
      >
        {expanded ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
        {opIcon}
        <span className="inline-flex items-center gap-1 rounded bg-gx-surface px-1.5 py-0.5 text-[10px] font-medium text-gx-muted">
          {cellTypeIcon}
          {entry.cell.cell_type}
        </span>
        <span className="text-gx-muted">
          Cell {entry.index}
        </span>
        <span className="ml-auto text-[10px] font-medium uppercase text-gx-muted">
          {entry.op}
        </span>
      </button>

      {/* Body */}
      {expanded && (
        <div className="border-t border-gx-border/30 px-2.5 py-2">
          {entry.op === "modified" && entry.sourceChanged && entry.oldCell ? (
            <ModifiedCellSource oldCell={entry.oldCell} newCell={entry.cell} />
          ) : (
            <CellSource cell={entry.cell} />
          )}

          {/* Output diff toggle */}
          {entry.outputsChanged && (
            <OutputDiffSection entry={entry} />
          )}
        </div>
      )}
    </div>
  );
}

/** Maximum characters to show before truncating output content. */
const OUTPUT_TRUNCATE_LIMIT = 500;

function OutputDiffSection({ entry }: { entry: CellDiff }) {
  const { globalOverride } = useContext(OutputVisibilityContext);
  const [localExpanded, setLocalExpanded] = useState(false);

  // Global override takes precedence over local state
  const isExpanded = globalOverride !== null ? globalOverride : localExpanded;

  return (
    <div className="mt-2 border-t border-gx-border/20 pt-2">
      <button
        type="button"
        className="flex items-center gap-1.5 rounded px-1.5 py-1 text-[11px] text-gx-muted hover:bg-gx-surface/50 hover:text-gx-text"
        onClick={() => setLocalExpanded(!isExpanded)}
        aria-label={isExpanded ? "Hide output diff" : "Show output diff"}
      >
        {isExpanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
        <Eye size={10} className="text-gx-warn" />
        <span className="font-medium">Outputs changed</span>
      </button>

      {isExpanded && (
        <div className="mt-1.5 flex flex-col gap-1.5 pl-4">
          {/* Old outputs (removed) */}
          {entry.oldCell && entry.oldCell.outputs.length > 0 && (
            <OutputBlock
              label="removed"
              outputs={entry.oldCell.outputs}
              variant="removed"
            />
          )}
          {/* New outputs (added) */}
          {entry.cell.outputs.length > 0 && (
            <OutputBlock
              label="added"
              outputs={entry.cell.outputs}
              variant="added"
            />
          )}
          {/* Handle case where outputs were cleared */}
          {entry.cell.outputs.length === 0 && (!entry.oldCell || entry.oldCell.outputs.length === 0) && (
            <span className="text-[11px] italic text-gx-muted">outputs cleared</span>
          )}
        </div>
      )}
    </div>
  );
}

function OutputBlock({
  label,
  outputs,
  variant,
}: {
  label: string;
  outputs: unknown[];
  variant: "added" | "removed";
}) {
  const [showFull, setShowFull] = useState(false);

  const borderColor = variant === "removed" ? "border-gx-error/20" : "border-gx-success/20";
  const bgColor = variant === "removed" ? "bg-gx-error/5" : "bg-gx-success/5";
  const labelColor = variant === "removed" ? "text-gx-error" : "text-gx-success";

  const text = JSON.stringify(outputs, null, 2);
  const isTruncated = text.length > OUTPUT_TRUNCATE_LIMIT && !showFull;
  const displayText = isTruncated ? text.slice(0, OUTPUT_TRUNCATE_LIMIT) : text;

  return (
    <div className={`rounded border ${borderColor} ${bgColor} p-2`}>
      <div className={`mb-1 text-[10px] font-medium uppercase ${labelColor}`}>
        {label}
      </div>
      <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11px] leading-relaxed text-gx-text">
        {displayText}
        {isTruncated && "…"}
      </pre>
      {isTruncated && (
        <button
          type="button"
          className="mt-1 text-[10px] font-medium text-gx-accent hover:underline"
          onClick={() => setShowFull(true)}
        >
          show more
        </button>
      )}
    </div>
  );
}

function CellSource({ cell }: { cell: { source: string[] } }) {
  const text = cell.source.join("");
  if (!text.trim()) {
    return <span className="text-[11px] italic text-gx-muted">empty cell</span>;
  }
  return (
    <pre className="overflow-x-auto whitespace-pre-wrap rounded bg-gx-bg-sub p-2 font-mono text-[11px] leading-relaxed text-gx-text">
      {text}
    </pre>
  );
}

function ModifiedCellSource({
  oldCell,
  newCell,
}: {
  oldCell: { source: string[] };
  newCell: { source: string[] };
}) {
  const oldText = oldCell.source.join("");
  const newText = newCell.source.join("");

  return (
    <div className="flex flex-col gap-1.5">
        <div className="rounded border border-gx-error/20 bg-gx-error/5 p-2">
        <div className="mb-1 text-[10px] font-medium uppercase text-gx-error">
          removed
        </div>
        <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11px] leading-relaxed text-gx-text">
          {oldText || <span className="italic text-gx-muted">empty</span>}
        </pre>
      </div>
      <div className="rounded border border-gx-success/20 bg-gx-success/5 p-2">
        <div className="mb-1 text-[10px] font-medium uppercase text-gx-success">
          added
        </div>
        <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-[11px] leading-relaxed text-gx-text">
          {newText || <span className="italic text-gx-muted">empty</span>}
        </pre>
      </div>
    </div>
  );
}
