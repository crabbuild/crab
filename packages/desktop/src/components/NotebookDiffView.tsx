import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useEffect, useRef, useState } from "react";
import { BookOpen, ChevronDown, ChevronRight, Eye, EyeOff, Loader2 } from "lucide-react";
import type { NotebookComparison } from "../ipc";
import NotebookWorker from "../lib/notebook-diff.worker?worker";
import type { NotebookDiffReply, RenderNotebookDiff } from "../lib/notebook-diff.worker";

/** Render a captured comparison, never separate live reads of the two sides. */
export function NotebookDiffView({ data, onRefresh }: { data: NotebookComparison; onRefresh?: () => void }) {
  const [result, setResult] = useState<NotebookDiffReply | null>(null);
  const [attempt, setAttempt] = useState(0);
  const cancel = useRef<(() => void) | null>(null);
  useEffect(() => {
    setResult(null);
    let worker: Worker | null = null;
    let active = true;
    const stop = () => { active = false; worker?.terminate(); };
    cancel.current = stop;
    try {
      worker = new NotebookWorker();
      worker.onmessage = (event: MessageEvent<NotebookDiffReply>) => {
        if (!active) return;
        stop();
        setResult(event.data);
      };
      worker.onerror = worker.onmessageerror = () => {
        if (!active) return;
        stop();
        setResult({ error: "The notebook comparison worker failed." });
      };
      worker.postMessage({ before: data.before, after: data.after });
    } catch (error) {
      stop();
      setResult({ error: error instanceof Error ? error.message : String(error) });
    }
    return () => {
      stop();
      cancel.current = null;
    };
  }, [data, attempt]);

  if (!result) return <div className="flex items-center justify-center gap-2 p-6 text-sm text-muted-foreground"><Loader2 size={14} className="animate-spin" /> Comparing notebook cells…
    <Button variant="outline" onClick={() => { cancel.current?.(); setResult({ error: "Notebook comparison cancelled." }); }}>Cancel notebook comparison</Button>
  </div>;
  if ("error" in result) return <div role="alert" className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{result.error}
    <Button variant="outline" className="ml-2" onClick={onRefresh ?? (() => setAttempt(attempt + 1))}>Retry notebook comparison</Button>
  </div>;
  const status = data.before === null ? data.after === null ? "File absent in both versions" : "File added" : data.after === null ? "File deleted" : null;
  return <NotebookCells key={data.path} diff={result.diff} status={status} />;
}

const CELL_PAGE_SIZE = 25;

function NotebookCells({ diff, status }: { diff: RenderNotebookDiff; status: string | null }) {
  const [showUnchanged, setShowUnchanged] = useState(false);
  const [showOutputs, setShowOutputs] = useState(false);
  const [page, setPage] = useState(0);
  const changed = diff.cells.filter((cell) => cell.op !== "unchanged");
  const unchanged = diff.cells.length - changed.length;
  const visible = showUnchanged ? diff.cells : changed;
  const hasOutputs = diff.cells.some((entry) => entry.cell.outputs !== null || (entry.oldCell !== undefined && entry.oldCell.outputs !== null));
  const pages = Math.max(1, Math.ceil(visible.length / CELL_PAGE_SIZE));
  const pageIndex = Math.min(page, pages - 1);
  return (
    <section aria-label="Notebook comparison" className="flex flex-col gap-3 p-3 text-xs">
      <div className="flex flex-wrap items-center gap-2 text-muted-foreground">
        <BookOpen size={13} />
        {status && <span className="chip text-foreground">{status}</span>}
        <span>{changed.length} changed cell{changed.length === 1 ? "" : "s"}</span>
        <span>• {unchanged} unchanged</span>
        {unchanged > 0 && <Button variant="outline" aria-pressed={showUnchanged} onClick={() => { setShowUnchanged(!showUnchanged); setPage(0); }}>
          {showUnchanged ? "Hide unchanged cells" : "Show unchanged cells"}
        </Button>}
        {hasOutputs && <Button variant="outline" className="ml-auto" onClick={() => setShowOutputs(!showOutputs)}>
          {showOutputs ? <EyeOff size={12} /> : <Eye size={12} />}{showOutputs ? "Hide all outputs" : "Show all outputs"}
        </Button>}
      </div>
      {diff.metadataChanged && <details className="rounded border border-border p-2">
        <summary className="cursor-pointer text-primary">Notebook metadata or format changed</summary>
        <TextPair label="Notebook details" before={diff.beforeDetails} after={diff.afterDetails} />
      </details>}
      {visible.length === 0 && <p className="py-4 text-muted-foreground">No cell changes.{unchanged > 0 ? " Unchanged cells are available above." : ""}</p>}
      <Pager label="Notebook cells" page={pageIndex} pages={pages} setPage={setPage} />
      {visible.slice(pageIndex * CELL_PAGE_SIZE, (pageIndex + 1) * CELL_PAGE_SIZE).map((entry, index) => (
        <CellCard key={`${pageIndex}-${showUnchanged}-${index}`} entry={entry} showOutputs={showOutputs} />
      ))}
    </section>
  );
}

function CellCard({ entry, showOutputs }: { entry: RenderNotebookDiff["cells"][number]; showOutputs: boolean }) {
  const [expanded, setExpanded] = useState(true);
  return <article aria-label={`${entry.op} cell ${entry.index + 1}`} className="overflow-hidden rounded border border-border bg-card">
    <button className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs hover:bg-background" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
      {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
      <span className="chip">{entry.cell.cell_type}</span><span>Cell {entry.index + 1}</span><span className="ml-auto text-muted-foreground">{entry.op}</span>
    </button>
    {expanded && <div className="flex flex-col gap-3 border-t border-border p-3">
      {entry.sourceChanged && entry.oldCell ? <TextPair label="Source" before={entry.oldCell.source} after={entry.cell.source} /> : <TextPage label="Source" text={entry.cell.source} />}
      {entry.detailsChanged && <details>
        <summary className="cursor-pointer text-primary">Cell details changed</summary>
        <TextPair label="Cell details" before={entry.oldCell?.details ?? (entry.op === "removed" ? entry.cell.details : null)} after={entry.op === "removed" ? null : entry.cell.details} />
      </details>}
      {(entry.outputsChanged || entry.cell.outputs !== null) && <Outputs key={String(showOutputs)} entry={entry} initiallyExpanded={showOutputs} />}
    </div>}
  </article>;
}

function Outputs({ entry, initiallyExpanded }: { entry: RenderNotebookDiff["cells"][number]; initiallyExpanded: boolean }) {
  const [expanded, setExpanded] = useState(initiallyExpanded);
  return <div className="border-t border-border pt-2">
    <button className="inline-flex items-center gap-2 text-primary" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
      {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />} {expanded ? "Hide output diff" : "Show output diff"}
    </button>
    {expanded && <TextPair label="Outputs" before={entry.oldCell?.outputs ?? (entry.op === "removed" ? entry.cell.outputs : null)} after={entry.op === "removed" ? null : entry.cell.outputs} />}
  </div>;
}

function TextPair({ label, before, after }: { label: string; before: string | null; after: string | null }) {
  return <div className="mt-2 grid min-w-0 gap-2 md:grid-cols-2">
    <TextPage label={`Before ${label}`} text={before} />
    <TextPage label={`After ${label}`} text={after} />
  </div>;
}

const TEXT_PAGE_SIZE = 4096;

function TextPage({ label, text }: { label: string; text: string | null }) {
  const [page, setPage] = useState(0);
  const pages = Math.max(1, Math.ceil((text?.length ?? 0) / TEXT_PAGE_SIZE));
  const pageIndex = Math.min(page, pages - 1);
  // Keep a surrogate pair together when it straddles a page boundary.
  const offset = (value: number) => {
    const index = value * TEXT_PAGE_SIZE;
    return text && index > 0 && index < text.length && /[\uDC00-\uDFFF]/.test(text[index]) && /[\uD800-\uDBFF]/.test(text[index - 1]) ? index - 1 : index;
  };
  return <section aria-label={label} className="min-w-0 rounded border border-border bg-background p-2">
    <div className="mb-1 text-[10px] font-medium text-muted-foreground">{label}</div>
    <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] text-foreground">{text === null ? "Absent" : text.length ? text.slice(offset(pageIndex), offset(pageIndex + 1)) : "Empty"}</pre>
    <Pager label={label} page={pageIndex} pages={pages} setPage={setPage} />
  </section>;
}

function Pager({ label, page, pages, setPage }: { label: string; page: number; pages: number; setPage: (page: number) => void }) {
  if (pages <= 1) return null;
  return <nav aria-label={`${label} pages`} className="flex items-center justify-end gap-2 py-1 text-[11px] text-muted-foreground">
    <Button variant="outline" disabled={page === 0} aria-label={`Previous ${label.toLowerCase()} page`} onClick={() => setPage(page - 1)}>Previous</Button>
    <label className="flex items-center gap-1">Page
      <Input type="number" aria-label={`${label} page`} min={1} max={pages} value={page + 1}
        className="w-16 rounded border border-border bg-background px-1 py-0.5 text-foreground"
        onChange={(event) => { const value = event.currentTarget.valueAsNumber; if (Number.isInteger(value) && value >= 1 && value <= pages) setPage(value - 1); }} />
      of {pages}
    </label>
    <Button variant="outline" disabled={page + 1 === pages} aria-label={`Next ${label.toLowerCase()} page`} onClick={() => setPage(page + 1)}>Next</Button>
  </nav>;
}
