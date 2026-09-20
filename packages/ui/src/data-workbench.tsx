import { useEffect, useId, useMemo, useRef, useState } from "react";
import {
  ColumnsIcon,
  DownloadIcon,
  HistoryIcon,
  PlayIcon,
  StopIcon,
} from "@primer/octicons-react";
import { Button, Label, SegmentedControl, Spinner } from "@primer/react";
import { DataTable } from "./data-explorer";
import { displayCell, type TableData } from "./file-preview-model";
import {
  defaultDataQuery,
  MAX_QUERY_ROWS,
  serializeQueryResult,
  sqlIdentifier,
  type QueryFormat,
  type QueryResult,
  type QueryRelation,
  type QuerySession,
} from "./data-query";
import { createDuckDbQuerySession } from "./duckdb-query";
import { createSqliteQuerySession } from "./sqlite-query";
import { SqlEditor, type SqlEditorHandle } from "./sql-editor";

type Props = {
  bytes?: Uint8Array;
  format: QueryFormat;
  name: string;
  size: number;
  url: string;
};

type RunMode = "query" | "explain";
type EnginePhase = "starting" | "ready" | "running" | "cancelling" | "failed";
type WorkbenchError = { summary: string; message: string };
type RunRecord = {
  id: string;
  mode: RunMode;
  query: string;
  status: "success" | "error" | "cancelled";
  at: number;
  elapsedMs?: number;
  rows?: number;
};

const HISTORY_LIMIT = 15;

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function readHistory(key: string) {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(key) ?? "[]");
    if (!Array.isArray(stored)) return [];
    return stored
      .filter((item): item is RunRecord => {
        if (!item || typeof item !== "object") return false;
        const record = item as Record<string, unknown>;
        return (
          typeof record.id === "string" &&
          typeof record.query === "string" &&
          typeof record.at === "number" &&
          (record.mode === "query" || record.mode === "explain") &&
          ["success", "error", "cancelled"].includes(String(record.status))
        );
      })
      .slice(0, HISTORY_LIMIT);
  } catch {
    return [];
  }
}

function downloadResult(data: TableData, format: "csv" | "json", name: string) {
  const blob = new Blob([serializeQueryResult(data, format)], {
    type: format === "csv" ? "text/csv;charset=utf-8" : "application/json",
  });
  const href = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = href;
  anchor.download = `${name.replace(/\.[^.]+$/, "")}-query.${format}`;
  anchor.hidden = true;
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(href), 0);
}

function isNumericColumn(data: TableData, column: number) {
  const values = data.rows
    .slice(0, 100)
    .map((row) => row[column])
    .filter((value) => value !== null && value !== undefined);
  return (
    values.length > 0 &&
    values.every(
      (value) => typeof value === "number" || typeof value === "bigint",
    )
  );
}

function ResultChart({ data }: { data: TableData }) {
  const numeric = useMemo(
    () =>
      data.columns.flatMap((_, index) =>
        isNumericColumn(data, index) ? [index] : [],
      ),
    [data],
  );
  const [x, setX] = useState(0);
  const [y, setY] = useState(numeric[0] ?? 0);
  useEffect(() => {
    setX(0);
    setY(numeric[0] ?? 0);
  }, [data, numeric]);
  if (!numeric.length)
    return (
      <div className="data-chart-empty" role="status">
        Choose a query with at least one numeric column to create a chart.
      </div>
    );
  const rows = data.rows.slice(0, 40);
  const values = rows.map((row) => {
    const value = Number(row[y] ?? 0);
    return Number.isFinite(value) ? value : 0;
  });
  const minimum = Math.min(0, ...values);
  const maximum = Math.max(0, ...values);
  const span = Math.max(maximum - minimum, 1);
  const zero = ((0 - minimum) / span) * 100;
  return (
    <section className="data-chart" aria-label="Query result chart">
      <div className="data-chart-controls">
        <label>
          Label
          <select
            value={x}
            onChange={(event) => setX(Number(event.target.value))}
          >
            {data.columns.map((column, index) => (
              <option key={`${column}:${index}`} value={index}>
                {column}
              </option>
            ))}
          </select>
        </label>
        <label>
          Value
          <select
            value={y}
            onChange={(event) => setY(Number(event.target.value))}
          >
            {numeric.map((index) => (
              <option key={`${data.columns[index]}:${index}`} value={index}>
                {data.columns[index]}
              </option>
            ))}
          </select>
        </label>
        <span className="muted">First {rows.length} result rows</span>
      </div>
      <div className="data-chart-bars">
        {rows.map((row, index) => {
          const value = values[index];
          const point = ((value - minimum) / span) * 100;
          return (
            <div className="data-chart-row" key={index}>
              <span className="data-chart-label" title={displayCell(row[x])}>
                {displayCell(row[x])}
              </span>
              <span
                className="data-chart-track"
                role="img"
                aria-label={`${displayCell(row[x])}: ${displayCell(row[y])}`}
              >
                <span
                  className="data-chart-zero"
                  style={{ left: `${zero}%` }}
                />
                <span
                  className={`data-chart-bar${value < 0 ? " negative" : ""}`}
                  style={{
                    left: `${Math.min(zero, point)}%`,
                    width: `${Math.max(Math.abs(point - zero), 1)}%`,
                  }}
                />
              </span>
              <code>{displayCell(row[y])}</code>
            </div>
          );
        })}
      </div>
    </section>
  );
}

function Schema({
  relations,
  onInsert,
  onSelect,
  selected,
}: {
  relations: QueryRelation[];
  onInsert: (name: string) => void;
  onSelect: (name: string) => void;
  selected?: string;
}) {
  const [query, setQuery] = useState("");
  const term = query.trim().toLocaleLowerCase();
  const visible = relations.flatMap((relation) => {
    const fields = relation.name.toLocaleLowerCase().includes(term)
      ? relation.fields
      : relation.fields.filter((field) =>
          field.name.toLocaleLowerCase().includes(term),
        );
    return fields.length || !term ? [{ ...relation, fields }] : [];
  });
  const fieldCount = relations.reduce(
    (total, relation) => total + relation.fields.length,
    0,
  );
  return (
    <aside className="data-schema" aria-label="Dataset schema">
      <header>
        <div>
          <strong>Schema</strong>
          <span>
            {relations.length.toLocaleString()} relations ·{" "}
            {fieldCount.toLocaleString()} columns
          </span>
        </div>
        {(fieldCount > 8 || relations.length > 1) && (
          <label className="data-schema-search">
            <span className="sr-only">Filter columns</span>
            <ColumnsIcon aria-hidden="true" />
            <input
              type="search"
              value={query}
              placeholder="Filter columns"
              onChange={(event) => setQuery(event.target.value)}
            />
          </label>
        )}
      </header>
      <ol className="data-schema-relations">
        {visible.map((relation) => (
          <li className="data-schema-relation" key={relation.name}>
            <div>
              <button
                type="button"
                aria-current={relation.name === selected ? "true" : undefined}
                title={`Use ${relation.name} for query presets`}
                onClick={() => onSelect(relation.name)}
              >
                {relation.name}
              </button>
              <small>
                {relation.type} · {relation.fields.length.toLocaleString()}
              </small>
            </div>
            <ol>
              {relation.fields.map((field) => (
                <li key={field.name}>
                  <button
                    type="button"
                    title={`Insert ${field.name} into the query`}
                    onClick={() => onInsert(field.name)}
                  >
                    <span>{field.name}</span>
                    <code>{field.type.toLowerCase()}</code>
                    <small>{field.nullable ? "nullable" : "required"}</small>
                  </button>
                </li>
              ))}
            </ol>
          </li>
        ))}
      </ol>
      {query && !visible.length && (
        <p className="data-schema-empty">No columns match “{query}”.</p>
      )}
    </aside>
  );
}

function queryError(reason: unknown, summary: string): WorkbenchError {
  return {
    summary,
    message:
      reason instanceof Error
        ? reason.message
        : "The local query engine returned an unknown error.",
  };
}

export function DataWorkbench({ bytes, format, name, size, url }: Props) {
  const historyKey = `crab:data-workbench:${url}`;
  const [generation, setGeneration] = useState(0);
  const [session, setSession] = useState<QuerySession>();
  const [query, setQuery] = useState(defaultDataQuery);
  const [result, setResult] = useState<QueryResult>();
  const [error, setError] = useState<WorkbenchError>();
  const [notice, setNotice] = useState<string>();
  const [phase, setPhase] = useState<EnginePhase>("starting");
  const [operation, setOperation] = useState<RunMode>("query");
  const [engineProgress, setEngineProgress] = useState<number>();
  const [view, setView] = useState<"table" | "chart">("table");
  const [relationName, setRelationName] = useState<string>();
  const [history, setHistory] = useState<RunRecord[]>(() =>
    readHistory(historyKey),
  );
  const request = useRef(0);
  const restarting = useRef(false);
  const activeRun = useRef<
    { mode: RunMode; query: string; at: number } | undefined
  >(undefined);
  const editor = useRef<SqlEditorHandle>(null);
  const helpId = useId();

  useEffect(() => {
    try {
      localStorage.setItem(historyKey, JSON.stringify(history));
    } catch {
      // Querying remains available when storage is disabled or full.
    }
  }, [history, historyKey]);

  useEffect(() => {
    let active = true;
    let opened: QuerySession | undefined;
    const id = ++request.current;
    setSession(undefined);
    setResult(undefined);
    setError(undefined);
    if (!restarting.current) setNotice(undefined);
    setRelationName(undefined);
    setEngineProgress(undefined);
    setPhase("starting");
    const openSession =
      format === "sqlite" ? createSqliteQuerySession : createDuckDbQuerySession;
    openSession({ bytes, format, name, size, url }, (loaded, total) => {
      if (active && id === request.current && total > 0)
        setEngineProgress(Math.min(100, Math.round((loaded / total) * 100)));
    })
      .then(async (value) => {
        opened = value;
        if (!active) return value.close();
        const initial = await value.query(value.defaultQuery);
        if (active && id === request.current) {
          setSession(value);
          setQuery(value.defaultQuery);
          setRelationName(value.relations[0]?.name);
          setResult(initial);
          setPhase("ready");
          if (restarting.current) {
            restarting.current = false;
            setNotice(
              "Query stopped. The local engine restarted and is ready.",
            );
          }
        }
      })
      .catch(async (reason: unknown) => {
        await opened?.close();
        if (active && id === request.current) {
          restarting.current = false;
          setSession(undefined);
          setError(queryError(reason, "Workbench could not start"));
          setPhase("failed");
        }
      });
    return () => {
      active = false;
      request.current += 1;
      void opened?.close();
    };
  }, [bytes, format, generation, name, size, url]);

  const recordRun = (record: Omit<RunRecord, "id">) => {
    setHistory((current) =>
      [{ ...record, id: crypto.randomUUID() }, ...current].slice(
        0,
        HISTORY_LIMIT,
      ),
    );
  };

  const execute = async (mode: RunMode) => {
    if (!session || phase === "running" || phase === "cancelling") return;
    const sql = query;
    const at = Date.now();
    const id = ++request.current;
    activeRun.current = { mode, query: sql, at };
    setOperation(mode);
    setPhase("running");
    setError(undefined);
    setNotice(undefined);
    try {
      const value =
        mode === "explain"
          ? await session.explain(sql)
          : await session.query(sql);
      if (id === request.current) {
        setResult(value);
        setView("table");
        setPhase("ready");
        recordRun({
          mode,
          query: sql,
          status: "success",
          at,
          elapsedMs: value.elapsedMs,
          rows: value.data.rows.length,
        });
        activeRun.current = undefined;
      }
    } catch (reason) {
      if (id === request.current) {
        setError(queryError(reason, "Query did not run"));
        setPhase("ready");
        recordRun({ mode, query: sql, status: "error", at });
        activeRun.current = undefined;
      }
    }
  };

  const cancel = async () => {
    if (!session || phase !== "running") return;
    const run = activeRun.current;
    request.current += 1;
    setPhase("cancelling");
    setError(undefined);
    try {
      const cancelled = await session.cancel();
      if (cancelled) {
        setPhase("ready");
        setNotice("Query stopped. The current data session is still ready.");
      } else {
        await session.close();
        setSession(undefined);
        setNotice("Query stopped. Restarting the local engine…");
        restarting.current = true;
        setGeneration((value) => value + 1);
      }
    } catch (reason) {
      await session.close();
      setSession(undefined);
      setError(queryError(reason, "Query stopped; engine restart required"));
      restarting.current = true;
      setGeneration((value) => value + 1);
    }
    if (run)
      recordRun({
        mode: run.mode,
        query: run.query,
        status: "cancelled",
        at: run.at,
      });
    activeRun.current = undefined;
  };

  const insertColumn = (field: string) => {
    editor.current?.insertIdentifier(field);
  };

  const sourceMode =
    format === "parquet"
      ? "Column-pruned range reads"
      : format === "sqlite"
        ? "Isolated in-browser copy"
        : format === "arrow"
          ? "In-memory columnar scan"
          : "Streaming source scan";
  const engine = format === "sqlite" ? "SQLite" : "DuckDB";
  const busy = phase === "running" || phase === "cancelling";
  const selectedRelation =
    session?.relations.find((relation) => relation.name === relationName) ??
    session?.relations[0];
  const shortName = name.split("/").pop() ?? name;
  return (
    <section className="data-workbench" aria-label={`${name} query workbench`}>
      <header className="data-workbench-header">
        <div>
          <span className="data-workbench-kicker">Local data workbench</span>
          <strong title={shortName}>{shortName}</strong>
        </div>
        <div className="data-workbench-badges">
          <Label variant="accent">{engine}</Label>
          <span>{format.toUpperCase()}</span>
          <span>{formatBytes(size)}</span>
          <span>{sourceMode}</span>
          <span>Local only</span>
        </div>
      </header>
      <div className="data-workbench-editor">
        <div className="data-query-heading">
          <strong>SQL query</strong>
          <span className="muted" id={helpId}>
            Read-only · Ctrl+Space completes · ⌘/Ctrl+Enter runs
          </span>
        </div>
        <SqlEditor
          ref={editor}
          defaultRelation={selectedRelation?.name}
          describedBy={helpId}
          format={format}
          relations={session?.relations ?? []}
          value={query}
          onChange={setQuery}
          onRun={(explain) => void execute(explain ? "explain" : "query")}
        />
        <div className="data-query-actions">
          <div className="data-query-presets" aria-label="Query examples">
            <button
              type="button"
              onClick={() =>
                setQuery(
                  selectedRelation
                    ? defaultDataQuery(selectedRelation.name)
                    : defaultDataQuery(),
                )
              }
            >
              Sample rows
            </button>
            <button
              type="button"
              onClick={() =>
                setQuery(
                  `SELECT count(*) AS rows\nFROM ${sqlIdentifier(selectedRelation?.name ?? "data")}`,
                )
              }
            >
              Count rows
            </button>
            <button
              type="button"
              disabled={!selectedRelation?.fields.length}
              onClick={() => {
                if (session && selectedRelation)
                  setQuery(
                    session.profileQuery(
                      selectedRelation.fields,
                      selectedRelation.name,
                    ),
                  );
              }}
            >
              Profile columns
            </button>
          </div>
          <div className="data-query-primary-actions">
            {busy ? (
              <Button
                leadingVisual={StopIcon}
                variant="danger"
                disabled={phase === "cancelling"}
                onClick={() => void cancel()}
              >
                {phase === "cancelling" ? "Stopping…" : "Stop query"}
              </Button>
            ) : (
              <>
                <Button
                  disabled={!session}
                  onClick={() => void execute("explain")}
                >
                  Explain
                </Button>
                <Button
                  leadingVisual={PlayIcon}
                  variant="primary"
                  disabled={!session}
                  onClick={() => void execute("query")}
                >
                  Run query
                </Button>
              </>
            )}
          </div>
        </div>
        <details className="data-query-history">
          <summary>
            <HistoryIcon aria-hidden="true" /> Recent runs
            <span>{history.length}</span>
          </summary>
          {history.length ? (
            <div>
              <ol>
                {history.map((item) => (
                  <li key={item.id} className={item.status}>
                    <button type="button" onClick={() => setQuery(item.query)}>
                      <code>
                        {item.query.split("\n", 1)[0] || "Empty query"}
                      </code>
                      <small>
                        {item.mode === "explain" ? "Explain" : "Query"} ·{" "}
                        {item.status}
                        {item.rows !== undefined &&
                          ` · ${item.rows.toLocaleString()} ${item.rows === 1 ? "row" : "rows"}`}
                        {item.elapsedMs !== undefined &&
                          ` · ${item.elapsedMs.toFixed(0)} ms`}
                        {` · ${new Intl.DateTimeFormat(undefined, {
                          hour: "numeric",
                          minute: "2-digit",
                        }).format(item.at)}`}
                      </small>
                    </button>
                  </li>
                ))}
              </ol>
              <button type="button" onClick={() => setHistory([])}>
                Clear history
              </button>
            </div>
          ) : (
            <p>Successful, failed, and stopped queries appear here.</p>
          )}
        </details>
      </div>
      {error && (
        <div className="data-query-error" role="alert">
          <div>
            <strong>{error.summary}</strong>
            <span>{error.message}</span>
          </div>
          {!session && (
            <Button onClick={() => setGeneration((value) => value + 1)}>
              Retry engine
            </Button>
          )}
        </div>
      )}
      {notice && (
        <div className="data-query-notice" role="status">
          {notice}
        </div>
      )}
      <div className="data-workbench-body">
        <Schema
          relations={session?.relations ?? []}
          selected={selectedRelation?.name}
          onInsert={insertColumn}
          onSelect={setRelationName}
        />
        <div className="data-results">
          <div className="data-results-toolbar">
            <SegmentedControl
              aria-label="Result view"
              onChange={(index) => setView(index === 1 ? "chart" : "table")}
            >
              <SegmentedControl.Button selected={view === "table"}>
                Table
              </SegmentedControl.Button>
              <SegmentedControl.Button selected={view === "chart"}>
                Chart
              </SegmentedControl.Button>
            </SegmentedControl>
            <div className="data-results-actions">
              <span className="data-query-stats" aria-live="polite">
                {phase === "starting" ? (
                  <>
                    <Spinner size="small" /> Loading engine
                    {engineProgress !== undefined && ` · ${engineProgress}%`}
                  </>
                ) : phase === "running" ? (
                  <>
                    <Spinner size="small" />
                    {operation === "explain"
                      ? "Building query plan…"
                      : "Running locally…"}
                  </>
                ) : phase === "cancelling" ? (
                  "Stopping query…"
                ) : result ? (
                  <>
                    {result.data.rows.length.toLocaleString()} rows ·{" "}
                    {result.elapsedMs.toFixed(0)} ms
                    {session?.sourceRows !== undefined && (
                      <> · {session.sourceRows.toLocaleString()} source rows</>
                    )}
                    {result.truncated && (
                      <> · {MAX_QUERY_ROWS.toLocaleString()} row cap</>
                    )}
                  </>
                ) : (
                  "Engine unavailable"
                )}
              </span>
              {result && (
                <div className="data-result-downloads">
                  <Button
                    aria-label="Download query result as CSV"
                    leadingVisual={DownloadIcon}
                    onClick={() =>
                      downloadResult(result.data, "csv", shortName)
                    }
                  >
                    CSV
                  </Button>
                  <Button
                    aria-label="Download query result as JSON"
                    onClick={() =>
                      downloadResult(result.data, "json", shortName)
                    }
                  >
                    JSON
                  </Button>
                </div>
              )}
            </div>
          </div>
          {result &&
            (view === "chart" ? (
              <ResultChart data={result.data} />
            ) : (
              <DataTable data={result.data} name="Query results" />
            ))}
        </div>
      </div>
      <footer className="data-workbench-footnote">
        Results and run history stay in this browser. Queries return at most{" "}
        {MAX_QUERY_ROWS.toLocaleString()} rows.{" "}
        {format === "parquet"
          ? "Large Parquet files can skip untouched columns and row groups."
          : format === "sqlite" || format === "arrow"
            ? `${format === "sqlite" ? "SQLite" : "Arrow"} files load into isolated browser memory and are limited to the interactive preview budget.`
            : "CSV and JSON scan source bytes; project only needed columns and use Parquet for repeated large-data analysis."}
      </footer>
    </section>
  );
}
