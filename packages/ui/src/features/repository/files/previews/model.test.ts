import { describe, expect, it } from "vitest";
import {
  decodeDelimitedText,
  parseNumpyHeader,
  parseSafetensors,
  previewDescriptor,
} from "./model";
import {
  boundedReadQuery,
  defaultDataQuery,
  explainReadQuery,
  MAX_QUERY_ROWS,
  profileDataQuery,
  profileSqliteDataQuery,
  serializeQueryResult,
} from "./data/query";

describe("file preview classification", () => {
  it.each([
    ["diagram.svg", "image"],
    ["paper.PDF", "pdf"],
    ["metrics.csv", "delimited"],
    ["analysis.ipynb", "notebook"],
    ["report.xlsx", "office"],
    ["events.sqlite3", "sqlite"],
    ["features.parquet", "parquet"],
    ["batch.arrow", "arrow"],
    ["weights.safetensors", "safetensors"],
    ["model.gguf", "gguf"],
    ["package.whl", "archive"],
  ])("routes %s to the %s preview", (name, kind) => {
    expect(previewDescriptor(name)?.kind).toBe(kind);
  });

  it("leaves unknown executable formats on the exact-byte fallback", () => {
    expect(previewDescriptor("release.exe")).toBeNull();
  });
});

describe("delimited text encodings", () => {
  it.each([
    [
      "Unicode UTF-8",
      new TextEncoder().encode("£,café,東京,🦀"),
      "£,café,東京,🦀",
      "utf-8",
    ],
    ["UTF-8 BOM", new Uint8Array([0xef, 0xbb, 0xbf, 0x41]), "A", "utf-8"],
    [
      "Windows-1252",
      new Uint8Array([0xa3, 0xe9, 0x80, 0x93, 0x94]),
      "£é€“”",
      "windows-1252",
    ],
    ["UTF-16LE", new Uint8Array([0xff, 0xfe, 0xa3, 0]), "£", "utf-16le"],
    ["UTF-16BE", new Uint8Array([0xfe, 0xff, 0, 0xa3]), "£", "utf-16be"],
  ])("preserves characters in %s", (_name, bytes, text, encoding) => {
    expect(decodeDelimitedText(bytes as Uint8Array)).toEqual({
      text,
      encoding,
    });
  });

  it.each([
    ["UTF-8", new Uint8Array([0xef, 0xbb, 0xbf, 0xff])],
    ["UTF-16", new Uint8Array([0xff, 0xfe, 0x41])],
  ])("rejects malformed text with an explicit %s BOM", (_name, bytes) => {
    expect(() => decodeDelimitedText(bytes as Uint8Array)).toThrow();
  });
});

describe("interactive data queries", () => {
  it("wraps one read query in a hard result limit", () => {
    expect(
      boundedReadQuery("WITH sample AS (SELECT 1 AS id) SELECT * FROM sample;"),
    ).toBe(
      `SELECT * FROM (WITH sample AS (SELECT 1 AS id) SELECT * FROM sample) AS crab_query_result LIMIT ${MAX_QUERY_ROWS + 1}`,
    );
    expect(defaultDataQuery()).toContain('FROM "data"');
    expect(defaultDataQuery('experiment "runs"')).toContain(
      'FROM "experiment ""runs"""',
    );
  });

  it("accepts semicolons in values and discards a terminal separator", () => {
    expect(
      boundedReadQuery("SELECT ';' AS punctuation; -- retained comment"),
    ).toBe(
      `SELECT * FROM (SELECT ';' AS punctuation -- retained comment) AS crab_query_result LIMIT ${MAX_QUERY_ROWS + 1}`,
    );
  });

  it("builds read-only explain and column-quality queries", () => {
    expect(explainReadQuery("SELECT score FROM data;")).toBe(
      "EXPLAIN SELECT score FROM data",
    );
    expect(
      profileDataQuery([
        { name: 'run "id"', type: "INTEGER", nullable: false },
      ]),
    ).toContain('count("run ""id""") AS populated');
    expect(
      profileSqliteDataQuery(
        [{ name: "run", type: "INTEGER", nullable: false }],
        "experiment runs",
      ),
    ).toContain(
      'count(DISTINCT "run") AS distinct_values FROM "experiment runs"',
    );
  });

  it.each([
    "DELETE FROM data",
    "COPY data TO 'remote.csv'",
    "SELECT * FROM data; SELECT * FROM data",
  ])("rejects unsafe or multiple statements: %s", (query) => {
    expect(() => boundedReadQuery(query)).toThrow();
  });

  it("exports typed query results without losing nulls or big integers", () => {
    const data = {
      columns: ["name", "count", "note"],
      rows: [["crab, build", 9_007_199_254_740_993n, null]],
      totalRows: 1,
    };
    expect(serializeQueryResult(data, "csv")).toBe(
      'name,count,note\r\n"crab, build",9007199254740993,',
    );
    expect(JSON.parse(serializeQueryResult(data, "json"))).toEqual([
      { name: "crab, build", count: "9007199254740993", note: null },
    ]);
  });
});

describe("machine-learning metadata previews", () => {
  it("reads Safetensors tensor names, shapes, and byte spans", () => {
    const header = new TextEncoder().encode(
      JSON.stringify({
        encoder: { dtype: "F32", shape: [2, 3], data_offsets: [0, 24] },
      }),
    );
    const bytes = new Uint8Array(8 + header.length + 24);
    new DataView(bytes.buffer).setBigUint64(0, BigInt(header.length), true);
    bytes.set(header, 8);
    expect(parseSafetensors(bytes).rows).toEqual([
      ["encoder", "F32", "2 × 3", 24],
    ]);
  });

  it("reads NumPy dtype, shape, order, and payload offset", () => {
    const rawHeader =
      "{'descr': '<f4', 'fortran_order': False, 'shape': (2, 3), }";
    const padding = " ".repeat((16 - ((10 + rawHeader.length + 1) % 16)) % 16);
    const header = new TextEncoder().encode(`${rawHeader}${padding}\n`);
    const bytes = new Uint8Array(10 + header.length + 24);
    bytes.set([0x93, 0x4e, 0x55, 0x4d, 0x50, 0x59, 1, 0], 0);
    new DataView(bytes.buffer).setUint16(8, header.length, true);
    bytes.set(header, 10);
    expect(parseNumpyHeader(bytes)).toEqual({
      dtype: "<f4",
      shape: [2, 3],
      order: "C",
      dataOffset: 10 + header.length,
    });
  });
});
