import { describe, expect, it } from "vitest";
import {
  parseDelimited,
  parseJsonTable,
  parseNumpyHeader,
  parseSafetensors,
  previewDescriptor,
} from "./file-preview-model";

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

describe("tabular previews", () => {
  it("parses quoted delimiters, escaped quotes, and line breaks", () => {
    expect(
      parseDelimited(
        'name,notes\nAlice,"one,two"\nBob,"said ""ship""\nnow"\n',
        ",",
      ),
    ).toMatchObject({
      columns: ["name", "notes"],
      rows: [
        ["Alice", "one,two"],
        ["Bob", 'said "ship"\nnow'],
      ],
      totalRows: 2,
    });
  });

  it("normalizes JSON objects into a union-column table", () => {
    expect(
      parseJsonTable('[{"name":"Alice"},{"name":"Bob","score":9}]', false),
    ).toEqual({
      columns: ["name", "score"],
      rows: [
        ["Alice", undefined],
        ["Bob", 9],
      ],
      totalRows: 2,
      note: undefined,
    });
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
