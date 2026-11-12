import { useCallback, useEffect, useRef, useState, useMemo, useSyncExternalStore } from "react";
import clsx from "clsx";
import { Database, Key, Table2 } from "lucide-react";
import { rpc } from "../../ipc";
import { formatBytes } from "../../lib/format";
import { FileBrowseState, FileQueryMode, FileQueryPanel } from "./FileQueryPanel";
import { fileQuery } from "../../lib/file-query";

interface Props { repoId: string; path: string; rev: string | null; nativeSessionId: string; }

interface SqliteTable {
  name: string;
  columns: Array<{ name: string; type: string; notnull: boolean; pk: boolean }>;
  row_count: number;
  preview_rows: Array<Record<string, unknown>>;
}

interface SqliteData {
  kind: "sqlite";
  file_size: number;
  table_count: number;
  tables: SqliteTable[];
}


export function SqliteView({ repoId, path, rev, nativeSessionId }: Props) {
  const [data, setData] = useState<SqliteData | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [activeTable, setActiveTable] = useState<string | null>(null);
  const query = useMemo(() => fileQuery(repoId, path, rev), [repoId, path, rev]);
  const { mode, sql } = useSyncExternalStore(query.subscribe, query.snapshot, query.snapshot);
  const needsMetadata = mode === "browse" || sql === null;

  useEffect(() => {
    if (!needsMetadata) return;
    const controller = new AbortController();
    setData(null);
    setErr(null);
    rpc
      .previewSqlite(repoId, path, { rev: rev ?? undefined, native_session_id: nativeSessionId, signal: controller.signal })
      .then((d) => {
        if (controller.signal.aborted) return;
        setData(d);
        if (d.tables.length > 0) setActiveTable(d.tables[0].name);
      })
      .catch((e: Error) => { if (!controller.signal.aborted) setErr(e.message); });
    return () => controller.abort();
  }, [repoId, path, rev, nativeSessionId, needsMetadata]);

  const table = data?.tables.find(t => t.name === activeTable);

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center gap-4 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <Database size={13} className="text-gx-accent" />
        <FileQueryMode query={query} mode={mode} />
        {data && (
          <>
            <span><strong className="text-gx-text">{data.table_count}</strong> tables</span>
            <span>{formatBytes(data.file_size)}</span>
          </>
        )}

        <span className="ml-auto font-mono">{path.split("/").pop()}</span>
      </div>

      {/* Content */}
      {mode === "browse" ? (
        data ? <div className="grid min-h-0 flex-1 grid-cols-[240px_1fr]">
          {/* Table list */}
          <aside className="min-h-0 overflow-auto border-r border-gx-border bg-gx-surface p-2">
            <div className="mb-2 text-[10px] uppercase tracking-wider text-gx-muted">Tables</div>
            <ul className="space-y-1">
              {data.tables.map(t => (
                <li key={t.name}>
                  <button
                    type="button"
                    onClick={() => setActiveTable(t.name)}
                    className={clsx(
                      "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[12px] transition",
                      activeTable === t.name ? "bg-gx-accent/15 text-gx-text" : "text-gx-text hover:bg-gx-surface-hover",
                    )}
                  >
                    <Table2 size={11} className="shrink-0 text-gx-muted" />
                    <span className="min-w-0 flex-1 truncate font-mono">{t.name}</span>
                    <span className="shrink-0 text-[10px] text-gx-muted">{t.row_count.toLocaleString()}</span>
                  </button>
                </li>
              ))}
            </ul>
          </aside>
          {/* Data grid */}
          <div className="min-h-0 overflow-auto bg-gx-bg-sub">
            {table ? <TableGrid table={table} /> : (
              <div className="flex h-full items-center justify-center text-sm text-gx-muted">Select a table</div>
            )}
          </div>
        </div> : <FileBrowseState error={err} loading="Reading SQLite database…" />
      ) : (
        <div className="flex min-h-0 flex-1 flex-col">
          {err && <div role="alert" className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>}
          {!data && sql === null && !err && <p role="status" className="shrink-0 px-3 py-1.5 text-xs text-gx-muted">Reading table metadata…</p>}
          <div className="min-h-0 flex-1">
            <FileQueryPanel
              query={query}
              nativeSessionId={nativeSessionId}
              tables={data?.tables}
            />
          </div>
        </div>
      )}
    </div>
  );
}

function TableGrid({ table }: { table: SqliteTable }) {
  const cols = table.columns.map(c => c.name);
  const [colWidths, setColWidths] = useState<Record<string, number>>({});
  const resizingRef = useRef<{ col: string; startX: number; startW: number } | null>(null);

  const handleResizeStart = useCallback(
    (e: React.MouseEvent, col: string) => {
      e.preventDefault();
      e.stopPropagation();
      const startW = colWidths[col] || 150;
      resizingRef.current = { col, startX: e.clientX, startW };

      const handleMove = (ev: MouseEvent) => {
        if (!resizingRef.current) return;
        const diff = ev.clientX - resizingRef.current.startX;
        const newW = Math.max(60, resizingRef.current.startW + diff);
        setColWidths((prev) => ({ ...prev, [resizingRef.current!.col]: newW }));
      };
      const handleUp = () => {
        resizingRef.current = null;
        document.removeEventListener("mousemove", handleMove);
        document.removeEventListener("mouseup", handleUp);
      };
      document.addEventListener("mousemove", handleMove);
      document.addEventListener("mouseup", handleUp);
    },
    [colWidths],
  );

  return (
    <div className="flex h-full flex-col">
      {/* Schema strip */}
      <div className="shrink-0 border-b border-gx-border bg-gx-surface px-3 py-1.5">
        <div className="flex flex-wrap gap-2 text-[11px]">
          {table.columns.map(c => (
            <span key={c.name} className="flex items-center gap-1 rounded border border-gx-border bg-gx-bg-sub px-1.5 py-0.5 font-mono">
              {c.pk && <Key size={9} className="text-gx-warn" />}
              <span className="text-gx-text">{c.name}</span>
              <span className="text-gx-muted">{c.type}</span>
            </span>
          ))}
        </div>
      </div>
      {/* Rows */}
      <div className="min-h-0 flex-1 overflow-auto">
        {table.preview_rows.length === 0 ? (
          <div className="p-4 text-center text-xs text-gx-muted">Table is empty.</div>
        ) : (
          <table className="border-collapse font-mono text-[12px]" style={{ minWidth: "100%" }}>
            <thead className="sticky top-0 bg-gx-surface">
              <tr>
                <th className="sticky left-0 z-10 w-[42px] min-w-[42px] border-b border-r border-gx-border bg-gx-surface px-2 py-1 text-right text-gx-muted whitespace-nowrap">#</th>
                {cols.map(c => {
                  const w = colWidths[c];
                  return (
                    <th
                      key={c}
                      className="relative border-b border-gx-border px-2 py-1 text-left font-semibold text-gx-text whitespace-nowrap"
                      style={w ? { width: w, minWidth: w, maxWidth: w } : { minWidth: 80 }}
                    >
                      <span className="block truncate">{c}</span>
                      {/* Resize handle */}
                      <div
                        className="absolute right-0 top-0 h-full w-[5px] cursor-col-resize hover:bg-gx-accent/30"
                        onMouseDown={(e) => handleResizeStart(e, c)}
                      />
                    </th>
                  );
                })}
              </tr>
            </thead>
            <tbody>
              {table.preview_rows.map((row, ri) => (
                <tr key={ri} className="odd:bg-gx-surface/50 hover:bg-gx-accent/5">
                  <td className="sticky left-0 z-10 w-[42px] min-w-[42px] border-r border-gx-border/40 bg-inherit px-2 py-1 text-right text-gx-muted whitespace-nowrap">{ri + 1}</td>
                  {cols.map(c => {
                    const w = colWidths[c];
                    return (
                      <td
                        key={c}
                        className="border-b border-gx-border/40 px-2 py-1 text-gx-text overflow-hidden text-ellipsis whitespace-nowrap"
                        style={w ? { width: w, minWidth: w, maxWidth: w } : { maxWidth: 300 }}
                        title={renderCell(row[c])}
                      >
                        {renderCell(row[c])}
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

function renderCell(v: unknown): string {
  if (v === null || v === undefined) return "∅";
  if (typeof v === "string") return v.length > 200 ? v.slice(0, 200) + "…" : v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return String(v);
}
