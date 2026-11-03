import { useEffect, useState } from "react";
import clsx from "clsx";
import { Loader2, Table2, Terminal } from "lucide-react";
import { rpc, ParquetPreview } from "../../ipc";
import { formatBytes } from "../../lib/format";
import { useSetting } from "../../settings";
import { DuckDbQueryPanel } from "./DuckDbQueryPanel";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

type ViewMode = "browse" | "sql";

// Parquet preview: the agent parses the file footer + first row
// group and returns summary + rows. The UI stays dumb on purpose.
export function ParquetView({ repoId, path, rev }: Props) {
  const [data, setData] = useState<ParquetPreview | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [mode, setMode] = useState<ViewMode>("browse");
  const maxRows = useSetting("parquetPreviewRows");

  useEffect(() => {
    let cancelled = false;
    setData(null);
    setErr(null);
    rpc
      .previewParquet(repoId, path, { rev: rev ?? undefined, max_rows: maxRows })
      .then((d) => {
        if (!cancelled) setData(d);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [repoId, path, rev, maxRows]);

  if (err) {
    return (
      <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
        {err}
      </div>
    );
  }
  if (!data) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
        <Loader2 size={14} className="animate-spin" /> Reading Parquet metadata…
      </div>
    );
  }

  const rowsAny = data.preview_rows ?? [];

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center gap-4 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <Table2 size={13} className="text-gx-accent" />
        <span>
          <strong className="text-gx-text">{data.num_rows.toLocaleString()}</strong> rows
        </span>
        <span>
          <strong className="text-gx-text">{data.num_columns}</strong> columns
        </span>
        <span>
          <strong className="text-gx-text">{data.num_row_groups}</strong> row groups
        </span>
        {data.created_by && <span className="truncate">· {data.created_by}</span>}

        {/* Mode toggle */}
        <div className="ml-4 flex items-center rounded border border-gx-border bg-gx-bg-sub">
          <button
            type="button"
            onClick={() => setMode("browse")}
            className={clsx(
              "flex items-center gap-1 px-2 py-0.5 text-[10px] font-medium transition rounded-l",
              mode === "browse"
                ? "bg-gx-accent/15 text-gx-accent"
                : "text-gx-muted hover:text-gx-text",
            )}
          >
            <Table2 size={10} />
            Browse
          </button>
          <button
            type="button"
            onClick={() => setMode("sql")}
            className={clsx(
              "flex items-center gap-1 px-2 py-0.5 text-[10px] font-medium transition rounded-r",
              mode === "sql"
                ? "bg-gx-accent/15 text-gx-accent"
                : "text-gx-muted hover:text-gx-text",
            )}
          >
            <Terminal size={10} />
            SQL
          </button>
        </div>

        <span className="ml-auto">preview · first {data.preview_row_count} rows</span>
      </div>

      {mode === "browse" ? (
        <div className="grid min-h-0 flex-1 grid-cols-[300px_1fr]">
          <SchemaPanel data={data} />
          <RowsGrid data={data} rows={rowsAny} />
        </div>
      ) : (
        <div className="min-h-0 flex-1">
          <DuckDbQueryPanel repoId={repoId} filePath={path} />
        </div>
      )}
    </div>
  );
}

function SchemaPanel({ data }: { data: ParquetPreview }) {
  return (
    <aside className="min-h-0 overflow-auto border-r border-gx-border bg-gx-surface p-3 text-[12px]">
      <div className="mb-2 text-[10px] uppercase tracking-wider text-gx-muted">Columns</div>
      <ul className="space-y-1">
        {data.columns.map((col, i) => {
          const stats = data.row_group_stats[i];
          return (
            <li key={col.name} className="rounded border border-gx-border bg-gx-bg-sub p-2">
              <div className="flex items-center justify-between gap-2">
                <span className="truncate font-mono text-[12.5px] text-gx-text">{col.name}</span>
                {col.nullable && (
                  <span className="chip text-[10px] text-gx-muted">nullable</span>
                )}
              </div>
              <div className="mt-0.5 truncate font-mono text-[11px] text-gx-accent">
                {col.type}
              </div>
              {stats && (
                <div className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-[11px] text-gx-muted">
                  <span>{stats.compression}</span>
                  <span>
                    {formatBytes(stats.compressed_size)} →{" "}
                    {formatBytes(stats.uncompressed_size)}
                  </span>
                  {stats.null_count != null && <span>{stats.null_count} nulls</span>}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </aside>
  );
}

function RowsGrid({
  data,
  rows,
}: {
  data: ParquetPreview;
  rows: Array<Record<string, unknown>>;
}) {
  if (rows.length === 0) {
    return (
      <div className="flex items-center justify-center text-sm text-gx-muted">No rows preview available.</div>
    );
  }
  const cols = data.columns.map((c) => c.name);
  return (
    <div className="min-h-0 overflow-auto bg-gx-bg-sub">
      <table className="w-full border-collapse font-mono text-[12px]">
        <thead className="sticky top-0 bg-gx-surface">
          <tr>
            <th className="sticky left-0 border-b border-gx-border bg-gx-surface px-2 py-1 text-right text-gx-muted">#</th>
            {cols.map((c) => (
              <th
                key={c}
                className="border-b border-gx-border px-2 py-1 text-left font-semibold text-gx-text"
                title={c}
              >
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, ri) => (
            <tr key={ri} className="odd:bg-gx-surface">
              <td className="sticky left-0 bg-inherit px-2 py-1 text-right text-gx-muted">{ri + 1}</td>
              {cols.map((c) => (
                <td key={c} className="border-b border-gx-border/40 px-2 py-1 align-top text-gx-text">
                  {renderCell(row[c])}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function renderCell(v: unknown): string {
  if (v === null || v === undefined) return "∅";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}
