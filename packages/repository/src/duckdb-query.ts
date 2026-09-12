import * as duckdb from "@duckdb/duckdb-wasm";
import duckdbWasm from "@duckdb/duckdb-wasm/dist/duckdb-mvp.wasm?url";
import duckdbWorker from "@duckdb/duckdb-wasm/dist/duckdb-browser-mvp.worker.js?url";
import type { TableData } from "./file-preview-model";

export const MAX_QUERY_ROWS = 1_000;

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
  close: () => Promise<void>;
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

export function defaultDataQuery() {
  return "SELECT *\nFROM data\nLIMIT 100";
}

export function boundedReadQuery(sql: string) {
  const query = sql.trim().replace(/;\s*$/, "");
  if (!query) throw new Error("Enter a SQL query to run.");
  if (query.includes(";"))
    throw new Error(
      "Run one query at a time; multiple statements are disabled.",
    );
  const executable = query.replace(
    /^(?:(?:--[^\n]*(?:\n|$))|(?:\/\*[\s\S]*?\*\/)|\s)*/,
    "",
  );
  if (!/^(select|with)\b/i.test(executable))
    throw new Error("Only read-only SELECT or WITH queries are allowed.");
  return `SELECT * FROM (${query}) AS crab_query_result LIMIT ${MAX_QUERY_ROWS + 1}`;
}

function toTableData(table: ArrowResult): TableData {
  const columns = table.schema.fields.map((field) => field.name);
  const count = Math.min(table.numRows, MAX_QUERY_ROWS);
  const vectors = columns.map((_, index) => table.getChildAt(index));
  return {
    columns,
    rows: Array.from({ length: count }, (_, row) =>
      vectors.map((vector) => vector?.get(row)),
    ),
    totalRows: count,
    note:
      table.numRows > MAX_QUERY_ROWS
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
): Promise<QuerySession> {
  const worker = new Worker(duckdbWorker);
  const database = new duckdb.AsyncDuckDB(new duckdb.VoidLogger(), worker);
  let connection: duckdb.AsyncDuckDBConnection | undefined;
  try {
    await database.instantiate(duckdbWasm);
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
    return {
      schema,
      sourceRows,
      query: async (sql) => {
        const started = performance.now();
        const result = (await connection?.query(
          boundedReadQuery(sql),
        )) as ArrowResult;
        if (!result) throw new Error("The query session was closed.");
        return {
          data: toTableData(result),
          elapsedMs: performance.now() - started,
          truncated: result.numRows > MAX_QUERY_ROWS,
        };
      },
      close: async () => {
        await connection?.close().catch(() => undefined);
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
