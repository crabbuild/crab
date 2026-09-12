import * as duckdb from "@duckdb/duckdb-wasm";
import duckdbWasm from "@duckdb/duckdb-wasm/dist/duckdb-mvp.wasm?url";
import duckdbWorker from "@duckdb/duckdb-wasm/dist/duckdb-browser-mvp.worker.js?url";
import type { TableData } from "./file-preview-model";

export const MAX_QUERY_ROWS = 1_000;
const MAX_PROFILE_COLUMNS = 100;

// Worker responses carry CSP and are cached immutably. Change this revision
// whenever the worker's allowed network policy changes to avoid stale policy.
const DUCKDB_WORKER_POLICY = "duckdb-extensions-v1";

export type QueryFormat = "csv" | "tsv" | "json" | "jsonl" | "parquet";

export type QuerySource = {
  format: QueryFormat;
  name: string;
  size: number;
  url: string;
};

export type QuerySchemaField = {
  name: string;
  type: string;
  nullable: boolean;
};

export type QueryResult = {
  data: TableData;
  elapsedMs: number;
  truncated: boolean;
};

export type QuerySession = {
  cancel: () => Promise<boolean>;
  close: () => Promise<void>;
  explain: (sql: string) => Promise<QueryResult>;
  query: (sql: string) => Promise<QueryResult>;
  schema: QuerySchemaField[];
  sourceRows?: number | bigint;
};

type ArrowResult = {
  getChildAt(index: number): { get(index: number): unknown } | null;
  numRows: number;
  schema: { fields: Array<{ name: string }> };
};

function sqlString(value: string) {
  return `'${value.replaceAll("'", "''")}'`;
}

function sqlIdentifier(value: string) {
  return `"${value.replaceAll('"', '""')}"`;
}

export function defaultDataQuery() {
  return "SELECT *\nFROM data\nLIMIT 100";
}

function statementBoundary(sql: string) {
  let quote: "'" | '"' | undefined;
  let lineComment = false;
  let blockComment = false;
  let statementEnded = false;
  let terminalSeparator: number | undefined;
  for (let index = 0; index < sql.length; index += 1) {
    const character = sql[index];
    const next = sql[index + 1];
    if (lineComment) {
      if (character === "\n") lineComment = false;
      continue;
    }
    if (blockComment) {
      if (character === "*" && next === "/") {
        blockComment = false;
        index += 1;
      }
      continue;
    }
    if (quote) {
      if (character === quote && next === quote) {
        index += 1;
      } else if (character === quote) {
        quote = undefined;
      }
      continue;
    }
    if (/\s/.test(character)) continue;
    if (character === "-" && next === "-") {
      lineComment = true;
      index += 1;
    } else if (character === "/" && next === "*") {
      blockComment = true;
      index += 1;
    } else if (character === "'" || character === '"') {
      if (statementEnded) return { multiple: true };
      quote = character;
    } else if (character === ";") {
      statementEnded = true;
      terminalSeparator = index;
    } else if (statementEnded) {
      return { multiple: true };
    }
  }
  return { multiple: false, terminalSeparator };
}

function readQuery(sql: string) {
  const trimmed = sql.trim();
  if (!trimmed) throw new Error("Enter a SQL query to run.");
  const boundary = statementBoundary(trimmed);
  if (boundary.multiple)
    throw new Error(
      "Run one query at a time; multiple statements are disabled.",
    );
  const query =
    boundary.terminalSeparator === undefined
      ? trimmed
      : `${trimmed.slice(0, boundary.terminalSeparator)}${trimmed.slice(boundary.terminalSeparator + 1)}`.trim();
  const executable = query.replace(
    /^(?:(?:--[^\n]*(?:\n|$))|(?:\/\*[\s\S]*?\*\/)|\s)*/,
    "",
  );
  if (!/^(select|with)\b/i.test(executable))
    throw new Error("Only read-only SELECT or WITH queries are allowed.");
  return query;
}

export function boundedReadQuery(sql: string) {
  const query = readQuery(sql);
  return `SELECT * FROM (${query}) AS crab_query_result LIMIT ${MAX_QUERY_ROWS + 1}`;
}

export function explainReadQuery(sql: string) {
  return `EXPLAIN ${readQuery(sql)}`;
}

export function profileDataQuery(fields: QuerySchemaField[]) {
  if (!fields.length) return defaultDataQuery();
  const query = fields
    .slice(0, MAX_PROFILE_COLUMNS)
    .map((field) => {
      const column = sqlIdentifier(field.name);
      return `SELECT ${sqlString(field.name)} AS column_name, count(*) AS rows, count(${column}) AS populated, count(*) - count(${column}) AS nulls, approx_count_distinct(${column}) AS approx_distinct FROM data`;
    })
    .join("\nUNION ALL\n");
  return fields.length > MAX_PROFILE_COLUMNS
    ? `-- Profiling the first ${MAX_PROFILE_COLUMNS} of ${fields.length} columns\n${query}`
    : query;
}

function downloadableCell(value: unknown) {
  if (value === null || value === undefined) return null;
  if (typeof value === "bigint") return value.toString();
  if (value instanceof Uint8Array)
    return Array.from(value, (byte) => byte.toString(16).padStart(2, "0")).join(
      "",
    );
  if (value instanceof Date) return value.toISOString();
  return value;
}

function csvCell(value: unknown) {
  const normalized = downloadableCell(value);
  const text =
    normalized !== null && typeof normalized === "object"
      ? JSON.stringify(normalized)
      : String(normalized ?? "");
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

export function serializeQueryResult(data: TableData, format: "csv" | "json") {
  if (format === "csv")
    return [
      data.columns.map(csvCell).join(","),
      ...data.rows.map((row) => row.map(csvCell).join(",")),
    ].join("\r\n");
  return JSON.stringify(
    data.rows.map((row) =>
      Object.fromEntries(
        data.columns.map((column, index) => [
          column,
          downloadableCell(row[index]),
        ]),
      ),
    ),
    null,
    2,
  );
}

function batchesToTableData(
  batches: ArrowResult[],
  columns: string[],
): TableData {
  const totalRows = batches.reduce((total, batch) => total + batch.numRows, 0);
  const rows: unknown[][] = [];
  for (const batch of batches) {
    const vectors = columns.map((_, index) => batch.getChildAt(index));
    for (
      let row = 0;
      row < batch.numRows && rows.length < MAX_QUERY_ROWS;
      row += 1
    )
      rows.push(vectors.map((vector) => vector?.get(row)));
  }
  return {
    columns,
    rows,
    totalRows: rows.length,
    note:
      totalRows > MAX_QUERY_ROWS
        ? `Result capped at ${MAX_QUERY_ROWS.toLocaleString()} rows. Add filters or aggregation to narrow it.`
        : undefined,
  };
}

function sourceExpression(format: QueryFormat, fileName: string) {
  const file = sqlString(fileName);
  if (format === "parquet") return `read_parquet(${file})`;
  if (format === "tsv")
    return `read_csv_auto(${file}, delim='\\t', header=true)`;
  if (format === "csv") return `read_csv_auto(${file}, header=true)`;
  return `read_json_auto(${file}, format='auto')`;
}

function valueAt(table: ArrowResult, row: number, column: number) {
  return table.getChildAt(column)?.get(row);
}

export async function createQuerySession(
  source: QuerySource,
  onProgress?: (loaded: number, total: number) => void,
): Promise<QuerySession> {
  const workerUrl = new URL(duckdbWorker, window.location.href);
  workerUrl.searchParams.set("worker-policy", DUCKDB_WORKER_POLICY);
  const worker = new Worker(workerUrl);
  const database = new duckdb.AsyncDuckDB(new duckdb.VoidLogger(), worker);
  let connection: duckdb.AsyncDuckDBConnection | undefined;
  try {
    await database.instantiate(duckdbWasm, null, (progress) =>
      onProgress?.(progress.bytesLoaded, progress.bytesTotal),
    );
    await database.open({
      query: {
        castBigIntToDouble: false,
        castDecimalToDouble: false,
        castDurationToTime64: false,
        castTimestampToDate: true,
      },
    });
    const fileName = `source.${source.format === "jsonl" ? "json" : source.format}`;
    await database.registerFileURL(
      fileName,
      new URL(source.url, window.location.href).toString(),
      duckdb.DuckDBDataProtocol.HTTP,
      false,
    );
    connection = await database.connect();
    await connection.query(
      `CREATE VIEW data AS SELECT * FROM ${sourceExpression(source.format, fileName)}`,
    );
    const description = (await connection.query(
      "DESCRIBE data",
    )) as ArrowResult;
    const schema = Array.from({ length: description.numRows }, (_, row) => ({
      name: String(valueAt(description, row, 0) ?? ""),
      type: String(valueAt(description, row, 1) ?? "unknown"),
      nullable: String(valueAt(description, row, 2) ?? "YES") === "YES",
    }));
    let sourceRows: number | bigint | undefined;
    if (source.format === "parquet") {
      const count = (await connection.query(
        "SELECT count(*) AS rows FROM data",
      )) as ArrowResult;
      const value = valueAt(count, 0, 0);
      sourceRows = typeof value === "bigint" ? value : Number(value);
    }
    const liveConnection = connection;
    const execute = async (sql: string) => {
      const reader = await liveConnection.send(sql);
      const batches = (await reader.readAll()) as ArrowResult[];
      return {
        batches,
        columns: batches[0]?.schema.fields.map((field) => field.name) ?? [],
        numRows: batches.reduce((total, batch) => total + batch.numRows, 0),
      };
    };
    const result = async (sql: string, explain: boolean) => {
      const started = performance.now();
      const table = await execute(
        explain ? explainReadQuery(sql) : boundedReadQuery(sql),
      );
      return {
        data: batchesToTableData(table.batches, table.columns),
        elapsedMs: performance.now() - started,
        truncated: !explain && table.numRows > MAX_QUERY_ROWS,
      };
    };
    return {
      schema,
      sourceRows,
      cancel: () => liveConnection.cancelSent(),
      explain: (sql) => result(sql, true),
      query: (sql) => result(sql, false),
      close: async () => {
        await liveConnection.close().catch(() => undefined);
        await database.terminate().catch(() => undefined);
        worker.terminate();
      },
    };
  } catch (error) {
    await connection?.close().catch(() => undefined);
    await database.terminate().catch(() => undefined);
    worker.terminate();
    throw error;
  }
}
