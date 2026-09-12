import { useEffect, useMemo, useRef, useState } from "react";
import { Button, Label, SegmentedControl, Spinner } from "@primer/react";
import { DataTable } from "./data-explorer";
import { displayCell, type TableData } from "./file-preview-model";
import {
  createQuerySession,
  defaultDataQuery,
  MAX_QUERY_ROWS,
  type QueryFormat,
  type QueryResult,
  type QuerySchemaField,
  type QuerySession,
} from "./duckdb-query";

type Props = {
  format: QueryFormat;
  name: string;
  size: number;
  url: string;
};

function formatBytes(bytes: number) {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
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
        This result has no numeric column to chart.
      </div>
    );
  const rows = data.rows.slice(0, 40);
  const values = rows.map((row) => Math.abs(Number(row[y] ?? 0)));
  const maximum = Math.max(...values, 1);
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
              <option key={data.columns[index]} value={index}>
                {data.columns[index]}
              </option>
            ))}
          </select>
        </label>
        <span className="muted">First {rows.length} result rows</span>
      </div>
      <div className="data-chart-bars">
        {rows.map((row, index) => (
          <div className="data-chart-row" key={index}>
            <span className="data-chart-label" title={displayCell(row[x])}>
              {displayCell(row[x])}
            </span>
            <span className="data-chart-track">
              <span
                className="data-chart-bar"
                style={{
                  width: `${Math.max((values[index] / maximum) * 100, 1)}%`,
                }}
              />
            </span>
            <code>{displayCell(row[y])}</code>
          </div>
        ))}
      </div>
    </section>
  );
}

function Schema({ fields }: { fields: QuerySchemaField[] }) {
  return (
    <aside className="data-schema" aria-label="Dataset schema">
      <header>
        <strong>data</strong>
        <span>{fields.length} columns</span>
      </header>
      <ol>
        {fields.map((field) => (
          <li key={field.name}>
            <span title={field.name}>{field.name}</span>
            <code>{field.type.toLowerCase()}</code>
            {!field.nullable && <span title="Required">•</span>}
          </li>
        ))}
      </ol>
    </aside>
  );
}

export function DataWorkbench({ format, name, size, url }: Props) {
  const [generation, setGeneration] = useState(0);
  const [session, setSession] = useState<QuerySession>();
  const [query, setQuery] = useState(defaultDataQuery);
  const [result, setResult] = useState<QueryResult>();
  const [error, setError] = useState<string>();
  const [running, setRunning] = useState(false);
  const [view, setView] = useState<"table" | "chart">("table");
  const request = useRef(0);

  useEffect(() => {
    let active = true;
    let opened: QuerySession | undefined;
    const id = ++request.current;
    setSession(undefined);
    setResult(undefined);
    setError(undefined);
    setRunning(true);
    createQuerySession({ format, name, size, url })
      .then(async (value) => {
        opened = value;
        if (!active) return value.close();
        setSession(value);
        const initial = await value.query(defaultDataQuery());
        if (active && id === request.current) setResult(initial);
      })
      .catch((reason: unknown) => {
        if (active && id === request.current)
          setError(
            reason instanceof Error
              ? reason.message
              : "The query engine could not start.",
          );
      })
      .finally(() => {
        if (active && id === request.current) setRunning(false);
      });
    return () => {
      active = false;
      request.current += 1;
      void opened?.close();
    };
  }, [format, generation, name, size, url]);

  const run = async () => {
    if (!session || running) return;
    const id = ++request.current;
    setRunning(true);
    setError(undefined);
    try {
      const value = await session.query(query);
      if (id === request.current) {
        setResult(value);
        setView("table");
      }
    } catch (reason) {
      if (id === request.current)
        setError(
          reason instanceof Error ? reason.message : "The query failed.",
        );
    } finally {
      if (id === request.current) setRunning(false);
    }
  };

  const cancel = async () => {
    request.current += 1;
    setRunning(false);
    setError("Query cancelled. The local engine was restarted.");
    await session?.close();
    setSession(undefined);
    setGeneration((value) => value + 1);
  };

  const sourceMode =
    format === "parquet"
      ? "Column-pruned range reads"
      : "Streaming scan for each query";
  return (
    <section className="data-workbench" aria-label={`${name} query workbench`}>
      <header className="data-workbench-header">
        <div>
          <span className="data-workbench-kicker">Local data workbench</span>
          <strong>{name.split("/").pop()}</strong>
        </div>
        <div className="data-workbench-badges">
          <Label variant="accent">DuckDB</Label>
          <span>{format.toUpperCase()}</span>
          <span>{formatBytes(size)}</span>
          <span>{sourceMode}</span>
        </div>
      </header>
      <div className="data-workbench-editor">
        <div className="data-query-heading">
          <strong>Query</strong>
          <span className="muted">Read-only · ⌘/Ctrl + Enter to run</span>
        </div>
        <textarea
          aria-label="SQL query"
          spellCheck={false}
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
              event.preventDefault();
              void run();
            }
          }}
        />
        <div className="data-query-actions">
          <div className="data-query-presets" aria-label="Query examples">
            <button type="button" onClick={() => setQuery(defaultDataQuery())}>
              Sample rows
            </button>
            <button
              type="button"
              onClick={() => setQuery("SELECT count(*) AS rows\nFROM data")}
            >
              Count rows
            </button>
          </div>
          {running && session ? (
            <Button variant="danger" onClick={() => void cancel()}>
              Stop query
            </Button>
          ) : (
            <Button
              variant="primary"
              disabled={!session}
              onClick={() => void run()}
            >
              Run query
            </Button>
          )}
        </div>
      </div>
      {error && (
        <div className="data-query-error" role="alert">
          <strong>Query did not run</strong>
          <span>{error}</span>
        </div>
      )}
      <div className="data-workbench-body">
        <Schema fields={session?.schema ?? []} />
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
            <span className="data-query-stats" aria-live="polite">
              {running ? (
                <>
                  <Spinner size="small" /> Running locally…
                </>
              ) : result ? (
                <>
                  {result.data.rows.length.toLocaleString()} rows ·{" "}
                  {result.elapsedMs.toFixed(0)} ms
                  {session?.sourceRows !== undefined && (
                    <> · {session.sourceRows.toLocaleString()} source rows</>
                  )}
                  {result.truncated && (
                    <> · {MAX_QUERY_ROWS.toLocaleString()} row cap reached</>
                  )}
                </>
              ) : (
                "Preparing the local engine…"
              )}
            </span>
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
        Results stay in this browser and are capped at{" "}
        {MAX_QUERY_ROWS.toLocaleString()} rows.{" "}
        {format === "parquet"
          ? "Large Parquet files can skip untouched columns and row groups."
          : "CSV and JSON must scan source bytes; use filters, projections, and Parquet for repeated large-data analysis."}
      </footer>
    </section>
  );
}
