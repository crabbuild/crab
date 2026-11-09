import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  Database,
  File,
  FileText,
  Filter,
  Loader2,
  RefreshCw,
  Search,
  X,
} from "lucide-react";
import { OpenRepo } from "../state";
import {
  DirEntry,
  ParquetPreview,
  SafetensorsPreview,
  rpc,
} from "../ipc";
import { classifyFile, DatasetKind } from "../lib/dataset-kind";
import { useDatasetRules } from "../lib/useDatasetRules";
import { formatBytes } from "../lib/format";

interface Props {
  repo: OpenRepo;
  onOpenFile: (path: string, entry: DirEntry) => void;
}

interface ClassifiedEntry extends DirEntry {
  datasetKind: DatasetKind;
}

const KIND_LABELS: Record<DatasetKind, string> = {
  "training-data": "Training Data",
  "eval-data": "Evaluation Data",
  checkpoint: "Checkpoints",
  tokenizer: "Tokenizers",
  notebook: "Notebooks",
  config: "Configuration",
  code: "Code",
  other: "Other",
};

const KIND_ORDER: DatasetKind[] = [
  "checkpoint",
  "training-data",
  "eval-data",
  "tokenizer",
  "notebook",
  "config",
  "code",
  "other",
];

// ---- Filter types ----

interface SizeThreshold {
  label: string;
  bytes: number;
}

const SIZE_THRESHOLDS: SizeThreshold[] = [
  { label: "> 1 MB", bytes: 1_000_000 },
  { label: "> 10 MB", bytes: 10_000_000 },
  { label: "> 100 MB", bytes: 100_000_000 },
  { label: "> 1 GB", bytes: 1_000_000_000 },
];

interface MtimeOption {
  label: string;
  /** Max age in milliseconds. 0 means "all time" (no filter). */
  maxAgeMs: number;
}

const MTIME_OPTIONS: MtimeOption[] = [
  { label: "All time", maxAgeMs: 0 },
  { label: "Last 24h", maxAgeMs: 24 * 60 * 60 * 1000 },
  { label: "Last 7 days", maxAgeMs: 7 * 24 * 60 * 60 * 1000 },
  { label: "Last 30 days", maxAgeMs: 30 * 24 * 60 * 60 * 1000 },
];

interface FilterState {
  /** Minimum size in bytes, or null for no size filter. */
  minSize: number | null;
  /** Set of kinds to show. Empty set means show all. */
  visibleKinds: Set<DatasetKind>;
  /** Max age in ms for mtime filter. 0 means no filter. */
  mtimeMaxAgeMs: number;
}

/**
 * Dataset-aware view that groups repository files by ML kind
 * (checkpoints, training data, notebooks, etc.) and shows per-kind
 * metadata when a file is selected.
 */
export function DatasetsView({ repo, onOpenFile }: Props) {
  const [entries, setEntries] = useState<DirEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [selectedEntry, setSelectedEntry] = useState<ClassifiedEntry | null>(null);
  const [collapsedKinds, setCollapsedKinds] = useState<Set<DatasetKind>>(new Set());
  const [showFilters, setShowFilters] = useState(false);
  const [filters, setFilters] = useState<FilterState>({
    minSize: null,
    visibleKinds: new Set(),
    mtimeMaxAgeMs: 0,
  });

  const rules = useDatasetRules(repo.repo_id);

  const hasActiveFilters =
    filters.minSize !== null ||
    filters.visibleKinds.size > 0 ||
    filters.mtimeMaxAgeMs > 0;

  const fetchEntries = () => {
    setLoading(true);
    setError(null);
    rpc
      .list(repo.repo_id)
      .then((res) => {
        setEntries(res.entries);
        setLoading(false);
      })
      .catch((e) => {
        setError(e instanceof Error ? e.message : String(e));
        setLoading(false);
      });
  };

  useEffect(() => {
    fetchEntries();
  }, [repo.repo_id]);

  const classified = useMemo<ClassifiedEntry[]>(() => {
    if (!entries) return [];
    return entries
      .filter((e) => e.kind === "file" || e.kind === "pointer" || e.kind === "lfs")
      .map((e) => ({
        ...e,
        datasetKind: classifyFile(
          {
            path: e.path,
            size: e.size,
            pointerKind: e.kind === "pointer" ? "crab" : e.kind === "lfs" ? "lfs" : null,
          },
          rules,
        ),
      }));
  }, [entries, rules]);

  const filtered = useMemo(() => {
    let result = classified;

    // Text search filter
    const f = filter.trim().toLowerCase();
    if (f) {
      result = result.filter((e) => e.path.toLowerCase().includes(f));
    }

    // Size filter
    if (filters.minSize !== null) {
      result = result.filter((e) => e.size != null && e.size >= filters.minSize!);
    }

    // Kind filter
    if (filters.visibleKinds.size > 0) {
      result = result.filter((e) => filters.visibleKinds.has(e.datasetKind));
    }

    // Mtime filter — DirEntry doesn't carry mtime, so this filter
    // is a no-op until the listing includes timestamps. The UI still
    // shows the control so users know the capability exists.
    // When mtime becomes available on entries, uncomment:
    // if (filters.mtimeMaxAgeMs > 0) {
    //   const cutoff = Date.now() - filters.mtimeMaxAgeMs;
    //   result = result.filter((e) => e.mtime != null && e.mtime * 1000 >= cutoff);
    // }

    return result;
  }, [classified, filter, filters]);

  const grouped = useMemo(() => {
    const groups = new Map<DatasetKind, ClassifiedEntry[]>();
    for (const entry of filtered) {
      const list = groups.get(entry.datasetKind) ?? [];
      list.push(entry);
      groups.set(entry.datasetKind, list);
    }
    // Return in defined order, only non-empty groups
    return KIND_ORDER
      .filter((k) => groups.has(k))
      .map((k) => ({ kind: k, entries: groups.get(k)! }));
  }, [filtered]);

  const toggleCollapse = (kind: DatasetKind) => {
    setCollapsedKinds((prev) => {
      const next = new Set(prev);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
  };

  const handleFileClick = (entry: ClassifiedEntry) => {
    setSelectedEntry(entry);
    onOpenFile(entry.path, entry);
  };

  const clearFilters = () => {
    setFilters({ minSize: null, visibleKinds: new Set(), mtimeMaxAgeMs: 0 });
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* Header */}
      <div className="shrink-0 border-b border-gx-border px-3 py-2">
        <div className="mb-2 flex items-center justify-between text-[10px] uppercase tracking-wider text-gx-muted">
          <span className="flex items-center gap-1">
            <Database size={11} /> Datasets
          </span>
          <div className="flex items-center gap-1.5">
            <button
              type="button"
              className={clsx(
                "relative text-gx-muted hover:text-gx-text",
                showFilters && "text-gx-accent",
              )}
              onClick={() => setShowFilters((v) => !v)}
              title="Toggle filters"
              aria-label="Toggle filters"
            >
              <Filter size={11} />
              {hasActiveFilters && (
                <span className="absolute -right-0.5 -top-0.5 h-1.5 w-1.5 rounded-full bg-gx-accent" />
              )}
            </button>
            <button
              type="button"
              className="text-gx-muted hover:text-gx-text"
              onClick={fetchEntries}
              title="Refresh"
              aria-label="Refresh datasets"
            >
              <RefreshCw size={11} />
            </button>
          </div>
        </div>
        <div className="relative">
          <Search size={12} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-gx-muted" />
          <input
            className="w-full rounded border border-gx-border bg-gx-bg-sub pl-7 pr-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
            placeholder="Filter files…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </div>

        {/* Filter bar */}
        {showFilters && (
          <FilterBar
            filters={filters}
            onChange={setFilters}
            onClear={clearFilters}
            hasActive={hasActiveFilters}
          />
        )}
      </div>

      {/* File listing */}
      <div className="min-h-0 flex-1 overflow-auto">
        {loading && (
          <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading…
          </div>
        )}
        {error && (
          <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            {error}
          </div>
        )}
        {!loading && !error && grouped.length === 0 && entries && (
          <div className="p-3 text-xs text-gx-muted">No dataset files found.</div>
        )}
        {!loading &&
          !error &&
          grouped.map((group) => (
            <KindGroup
              key={group.kind}
              kind={group.kind}
              entries={group.entries}
              collapsed={collapsedKinds.has(group.kind)}
              onToggle={() => toggleCollapse(group.kind)}
              selectedPath={selectedEntry?.path ?? null}
              onSelect={handleFileClick}
            />
          ))}
      </div>

      {/* Metadata panel */}
      {selectedEntry && (
        <div className="shrink-0 border-t border-gx-border">
          <MetadataPanel repo={repo} entry={selectedEntry} />
        </div>
      )}
    </div>
  );
}

// ---- Kind Group ----

function KindGroup({
  kind,
  entries,
  collapsed,
  onToggle,
  selectedPath,
  onSelect,
}: {
  kind: DatasetKind;
  entries: ClassifiedEntry[];
  collapsed: boolean;
  onToggle: () => void;
  selectedPath: string | null;
  onSelect: (entry: ClassifiedEntry) => void;
}) {
  return (
    <div>
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center gap-1.5 px-3 py-1.5 text-left text-[11px] font-medium text-gx-text hover:bg-gx-surface-hover"
      >
        {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
        <span>{KIND_LABELS[kind]}</span>
        <span className="ml-auto rounded-full bg-gx-accent/20 px-1.5 py-[1px] text-[10px] text-gx-accent">
          {entries.length}
        </span>
      </button>
      {!collapsed && (
        <ul className="pb-1">
          {entries.map((entry) => (
            <li key={entry.path}>
              <button
                type="button"
                onClick={() => onSelect(entry)}
                className={clsx(
                  "flex w-full items-center gap-2 px-5 py-1 text-left text-xs transition",
                  entry.path === selectedPath
                    ? "bg-gx-accent/15 text-gx-text"
                    : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
                )}
              >
                <FileIcon kind={kind} />
                <span className="min-w-0 flex-1 truncate">{fileName(entry.path)}</span>
                {entry.size != null && (
                  <span className="shrink-0 text-[10px] text-gx-muted">
                    {formatBytes(entry.size)}
                  </span>
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function FileIcon({ kind }: { kind: DatasetKind }) {
  switch (kind) {
    case "checkpoint":
      return <Database size={12} className="shrink-0 text-gx-accent" />;
    case "training-data":
    case "eval-data":
      return <Database size={12} className="shrink-0 text-gx-success" />;
    case "notebook":
      return <FileText size={12} className="shrink-0 text-gx-warn" />;
    default:
      return <File size={12} className="shrink-0" />;
  }
}

function fileName(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] ?? path;
}

// ---- Filter Bar ----

function FilterBar({
  filters,
  onChange,
  onClear,
  hasActive,
}: {
  filters: FilterState;
  onChange: (f: FilterState) => void;
  onClear: () => void;
  hasActive: boolean;
}) {
  const toggleKind = (kind: DatasetKind) => {
    const next = new Set(filters.visibleKinds);
    if (next.has(kind)) next.delete(kind);
    else next.add(kind);
    onChange({ ...filters, visibleKinds: next });
  };

  return (
    <div className="mt-2 space-y-2 rounded border border-gx-border bg-gx-bg-sub p-2">
      {/* Size filter */}
      <div>
        <label className="mb-0.5 block text-[10px] text-gx-muted">Min size</label>
        <select
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-1.5 py-0.5 text-xs text-gx-text focus-visible:outline-none focus:border-gx-accent"
          value={filters.minSize ?? ""}
          onChange={(e) =>
            onChange({
              ...filters,
              minSize: e.target.value ? Number(e.target.value) : null,
            })
          }
        >
          <option value="">Any size</option>
          {SIZE_THRESHOLDS.map((t) => (
            <option key={t.bytes} value={t.bytes}>
              {t.label}
            </option>
          ))}
        </select>
      </div>

      {/* Kind filter */}
      <div>
        <label className="mb-0.5 block text-[10px] text-gx-muted">Kind</label>
        <div className="flex flex-wrap gap-1">
          {KIND_ORDER.map((kind) => (
            <button
              key={kind}
              type="button"
              onClick={() => toggleKind(kind)}
              className={clsx(
                "rounded px-1.5 py-[2px] text-[10px] transition",
                filters.visibleKinds.has(kind)
                  ? "bg-gx-accent/20 text-gx-accent"
                  : filters.visibleKinds.size > 0
                    ? "bg-gx-border/40 text-gx-muted hover:bg-gx-border"
                    : "bg-gx-border/40 text-gx-text hover:bg-gx-border",
              )}
            >
              {KIND_LABELS[kind]}
            </button>
          ))}
        </div>
      </div>

      {/* Mtime filter */}
      <div>
        <label className="mb-0.5 block text-[10px] text-gx-muted">
          Modified
          <span className="ml-1 italic text-gx-muted/60">(when available)</span>
        </label>
        <select
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-1.5 py-0.5 text-xs text-gx-text focus-visible:outline-none focus:border-gx-accent"
          value={filters.mtimeMaxAgeMs}
          onChange={(e) =>
            onChange({ ...filters, mtimeMaxAgeMs: Number(e.target.value) })
          }
        >
          {MTIME_OPTIONS.map((opt) => (
            <option key={opt.maxAgeMs} value={opt.maxAgeMs}>
              {opt.label}
            </option>
          ))}
        </select>
      </div>

      {/* Clear button */}
      {hasActive && (
        <button
          type="button"
          onClick={onClear}
          className="flex items-center gap-1 text-[10px] text-gx-muted hover:text-gx-text"
        >
          <X size={10} /> Clear all filters
        </button>
      )}
    </div>
  );
}

// ---- Metadata Panel ----

function MetadataPanel({ repo, entry }: { repo: OpenRepo; entry: ClassifiedEntry }) {
  switch (entry.datasetKind) {
    case "training-data":
    case "eval-data":
      if (entry.path.endsWith(".parquet")) {
        return <ParquetMetadata repo={repo} path={entry.path} />;
      }
      return <BasicMetadata entry={entry} />;
    case "checkpoint":
      if (entry.path.endsWith(".safetensors")) {
        return <SafetensorsMetadata repo={repo} path={entry.path} />;
      }
      return <CheckpointMetadata entry={entry} />;
    case "notebook":
      return <NotebookMetadata entry={entry} />;
    default:
      return <BasicMetadata entry={entry} />;
  }
}

function ParquetMetadata({ repo, path }: { repo: OpenRepo; path: string }) {
  const [preview, setPreview] = useState<ParquetPreview | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    rpc
      .previewParquet(repo.repo_id, path, { max_rows: 0, signal: controller.signal })
      .then((p) => {
        if (controller.signal.aborted) return;
        setPreview(p);
        setLoading(false);
      })
      .catch((e) => {
        if (controller.signal.aborted) return;
        setError(e instanceof Error ? e.message : String(e));
        setLoading(false);
      });
    return () => controller.abort();
  }, [repo.repo_id, path]);

  if (loading) return <MetadataLoading />;
  if (error) return <MetadataError message={error} />;
  if (!preview) return null;

  return (
    <div className="max-h-40 overflow-auto p-3 text-xs">
      <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">Parquet</div>
      <div className="space-y-1">
        <MetaRow label="Rows" value={preview.num_rows.toLocaleString()} />
        <MetaRow label="Columns" value={String(preview.num_columns)} />
        <MetaRow label="Row groups" value={String(preview.num_row_groups)} />
        {preview.columns.length > 0 && (
          <div className="mt-1.5">
            <div className="text-[10px] text-gx-muted">Columns:</div>
            <div className="mt-0.5 flex flex-wrap gap-1">
              {preview.columns.slice(0, 8).map((c) => (
                <span key={c.name} className="chip font-mono text-[10px]">
                  {c.name}
                  <span className="text-gx-muted">:{c.type}</span>
                </span>
              ))}
              {preview.columns.length > 8 && (
                <span className="text-[10px] text-gx-muted">
                  +{preview.columns.length - 8} more
                </span>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function SafetensorsMetadata({ repo, path }: { repo: OpenRepo; path: string }) {
  const [preview, setPreview] = useState<SafetensorsPreview | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    rpc
      .previewSafetensors(repo.repo_id, path, { signal: controller.signal })
      .then((p) => {
        if (controller.signal.aborted) return;
        setPreview(p);
        setLoading(false);
      })
      .catch((e) => {
        if (controller.signal.aborted) return;
        setError(e instanceof Error ? e.message : String(e));
        setLoading(false);
      });
    return () => controller.abort();
  }, [repo.repo_id, path]);

  if (loading) return <MetadataLoading />;
  if (error) return <MetadataError message={error} />;
  if (!preview) return null;

  return (
    <div className="max-h-40 overflow-auto p-3 text-xs">
      <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">Safetensors</div>
      <div className="space-y-1">
        <MetaRow label="Tensors" value={String(preview.tensor_count)} />
        <MetaRow label="Total params" value={formatParamCount(preview.total_params)} />
        <MetaRow label="Total size" value={formatBytes(Number(preview.total_bytes))} />
        {Object.keys(preview.dtype_histogram).length > 0 && (
          <div className="mt-1.5">
            <div className="text-[10px] text-gx-muted">Dtypes:</div>
            <div className="mt-0.5 flex flex-wrap gap-1">
              {Object.entries(preview.dtype_histogram).map(([dtype, count]) => (
                <span key={dtype} className="chip font-mono text-[10px]">
                  {dtype} ×{count}
                </span>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function CheckpointMetadata({ entry }: { entry: ClassifiedEntry }) {
  const isPointer = entry.kind === "pointer" || entry.kind === "lfs";
  return (
    <div className="max-h-40 overflow-auto p-3 text-xs">
      <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">Checkpoint</div>
      <div className="space-y-1">
        <MetaRow label="File" value={fileName(entry.path)} />
        <MetaRow label="Size" value={formatBytes(entry.size)} />
        <MetaRow label="Pointer" value={isPointer ? `Yes (${entry.kind})` : "No"} />
      </div>
    </div>
  );
}

function NotebookMetadata({ entry }: { entry: ClassifiedEntry }) {
  return (
    <div className="max-h-40 overflow-auto p-3 text-xs">
      <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">Notebook</div>
      <div className="space-y-1">
        <MetaRow label="File" value={fileName(entry.path)} />
        <MetaRow label="Size" value={formatBytes(entry.size)} />
        <MetaRow label="Cells" value="—" />
      </div>
      <div className="mt-1 text-[10px] text-gx-muted italic">
        Cell count available after preview verb is implemented.
      </div>
    </div>
  );
}

function BasicMetadata({ entry }: { entry: ClassifiedEntry }) {
  return (
    <div className="max-h-40 overflow-auto p-3 text-xs">
      <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">File Info</div>
      <div className="space-y-1">
        <MetaRow label="Path" value={entry.path} />
        <MetaRow label="Size" value={formatBytes(entry.size)} />
        <MetaRow label="Kind" value={KIND_LABELS[entry.datasetKind]} />
      </div>
    </div>
  );
}

// ---- Shared metadata helpers ----

function MetaRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-2">
      <span className="shrink-0 text-gx-muted">{label}</span>
      <span className="min-w-0 truncate text-right font-mono text-gx-text">{value}</span>
    </div>
  );
}

function MetadataLoading() {
  return (
    <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
      <Loader2 size={12} className="animate-spin" /> Loading metadata…
    </div>
  );
}

function MetadataError({ message }: { message: string }) {
  return (
    <div className="p-3 text-xs text-gx-error">{message}</div>
  );
}

function formatParamCount(totalParams: string): string {
  const n = Number(totalParams);
  if (!Number.isFinite(n) || n === 0) return totalParams;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return String(n);
}
