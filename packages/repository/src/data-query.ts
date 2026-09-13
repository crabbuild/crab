import type { TableData } from "./file-preview-model";

export const MAX_QUERY_ROWS = 1_000;
const MAX_PROFILE_COLUMNS = 100;

export type QueryFormat =
  | "arrow"
  | "csv"
  | "json"
  | "jsonl"
  | "parquet"
  | "sqlite"
  | "tsv";

export type QuerySource = {
  bytes?: Uint8Array;
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

export type QueryRelation = {
  fields: QuerySchemaField[];
  name: string;
  type: "table" | "view";
};

export type QueryResult = {
  data: TableData;
  elapsedMs: number;
  truncated: boolean;
};

export type QuerySession = {
  cancel: () => Promise<boolean>;
  close: () => Promise<void>;
  defaultQuery: string;
  explain: (sql: string) => Promise<QueryResult>;
  profileQuery: (fields: QuerySchemaField[], relation: string) => string;
  query: (sql: string) => Promise<QueryResult>;
  relations: QueryRelation[];
  sourceRows?: number | bigint;
};

export function sqlString(value: string) {
  return `'${value.replaceAll("'", "''")}'`;
}

export function sqlIdentifier(value: string) {
  return `"${value.replaceAll('"', '""')}"`;
}

export function defaultDataQuery(relation = "data") {
  return `SELECT *\nFROM ${sqlIdentifier(relation)}\nLIMIT 100`;
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

export function readQuery(sql: string) {
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
  return `SELECT * FROM (${readQuery(sql)}) AS crab_query_result LIMIT ${MAX_QUERY_ROWS + 1}`;
}

export function explainReadQuery(sql: string) {
  return `EXPLAIN ${readQuery(sql)}`;
}

export function profileDataQuery(
  fields: QuerySchemaField[],
  relation = "data",
) {
  if (!fields.length) return defaultDataQuery(relation);
  const table = sqlIdentifier(relation);
  const query = fields
    .slice(0, MAX_PROFILE_COLUMNS)
    .map((field) => {
      const column = sqlIdentifier(field.name);
      return `SELECT ${sqlString(field.name)} AS column_name, count(*) AS rows, count(${column}) AS populated, count(*) - count(${column}) AS nulls, approx_count_distinct(${column}) AS approx_distinct FROM ${table}`;
    })
    .join("\nUNION ALL\n");
  return fields.length > MAX_PROFILE_COLUMNS
    ? `-- Profiling the first ${MAX_PROFILE_COLUMNS} of ${fields.length} columns\n${query}`
    : query;
}

export function profileSqliteDataQuery(
  fields: QuerySchemaField[],
  relation: string,
) {
  if (!fields.length) return defaultDataQuery(relation);
  const table = sqlIdentifier(relation);
  const query = fields
    .slice(0, MAX_PROFILE_COLUMNS)
    .map((field) => {
      const column = sqlIdentifier(field.name);
      return `SELECT ${sqlString(field.name)} AS column_name, count(*) AS rows, count(${column}) AS populated, count(*) - count(${column}) AS nulls, count(DISTINCT ${column}) AS distinct_values FROM ${table}`;
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
