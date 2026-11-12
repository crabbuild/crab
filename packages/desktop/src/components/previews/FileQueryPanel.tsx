import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { Toggle } from "@/components/ui/toggle";
import clsx from "clsx";
import {
  AlertCircle,
  ChevronLeft,
  ChevronRight,
  Columns3,
  Copy,
  Database,
  Download,
  Hash,
  History,
  Key,
  Loader2,
  Play,
  Search,
  SortAsc,
  SortDesc,
  Table2,
  Terminal,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import type { FileQueryResult } from "../../ipc";
import type { FileQuery } from "../../lib/file-query";

export function FileQueryMode({ query, mode }: { query: FileQuery; mode: "browse" | "sql" }) {
  return (
    <div className="flex items-center rounded border border-border bg-background" role="group" aria-label="File view">
      {(["browse", "sql"] as const).map((value) => {
        const Icon = value === "browse" ? Table2 : Terminal;
        return (
          <Toggle
            size="sm"
            key={value}
            type="button"
            pressed={mode === value}
            onClick={() => query.setMode(value)}
            className={clsx("flex items-center gap-1 px-2 py-0.5 text-[10px] font-medium transition",
              mode === value ? "bg-primary/15 text-primary" : "text-muted-foreground hover:text-foreground")}
          >
            <Icon size={10} /> {value === "browse" ? "Browse" : "SQL"}
          </Toggle>
        );
      })}
    </div>
  );
}

export function FileBrowseState({ error, loading }: { error: string | null; loading: string }) {
  if (error) {
    return (
      <div role="alert" className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
        {error}
      </div>
    );
  }
  return (
    <div role="status" className="flex flex-1 items-center justify-center gap-2 text-sm text-muted-foreground">
      <Loader2 size={14} className="animate-spin" /> {loading}
    </div>
  );
}

/** Table metadata passed from the parent (SqliteView already has this). */
export interface TableMeta {
  name: string;
  columns: Array<{ name: string; type: string; notnull: boolean; pk: boolean }>;
  row_count: number;
}

interface Props {
  query: FileQuery;
  nativeSessionId: string;
  tables?: TableMeta[];
}

interface SortState {
  column: string;
  dir: "ASC" | "DESC";
}

interface PageState {
  offset: number;
  limit: number;
}

export function FileQueryPanel({ query, nativeSessionId, tables }: Props) {
  const { sql: draft, result, elapsed, history, error: err, status } = useSyncExternalStore(query.subscribe, query.snapshot, query.snapshot);
  const filePath = query.path;
  const initialSql = defaultQuery(filePath, tables);
  const sql = draft ?? initialSql;
  const loading = status === "running" || status === "cancelling";
  const setSql = useCallback((text: string) => query.setSql(text), [query]);
  const [expandedTable, setExpandedTable] = useState<string | null>(null);
  const [activeTable, setActiveTable] = useState<string | null>(
    tables && tables.length > 0 ? tables[0].name : null,
  );
  const [sort, setSort] = useState<SortState | null>(null);
  const [page, setPage] = useState<PageState>({ offset: 0, limit: 100 });
  const [filterText, setFilterText] = useState("");
  const [copiedCell, setCopiedCell] = useState<string | null>(null);
  const [isBrowsingTable, setIsBrowsingTable] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const execute = useCallback((overrideSql?: string) => {
    void query.run(nativeSessionId, overrideSql ?? sql);
  }, [query, nativeSessionId, sql]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
        e.preventDefault();
        setIsBrowsingTable(false);
        execute();
      }
    },
    [execute],
  );

  // Build and run a SELECT query for a table with sort + pagination
  const queryTable = useCallback(
    (tableName: string, sortOvr?: SortState | null, pageOvr?: PageState, filter?: string) => {
      const s = sortOvr !== undefined ? sortOvr : sort;
      const p = pageOvr ?? page;
      const f = filter !== undefined ? filter : filterText;
      let q = `SELECT * FROM ${identifier(tableName)}`;
      if (f.trim()) {
        q += ` WHERE ${f}`;
      }
      if (s) {
        q += ` ORDER BY ${identifier(s.column)} ${s.dir}`;
      }
      q += ` LIMIT ${p.limit} OFFSET ${p.offset};`;
      setSql(q);
      setActiveTable(tableName);
      setIsBrowsingTable(true);
      execute(q);
    },
    [sort, page, filterText, execute],
  );

  // Table click from sidebar
  const handleTableSelect = useCallback(
    (tableName: string) => {
      setSort(null);
      setPage({ offset: 0, limit: 100 });
      setFilterText("");
      setActiveTable(tableName);
      setIsBrowsingTable(true);
      const q = `SELECT * FROM ${identifier(tableName)} LIMIT 100;`;
      setSql(q);
      execute(q);
    },
    [execute],
  );

  // Quick actions
  const handleCountTable = useCallback(
    (tableName: string) => {
      setIsBrowsingTable(false);
      const q = `SELECT COUNT(*) AS total FROM ${identifier(tableName)};`;
      setSql(q);
      execute(q);
    },
    [execute],
  );

  const handleSchemaTable = useCallback(
    (tableName: string) => {
      setIsBrowsingTable(false);
      const q = `PRAGMA table_info('${tableName.replaceAll("'", "''")}');`;
      setSql(q);
      execute(q);
    },
    [execute],
  );

  const handleIndexesTable = useCallback(
    (tableName: string) => {
      setIsBrowsingTable(false);
      const q = `PRAGMA index_list('${tableName.replaceAll("'", "''")}');`;
      setSql(q);
      execute(q);
    },
    [execute],
  );

  // Sort by clicking column header — only works when browsing a table
  const handleSort = useCallback(
    (column: string) => {
      if (!activeTable || !isBrowsingTable) return;
      // Only allow sorting when the current query is a table browse
      // (not an arbitrary user-typed query)
      const newSort: SortState =
        sort?.column === column && sort.dir === "ASC"
          ? { column, dir: "DESC" }
          : { column, dir: "ASC" };
      setSort(newSort);
      queryTable(activeTable, newSort, page);
    },
    [activeTable, isBrowsingTable, sort, page, queryTable],
  );

  // Pagination
  const handleNextPage = useCallback(() => {
    if (!activeTable) return;
    const newPage = { ...page, offset: page.offset + page.limit };
    setPage(newPage);
    queryTable(activeTable, sort, newPage);
  }, [activeTable, page, sort, queryTable]);

  const handlePrevPage = useCallback(() => {
    if (!activeTable) return;
    const newPage = { ...page, offset: Math.max(0, page.offset - page.limit) };
    setPage(newPage);
    queryTable(activeTable, sort, newPage);
  }, [activeTable, page, sort, queryTable]);

  // Insert column name into SQL editor
  const insertColumnRef = useCallback((colName: string) => {
    const ta = textareaRef.current;
    if (!ta) return;
    const start = ta.selectionStart;
    const end = ta.selectionEnd;
    const before = ta.value.slice(0, start);
    const after = ta.value.slice(end);
    const insert = `${identifier(colName)}`;
    setSql(before + insert + after);
    setTimeout(() => {
      ta.focus();
      ta.selectionStart = ta.selectionEnd = start + insert.length;
    }, 0);
  }, []);

  // Copy cell value
  const handleCellCopy = useCallback((value: string) => {
    navigator.clipboard.writeText(value).then(() => {
      setCopiedCell(value);
      setTimeout(() => setCopiedCell(null), 1500);
    });
  }, []);

  // Export as CSV
  const handleExportCsv = useCallback(() => {
    if (!result) return;
    const cols = result.columns.map((c) => c.name);
    const header = cols.map((c) => `"${c}"`).join(",");
    const rows = result.rows.map((row) =>
      cols.map((c) => {
        const v = row[c];
        if (v === null || v === undefined) return "";
        const s = String(v);
        return s.includes(",") || s.includes('"') || s.includes("\n")
          ? `"${s.replace(/"/g, '""')}"`
          : s;
      }).join(","),
    );
    const csv = [header, ...rows].join("\n");
    const blob = new Blob([csv], { type: "text/csv" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "query_result.csv";
    a.click();
    URL.revokeObjectURL(url);
  }, [result]);

  useEffect(() => {
    query.startDefault(nativeSessionId, initialSql);
  }, [query, nativeSessionId, initialSql]);

  const hasSidebar = tables && tables.length > 0;
  const currentTableMeta = tables?.find((t) => t.name === activeTable);

  return (
    <div className="flex h-full">
      {/* ─── Table Sidebar ─────────────────────────────────────── */}
      {hasSidebar && (
        <aside className="flex w-[240px] shrink-0 flex-col border-r border-border bg-card">
          {/* Sidebar header */}
          <div className="flex items-center gap-2 border-b border-border px-3 py-2">
            <Database size={12} className="text-primary" />
            <span className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
              Tables ({tables.length})
            </span>
          </div>

          {/* Table list */}
          <div className="min-h-0 flex-1 overflow-auto p-1">
            {tables.map((t) => (
              <div key={t.name} className="mb-0.5">
                <div className="group flex items-center gap-0.5">
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    onClick={() =>
                      setExpandedTable(expandedTable === t.name ? null : t.name)
                    }
                    className="shrink-0 rounded p-0.5 text-muted-foreground hover:text-foreground"
                  >
                    <ChevronRight
                      size={10}
                      className={clsx(
                        "transition-transform",
                        expandedTable === t.name && "rotate-90",
                      )}
                    />
                  </Button>
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    onClick={() => handleTableSelect(t.name)}
                    className={clsx(
                      "flex min-w-0 flex-1 items-center gap-1.5 rounded px-1.5 py-1 text-left text-[11px] transition",
                      activeTable === t.name
                        ? "bg-primary/15 text-foreground"
                        : "hover:bg-accent text-foreground",
                    )}
                  >
                    <Table2 size={10} className="shrink-0 text-muted-foreground" />
                    <span className="min-w-0 flex-1 truncate font-mono">
                      {t.name}
                    </span>
                    <span className="shrink-0 text-[9px] text-muted-foreground">
                      {t.row_count.toLocaleString()}
                    </span>
                  </Button>
                  {/* Quick action buttons (visible on hover) */}
                  <div className="flex shrink-0 items-center gap-0.5 opacity-0 transition group-hover:opacity-100">
                    <Button
                      variant="ghost"
                      size="compact"
                      type="button"
                      onClick={() => handleCountTable(t.name)}
                      title="COUNT(*)"
                      className="rounded p-0.5 text-muted-foreground hover:bg-background hover:text-foreground"
                    >
                      <Hash size={9} />
                    </Button>
                    <Button
                      variant="ghost"
                      size="compact"
                      type="button"
                      onClick={() => handleSchemaTable(t.name)}
                      title="Table schema"
                      className="rounded p-0.5 text-muted-foreground hover:bg-background hover:text-foreground"
                    >
                      <Columns3 size={9} />
                    </Button>
                  </div>
                </div>

                {/* Expanded columns */}
                {expandedTable === t.name && (
                  <div className="ml-5 mt-0.5 mb-1 space-y-px border-l border-border/60 pl-2">
                    {t.columns.map((col) => (
                      <Button
                        variant="ghost"
                        size="compact"
                        key={col.name}
                        type="button"
                        onClick={() => insertColumnRef(col.name)}
                        title={`Insert "${col.name}" into query`}
                        className="flex w-full items-center gap-1.5 rounded px-1 py-0.5 text-[10px] transition hover:bg-accent"
                      >
                        {col.pk ? (
                          <Key size={8} className="shrink-0 text-warning" />
                        ) : (
                          <span className="w-2 shrink-0 text-center text-muted-foreground">
                            ·
                          </span>
                        )}
                        <span className="min-w-0 flex-1 truncate text-left font-mono text-foreground">
                          {col.name}
                        </span>
                        <span className="shrink-0 font-mono text-muted-foreground">
                          {col.type || "ANY"}
                        </span>
                        {col.notnull && (
                          <span className="shrink-0 rounded bg-warning/10 px-1 text-[8px] text-warning">
                            NN
                          </span>
                        )}
                      </Button>
                    ))}
                    {/* Quick schema actions */}
                    <div className="mt-1 flex gap-1 pt-1 border-t border-border/40">
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        onClick={() => handleIndexesTable(t.name)}
                        className="rounded bg-background px-1.5 py-0.5 text-[9px] text-muted-foreground hover:text-foreground"
                      >
                        Indexes
                      </Button>
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        onClick={() => {
                          setIsBrowsingTable(false);
                          const q = `SELECT sql FROM sqlite_master WHERE name='${t.name.replaceAll("'", "''")}';`;
                          setSql(q);
                          execute(q);
                        }}
                        className="rounded bg-background px-1.5 py-0.5 text-[9px] text-muted-foreground hover:text-foreground"
                      >
                        DDL
                      </Button>
                    </div>
                  </div>
                )}
              </div>
            ))}
          </div>

          {/* Query history */}
          {history.length > 0 && (
            <div className="border-t border-border">
              <div className="flex items-center gap-2 px-3 py-1.5 text-[10px] uppercase tracking-wider text-muted-foreground">
                <History size={10} />
                Recent ({history.length})
              </div>
              <div className="max-h-[100px] overflow-auto px-1 pb-1">
                {history.slice(0, 10).map((q, i) => (
                  <Button
                    variant="ghost"
                    size="compact"
                    key={i}
                    type="button"
                    onClick={() => {
                      setSql(q);
                      execute(q);
                    }}
                    className="justify-start mb-0.5 w-full truncate rounded px-2 py-0.5 text-left font-mono text-[9px] text-muted-foreground transition hover:bg-accent hover:text-foreground"
                    title={q}
                  >
                    {q}
                  </Button>
                ))}
              </div>
            </div>
          )}
        </aside>
      )}

      {/* ─── Main Area ─────────────────────────────────────────── */}
      <div className="flex min-w-0 flex-1 flex-col">
        {/* SQL Editor */}
        <div className="shrink-0 border-b border-border bg-card">
          <div className="flex items-center gap-2 px-3 py-1.5 text-[11px] text-muted-foreground">
            <Database size={12} className="text-primary" />
            <span className="font-semibold text-foreground">SQL</span>
            {activeTable && hasSidebar && (
              <span className="rounded bg-primary/10 px-1.5 py-0.5 font-mono text-[10px] text-primary">
                {activeTable}
              </span>
            )}
            {filePath && !hasSidebar && (
              <span className="rounded bg-background px-1.5 py-0.5 font-mono text-[10px]">
                {filePath.split("/").pop()}
              </span>
            )}
            <div className="ml-auto flex items-center gap-2 text-[10px]">
              {result && (
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={handleExportCsv}
                  className="flex items-center gap-1 rounded px-1.5 py-0.5 text-muted-foreground hover:bg-background hover:text-foreground"
                  title="Export results as CSV"
                >
                  <Download size={10} />
                  CSV
                </Button>
              )}
              <span className="text-muted-foreground">⌘+Enter</span>
            </div>
          </div>
          <div className="relative px-3 pb-2">
            <Textarea
              ref={textareaRef}
              value={sql}
              onChange={(e) => setSql(e.target.value)}
              onKeyDown={handleKeyDown}
              aria-label="SQL query"
              spellCheck={false}
              className={clsx(
                "w-full resize-y rounded border border-border bg-background px-3 py-2",
                "font-mono text-[12px] text-foreground placeholder:text-muted-foreground",
                "focus:border-primary focus-visible:outline-none",
                "min-h-[52px] max-h-[180px]",
              )}
              placeholder={
                hasSidebar
                  ? "Click a table or write SQL. Click columns to insert references."
                  : "SELECT * FROM data LIMIT 100"
              }
              rows={2}
            />
            {loading ? (
              <Button variant="outline" type="button" className="absolute bottom-4 right-5" onClick={() => query.cancel()} disabled={status === "cancelling"}>
                {status === "cancelling" ? "Cancelling…" : "Cancel query"}
              </Button>
            ) : (
              <Button
                variant="default"
                size="compact"
                type="button"
                onClick={() => { setIsBrowsingTable(false); execute(); }}
                disabled={!sql.trim()}
                className={clsx(
                "absolute bottom-4 right-5 flex items-center gap-1.5 rounded px-2.5 py-1",
                "text-[11px] font-medium transition",
                "bg-primary text-primary-foreground hover:bg-primary/90",
              )}
              >
                <Play size={10} /> Run
              </Button>
            )}
          </div>
        </div>

        {/* Filter bar (when viewing a table) */}
        {activeTable && hasSidebar && (
          <div className="flex shrink-0 items-center gap-2 border-b border-border bg-card px-3 py-1">
            <Search size={11} className="text-muted-foreground" />
            <Input
              type="text"
              value={filterText}
              onChange={(e) => setFilterText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && activeTable) {
                  setPage({ offset: 0, limit: 100 });
                  queryTable(activeTable, sort, { offset: 0, limit: 100 }, filterText);
                }
              }}
              placeholder="WHERE clause filter — e.g. age > 25 AND name LIKE '%a%'"
              className="min-w-0 flex-1 bg-transparent font-mono text-[11px] text-foreground placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-primary"
            />
            {filterText && (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => {
                  setFilterText("");
                  if (activeTable) {
                    setPage({ offset: 0, limit: 100 });
                    queryTable(activeTable, sort, { offset: 0, limit: 100 }, "");
                  }
                }}
                className="text-muted-foreground hover:text-foreground"
              >
                <span className="text-[10px]">Clear</span>
              </Button>
            )}
          </div>
        )}

        {/* Results area */}
        <div className="min-h-0 flex-1 overflow-hidden bg-background">
          {err && (
            <div role="alert" className="m-3 flex items-start gap-2 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
              <AlertCircle size={14} className="mt-0.5 shrink-0" />
              <pre className="whitespace-pre-wrap break-all">{err}</pre>
            </div>
          )}
          {result && (
            <ResultGrid
              result={result}
              elapsed={elapsed}
              sort={sort}
              onSort={handleSort}
              onCellCopy={handleCellCopy}
              copiedCell={copiedCell}
              sortable={isBrowsingTable}
            />
          )}
          {!result && !err && status === "idle" && (
            <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
              {hasSidebar
                ? "Click a table or write a query to get started"
                : "Write a SQL query and press ⌘+Enter"}
            </div>
          )}
          {status === "cancelled" && (
            <p role="status" className="p-3 text-xs text-muted-foreground">
              Query cancelled
            </p>
          )}
          {loading && (
            <div role="status" className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
              <Loader2 size={14} className="animate-spin" />
              {status === "cancelling" ? "Cancelling query…" : "Executing query…"}
            </div>
          )}
        </div>

        {/* Pagination footer */}
        {result && activeTable && hasSidebar && (
          <div className="flex shrink-0 items-center gap-3 border-t border-border bg-card px-3 py-1 text-[11px] text-muted-foreground">
            <span>
              Showing{" "}
              <strong className="text-foreground">{page.offset + 1}</strong>–
              <strong className="text-foreground">
                {Math.min(page.offset + result.row_count, page.offset + page.limit)}
              </strong>
              {currentTableMeta && (
                <span> of {currentTableMeta.row_count.toLocaleString()}</span>
              )}
            </span>
            <div className="ml-auto flex items-center gap-1">
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={handlePrevPage}
                disabled={page.offset === 0}
                className="rounded p-1 hover:bg-background disabled:opacity-30"
              >
                <ChevronLeft size={12} />
              </Button>
              <span className="px-1 text-[10px]">
                Page {Math.floor(page.offset / page.limit) + 1}
              </span>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={handleNextPage}
                disabled={result.row_count < page.limit}
                className="rounded p-1 hover:bg-background disabled:opacity-30"
              >
                <ChevronRight size={12} />
              </Button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// ─── Result Grid with resizable columns, per-column filter, pivot ────

interface ResultGridProps {
  result: FileQueryResult;
  elapsed: number | null;
  sort: SortState | null;
  onSort: (column: string) => void;
  onCellCopy: (value: string) => void;
  copiedCell: string | null;
  sortable: boolean;
}

type GridMode = "table" | "pivot";

interface PivotConfig {
  groupBy: string[];
  values: string[];
  agg: "COUNT" | "SUM" | "AVG" | "MIN" | "MAX";
}

function ResultGrid({ result, elapsed, sort, onSort, onCellCopy, copiedCell, sortable }: ResultGridProps) {
  const cols = result.columns.map((c) => c.name);
  const [mode, setMode] = useState<GridMode>("table");
  const [colWidths, setColWidths] = useState<Record<string, number>>({});
  const [colFilters, setColFilters] = useState<Record<string, string>>({});
  const [showFilters, setShowFilters] = useState(false);
  const [pivotCfg, setPivotCfg] = useState<PivotConfig>({
    groupBy: cols.length > 0 ? [cols[0]] : [],
    values: cols.length > 1 ? [cols[1]] : [],
    agg: "COUNT",
  });
  const resizingRef = useRef<{ col: string; startX: number; startW: number } | null>(null);

  // Column resize handlers
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

  // Client-side column filtering (applied to already-fetched rows)
  const filteredRows = useMemo(() => {
    if (!showFilters) return result.rows;
    const activeFilters = Object.entries(colFilters).filter(([, v]) => v.trim() !== "");
    if (activeFilters.length === 0) return result.rows;
    return result.rows.filter((row) =>
      activeFilters.every(([col, filter]) => {
        const cellVal = String(row[col] ?? "").toLowerCase();
        const f = filter.toLowerCase();
        // Support basic operators
        if (f.startsWith(">") && !f.startsWith(">=")) {
          const num = parseFloat(f.slice(1));
          return !isNaN(num) && parseFloat(cellVal) > num;
        }
        if (f.startsWith(">=")) {
          const num = parseFloat(f.slice(2));
          return !isNaN(num) && parseFloat(cellVal) >= num;
        }
        if (f.startsWith("<") && !f.startsWith("<=")) {
          const num = parseFloat(f.slice(1));
          return !isNaN(num) && parseFloat(cellVal) < num;
        }
        if (f.startsWith("<=")) {
          const num = parseFloat(f.slice(2));
          return !isNaN(num) && parseFloat(cellVal) <= num;
        }
        if (f.startsWith("!=") || f.startsWith("<>")) {
          const val = f.slice(2).trim();
          return cellVal !== val;
        }
        if (f.startsWith("=")) {
          const val = f.slice(1).trim();
          return cellVal === val;
        }
        if (f === "null" || f === "is null") {
          return row[col] === null || row[col] === undefined;
        }
        if (f === "!null" || f === "is not null" || f === "not null") {
          return row[col] !== null && row[col] !== undefined;
        }
        // Default: substring match
        return cellVal.includes(f);
      }),
    );
  }, [result.rows, colFilters, showFilters]);

  // Pivot computation
  const pivotData = useMemo(() => {
    if (mode !== "pivot" || pivotCfg.groupBy.length === 0) return null;
    const groups = new Map<string, Record<string, unknown>[]>();
    for (const row of result.rows) {
      const key = pivotCfg.groupBy.map((g) => String(row[g] ?? "NULL")).join(" | ");
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key)!.push(row);
    }
    const pivotRows: Array<Record<string, unknown>> = [];
    for (const [key, rows] of groups) {
      const pivotRow: Record<string, unknown> = { __group: key };
      pivotRow.__count = rows.length;
      for (const valCol of pivotCfg.values) {
        const nums = rows.map((r) => Number(r[valCol])).filter((n) => !isNaN(n));
        switch (pivotCfg.agg) {
          case "COUNT":
            pivotRow[`${valCol}_count`] = rows.length;
            break;
          case "SUM":
            pivotRow[`${valCol}_sum`] = nums.reduce((a, b) => a + b, 0);
            break;
          case "AVG":
            pivotRow[`${valCol}_avg`] =
              nums.length > 0 ? (nums.reduce((a, b) => a + b, 0) / nums.length).toFixed(2) : null;
            break;
          case "MIN":
            pivotRow[`${valCol}_min`] = nums.length > 0 ? Math.min(...nums) : null;
            break;
          case "MAX":
            pivotRow[`${valCol}_max`] = nums.length > 0 ? Math.max(...nums) : null;
            break;
        }
      }
      pivotRows.push(pivotRow);
    }
    // Sort by count descending
    pivotRows.sort((a, b) => (b.__count as number) - (a.__count as number));
    return pivotRows;
  }, [mode, pivotCfg, result.rows]);

  return (
    <div className="flex h-full flex-col">
      {/* Toolbar */}
      <div className="flex shrink-0 items-center gap-2 border-b border-border bg-card px-3 py-1 text-[11px] text-muted-foreground">
        <span>
          <strong className="text-foreground">
            {showFilters ? filteredRows.length : result.row_count}
          </strong>{" "}
          rows
          {result.truncated && (
            <span className="ml-1 text-warning">(result truncated)</span>
          )}
          {showFilters && filteredRows.length !== result.row_count && (
            <span className="ml-1 text-primary">
              (filtered from {result.row_count})
            </span>
          )}
        </span>
        <span>{result.columns.length} cols</span>
        {elapsed !== null && (
          <span>
            {elapsed < 1000 ? `${Math.round(elapsed)}ms` : `${(elapsed / 1000).toFixed(2)}s`}
          </span>
        )}

        <div className="ml-auto flex items-center gap-1">
          {copiedCell !== null && (
            <span className="flex items-center gap-1 text-primary">
              <Copy size={9} /> Copied
            </span>
          )}
          {/* Filter toggle */}
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setShowFilters(!showFilters)}
            className={clsx(
              "flex items-center gap-1 rounded px-1.5 py-0.5 transition",
              showFilters
                ? "bg-primary/15 text-primary"
                : "text-muted-foreground hover:text-foreground",
            )}
            title="Toggle column filters"
          >
            <Search size={10} />
            Filter
          </Button>
          {/* Filter toggle */}
          <div className="flex items-center rounded border border-border bg-background">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => setMode("table")}
              className={clsx(
                "rounded-l px-1.5 py-0.5 text-[10px] transition",
                mode === "table"
                  ? "bg-primary/15 text-primary"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              Table
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={() => setMode("pivot")}
              className={clsx(
                "rounded-r px-1.5 py-0.5 text-[10px] transition",
                mode === "pivot"
                  ? "bg-primary/15 text-primary"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              Pivot
            </Button>
          </div>
        </div>
      </div>

      {/* Pivot config bar */}
      {mode === "pivot" && (
        <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border bg-card px-3 py-1.5 text-[10px]">
          <label className="flex items-center gap-1 text-muted-foreground">
            Group by:
            <Select
              value={":" + String(pivotCfg.groupBy[0] || "")}
              onValueChange={(selectedValue) => {
                const value = selectedValue.slice(1);
                return setPivotCfg((p) => ({
                  ...p,
                  groupBy: value ? [value] : [],
                }));
              }}
            >
              <SelectTrigger className="rounded border border-border bg-background px-1 py-0.5 font-mono text-foreground">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value={":" + String("")}>—</SelectItem>
                  {cols.map((c) => (
                    <SelectItem key={c} value={":" + String(c)}>
                      {c}
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
          </label>
          <label className="flex items-center gap-1 text-muted-foreground">
            Value:
            <Select
              value={":" + String(pivotCfg.values[0] || "")}
              onValueChange={(selectedValue) => {
                const value = selectedValue.slice(1);
                return setPivotCfg((p) => ({
                  ...p,
                  values: value ? [value] : [],
                }));
              }}
            >
              <SelectTrigger className="rounded border border-border bg-background px-1 py-0.5 font-mono text-foreground">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value={":" + String("")}>—</SelectItem>
                  {cols.map((c) => (
                    <SelectItem key={c} value={":" + String(c)}>
                      {c}
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
          </label>
          <label className="flex items-center gap-1 text-muted-foreground">
            Agg:
            <Select
              value={":" + String(pivotCfg.agg)}
              onValueChange={(selectedValue) => {
                const value = selectedValue.slice(1);
                return setPivotCfg((p) => ({
                  ...p,
                  agg: value as PivotConfig["agg"],
                }));
              }}
            >
              <SelectTrigger className="rounded border border-border bg-background px-1 py-0.5 font-mono text-foreground">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  {(["COUNT", "SUM", "AVG", "MIN", "MAX"] as const).map((a) => (
                    <SelectItem key={a} value={":" + String(a)}>
                      {a}
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
          </label>
          <span className="text-muted-foreground">
            {pivotData ? `${pivotData.length} groups` : ""}
          </span>
        </div>
      )}

      {/* Data table / Pivot view */}
      <div className="min-h-0 flex-1 overflow-auto">
        {mode === "table" ? (
          result.row_count === 0 ? (
            <div className="p-4 text-center text-xs text-muted-foreground">
              No rows returned.
            </div>
          ) : (
            <table className="border-collapse font-mono text-[11px]" style={{ minWidth: "100%" }}>
              <thead className="sticky top-0 z-10 bg-card">
                <tr>
                  <th className="sticky left-0 z-20 w-[42px] min-w-[42px] border-b border-r border-border bg-card px-2 py-1.5 text-right text-[10px] text-muted-foreground whitespace-nowrap">
                    #
                  </th>
                  {cols.map((c) => {
                    const isSorted = sort?.column === c;
                    const w = colWidths[c];
                    return (
                      <th
                        key={c}
                        className={clsx(
                          "relative select-none border-b border-border px-2 py-1.5 text-left font-semibold text-foreground whitespace-nowrap",
                          sortable && "cursor-pointer hover:bg-accent",
                        )}
                        style={w ? { width: w, minWidth: w, maxWidth: w } : { minWidth: 80 }}
                        onClick={() => sortable && onSort(c)}
                        title={sortable ? `Sort by ${c}` : c}
                      >
                        <span className="flex items-center gap-1 overflow-hidden">
                          <span className="truncate">{c}</span>
                          {isSorted && sortable &&
                            (sort.dir === "ASC" ? (
                              <SortAsc size={10} className="shrink-0 text-primary" />
                            ) : (
                              <SortDesc size={10} className="shrink-0 text-primary" />
                            ))}
                        </span>
                        {/* Resize handle */}
                        <div
                          className="absolute right-0 top-0 h-full w-[5px] cursor-col-resize hover:bg-primary/30"
                          onMouseDown={(e) => handleResizeStart(e, c)}
                          onClick={(e) => e.stopPropagation()}
                        />
                      </th>
                    );
                  })}
                </tr>
                {/* Column type row */}
                <tr>
                  <th className="sticky left-0 z-20 w-[42px] min-w-[42px] border-b border-r border-border bg-card px-2 py-0.5" />
                  {result.columns.map((c) => {
                    const w = colWidths[c.name];
                    return (
                      <th
                        key={c.name}
                        className="border-b border-border bg-card px-2 py-0.5 text-left text-[9px] font-normal text-muted-foreground whitespace-nowrap overflow-hidden"
                        style={w ? { width: w, minWidth: w, maxWidth: w } : undefined}
                      >
                        {c.type}
                      </th>
                    );
                  })}
                </tr>
                {/* Per-column filter row */}
                {showFilters && (
                  <tr>
                    <th className="sticky left-0 z-20 w-[42px] min-w-[42px] border-b border-r border-border bg-card px-1 py-0.5">
                      <Search size={9} className="mx-auto text-muted-foreground" />
                    </th>
                    {cols.map((c) => {
                      const w = colWidths[c];
                      return (
                        <th
                          key={c}
                          className="border-b border-border bg-card px-1 py-0.5"
                          style={w ? { width: w, minWidth: w, maxWidth: w } : undefined}
                        >
                          <Input
                            type="text"
                            value={colFilters[c] || ""}
                            onChange={(e) =>
                              setColFilters((prev) => ({ ...prev, [c]: e.target.value }))
                            }
                            placeholder="filter…"
                            className="w-full rounded border border-border/50 bg-background px-1 py-0.5 font-mono text-[9px] text-foreground placeholder:text-muted-foreground/50 focus:border-primary focus-visible:outline-none"
                          />
                        </th>
                      );
                    })}
                  </tr>
                )}
              </thead>
              <tbody>
                {filteredRows.map((row, ri) => (
                  <tr key={ri} className="group odd:bg-card/30 hover:bg-primary/5">
                    <td className="sticky left-0 z-10 w-[42px] min-w-[42px] border-r border-border/40 bg-inherit px-2 py-1 text-right text-[10px] text-muted-foreground whitespace-nowrap">
                      {ri + 1}
                    </td>
                    {cols.map((c) => {
                      const w = colWidths[c];
                      return (
                        <td
                          key={c}
                          className="cursor-pointer border-b border-border/20 px-2 py-1 text-foreground hover:bg-primary/10 overflow-hidden text-ellipsis whitespace-nowrap"
                          style={w ? { width: w, minWidth: w, maxWidth: w } : { maxWidth: 300 }}
                          onClick={() => onCellCopy(renderCell(row[c]))}
                          title={renderCell(row[c])}
                        >
                          <CellValue value={row[c]} />
                        </td>
                      );
                    })}
                  </tr>
                ))}
              </tbody>
            </table>
          )
        ) : (
          /* Pivot view */
          <PivotTable data={pivotData} config={pivotCfg} />
        )}
      </div>
    </div>
  );
}

// ─── Pivot Table ─────────────────────────────────────────────────────

function PivotTable({
  data,
  config,
}: {
  data: Array<Record<string, unknown>> | null;
  config: PivotConfig;
}) {
  if (!data || data.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Configure Group By and Value columns above to see pivot results.
      </div>
    );
  }

  const pivotCols = Object.keys(data[0]).filter((k) => k !== "__group" && k !== "__count");

  return (
    <table className="w-full border-collapse font-mono text-[11px]">
      <thead className="sticky top-0 z-10 bg-card">
        <tr>
          <th className="border-b border-border px-3 py-1.5 text-left font-semibold text-foreground">
            {config.groupBy.join(", ")}
          </th>
          <th className="border-b border-border px-3 py-1.5 text-right font-semibold text-foreground">
            Count
          </th>
          {pivotCols.map((c) => (
            <th
              key={c}
              className="border-b border-border px-3 py-1.5 text-right font-semibold text-foreground"
            >
              {c}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {data.map((row, i) => (
          <tr key={i} className="odd:bg-card/30 hover:bg-primary/5">
            <td className="border-b border-border/20 px-3 py-1 text-foreground">
              {String(row.__group)}
            </td>
            <td className="border-b border-border/20 px-3 py-1 text-right text-info">
              {(row.__count as number).toLocaleString()}
            </td>
            {pivotCols.map((c) => (
              <td
                key={c}
                className="border-b border-border/20 px-3 py-1 text-right text-foreground"
              >
                <CellValue value={row[c]} />
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

// ─── Cell rendering with type-aware styling ──────────────────────────

function CellValue({ value }: { value: unknown }) {
  if (value === null || value === undefined) {
    return <span className="italic text-muted-foreground/60">NULL</span>;
  }
  if (typeof value === "number") {
    return <span className="text-info">{value.toLocaleString()}</span>;
  }
  if (typeof value === "boolean") {
    return (
      <span className={value ? "text-success" : "text-destructive"}>
        {String(value)}
      </span>
    );
  }
  if (typeof value === "string") {
    if (value.length > 120) {
      return (
        <span className="text-foreground" title={value}>
          {value.slice(0, 120)}
          <span className="text-muted-foreground">…</span>
        </span>
      );
    }
    return <span>{value}</span>;
  }
  return <span className="text-muted-foreground">{String(value)}</span>;
}

function renderCell(v: unknown): string {
  if (v === null || v === undefined) return "NULL";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return String(v);
}

function defaultQuery(filePath: string, tables?: TableMeta[]): string {
  // An unresolved SQLite schema must not become a saved catalog query. Leave
  // the editor usable while the captured table list selects its first default.
  if (tables === undefined && /\.(db|sqlite|sqlite3)$/i.test(filePath)) return "";
  if (tables?.length) return `SELECT * FROM ${identifier(tables[0].name)} LIMIT 100;`;
  if (/\.(db|sqlite|sqlite3)$/i.test(filePath)) return "SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name;";
  return "SELECT * FROM data LIMIT 100;";
}

function identifier(name: string): string {
  return `"${name.replaceAll('"', '""')}"`;
}
