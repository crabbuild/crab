import initSqlJs from "sql.js";
import sqlWasmUrl from "sql.js/dist/sql-wasm.wasm?url";
import {
  boundedReadQuery,
  MAX_QUERY_ROWS,
  readQuery,
  sqlIdentifier,
  type QueryRelation,
  type QueryResult,
} from "./data-query";

type WorkerRequest =
  | { type: "close" }
  | { bytes: Uint8Array; type: "open" }
  | { id: number; mode: "explain" | "query"; sql: string; type: "run" };

let database: import("sql.js").Database | undefined;

function userRelations(database: import("sql.js").Database) {
  const catalog = database.exec(
    "SELECT name, type FROM sqlite_master " +
      "WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' " +
      "ORDER BY name LIMIT 100",
  )[0];
  return (catalog?.values ?? []).map(([nameValue, typeValue]) => {
    const name = String(nameValue);
    const info = database.exec(`PRAGMA table_info(${sqlIdentifier(name)})`)[0];
    return {
      name,
      type: String(typeValue) === "view" ? "view" : "table",
      fields: (info?.values ?? []).map((row) => ({
        name: String(row[1] ?? ""),
        type: String(row[2] || "unknown"),
        nullable: Number(row[3]) === 0,
      })),
    } satisfies QueryRelation;
  });
}

function run(sql: string, mode: "explain" | "query") {
  if (!database) throw new Error("The SQLite session is not open.");
  const started = performance.now();
  const statement =
    mode === "explain"
      ? `EXPLAIN QUERY PLAN ${readQuery(sql)}`
      : boundedReadQuery(sql);
  const result = database.exec(statement)[0];
  const values = result?.values ?? [];
  const rows = values.slice(0, MAX_QUERY_ROWS);
  return {
    data: {
      columns: result?.columns ?? [],
      rows,
      totalRows: rows.length,
      note:
        values.length > MAX_QUERY_ROWS
          ? `Result capped at ${MAX_QUERY_ROWS.toLocaleString()} rows. Add filters or aggregation to narrow it.`
          : undefined,
    },
    elapsedMs: performance.now() - started,
    truncated: mode === "query" && values.length > MAX_QUERY_ROWS,
  } satisfies QueryResult;
}

self.onmessage = async (event: MessageEvent<WorkerRequest>) => {
  try {
    if (event.data.type === "open") {
      const SQL = await initSqlJs({ locateFile: () => sqlWasmUrl });
      database = new SQL.Database(event.data.bytes);
      database.run("PRAGMA query_only=ON");
      const relations = userRelations(database);
      if (!relations.length)
        throw new Error("This SQLite database has no user tables or views.");
      const defaultRelation = relations[0];
      const count = database.exec(
        `SELECT count(*) AS rows FROM ${sqlIdentifier(defaultRelation.name)}`,
      )[0]?.values[0]?.[0];
      self.postMessage({
        type: "ready",
        defaultRelation: defaultRelation.name,
        relations,
        sourceRows: typeof count === "number" ? count : undefined,
      });
      return;
    }
    if (event.data.type === "close") {
      database?.close();
      database = undefined;
      self.close();
      return;
    }
    self.postMessage({
      type: "result",
      id: event.data.id,
      result: run(event.data.sql, event.data.mode),
    });
  } catch (error) {
    self.postMessage({
      type: "error",
      id: event.data.type === "run" ? event.data.id : undefined,
      message: error instanceof Error ? error.message : "SQLite query failed.",
    });
  }
};
