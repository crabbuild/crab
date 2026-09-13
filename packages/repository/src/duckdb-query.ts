import * as duckdb from "@duckdb/duckdb-wasm";
import duckdbWasm from "@duckdb/duckdb-wasm/dist/duckdb-mvp.wasm?url";
import duckdbWorker from "@duckdb/duckdb-wasm/dist/duckdb-browser-mvp.worker.js?url";
import {
  boundedReadQuery,
  defaultDataQuery,
  explainReadQuery,
  MAX_QUERY_ROWS,
  profileDataQuery,
  sqlString,
  type QueryFormat,
  type QueryResult,
  type QuerySchemaField,
  type QuerySession,
  type QuerySource,
} from "./data-query";
import type { TableData } from "./file-preview-model";

// Worker responses carry CSP and are cached immutably. Change this revision
// whenever the worker's allowed network policy changes to avoid stale policy.
const DUCKDB_WORKER_POLICY = "duckdb-extensions-v1";

type ArrowResult = {
  getChildAt(index: number): { get(index: number): unknown } | null;
  numRows: number;
  schema: { fields: Array<{ name: string }> };
};

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

async function loadArrowSource(
  connection: duckdb.AsyncDuckDBConnection,
  source: QuerySource,
) {
  let bytes = source.bytes;
  if (!bytes) {
    const response = await fetch(source.url, {
      headers: { Accept: "application/octet-stream" },
    });
    if (!response.ok)
      throw new Error(`Arrow bytes could not be loaded (${response.status})`);
    bytes = new Uint8Array(await response.arrayBuffer());
  }
  const { tableFromIPC, tableToIPC } = await import("apache-arrow");
  const stream = tableToIPC(tableFromIPC(bytes), "stream");
  await connection.insertArrowFromIPCStream(stream, {
    create: true,
    name: "data",
  });
}

export async function createDuckDbQuerySession(
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
    connection = await database.connect();
    if (source.format === "arrow") {
      await loadArrowSource(connection, source);
    } else {
      const fileName = `source.${source.format === "jsonl" ? "json" : source.format}`;
      await database.registerFileURL(
        fileName,
        new URL(source.url, window.location.href).toString(),
        duckdb.DuckDBDataProtocol.HTTP,
        false,
      );
      await connection.query(
        `CREATE VIEW data AS SELECT * FROM ${sourceExpression(source.format, fileName)}`,
      );
    }
    const description = (await connection.query(
      "DESCRIBE data",
    )) as ArrowResult;
    const fields: QuerySchemaField[] = Array.from(
      { length: description.numRows },
      (_, row) => ({
        name: String(valueAt(description, row, 0) ?? ""),
        type: String(valueAt(description, row, 1) ?? "unknown"),
        nullable: String(valueAt(description, row, 2) ?? "YES") === "YES",
      }),
    );
    let sourceRows: number | bigint | undefined;
    if (source.format === "parquet" || source.format === "arrow") {
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
      } satisfies QueryResult;
    };
    return {
      defaultQuery: defaultDataQuery(),
      relations: [{ name: "data", type: "view", fields }],
      sourceRows,
      cancel: () => liveConnection.cancelSent(),
      explain: (sql) => result(sql, true),
      profileQuery: profileDataQuery,
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
