import { describe, expect, it } from "vitest";
import fc from "fast-check";
import { diffNotebooks, parseNotebook } from "../src/lib/nbdiff";

function notebook(cells: unknown[] = [], extra: Record<string, unknown> = {}): string {
  return JSON.stringify({ nbformat: 4, nbformat_minor: 4, metadata: {}, cells, ...extra });
}

function cell(source: string, extra: Record<string, unknown> = {}) {
  return { cell_type: "code", source, metadata: {}, outputs: [], execution_count: null, ...extra };
}

describe("notebook comparison integrity", () => {
  it.each(["bad JSON", "", "null", "{}", notebook([null]), notebook([cell("x", { source: [1] })])])(
    "rejects invalid notebook content instead of reporting an empty notebook: %s",
    (text) => expect(() => parseNotebook(text)).toThrow(),
  );

  it("does not turn two corrupt files into a successful no-change result", () => {
    expect(() => diffNotebooks("bad", "also bad")).toThrow();
  });

  it("detects cell metadata and execution changes with unchanged source", () => {
    const before = notebook([cell("x", { metadata: { tags: ["before"] } })]);
    const after = notebook([cell("x", { metadata: { tags: ["after"] }, execution_count: 2 })]);
    expect(diffNotebooks(before, after).cells[0].op).toBe("modified");
  });

  it("uses persistent cell IDs to align edited sources", () => {
    const before = notebook([cell("x = 1", { id: "stable" })], { nbformat_minor: 5 });
    const after = notebook([cell("x = 2", { id: "stable" })], { nbformat_minor: 5 });
    expect(diffNotebooks(before, after).cells.map(({ op }) => op)).toEqual(["modified"]);
  });

  it("compares metadata objects independently of JSON key order", () => {
    const before = notebook([], { metadata: { a: 1, b: 2 } });
    const after = notebook([], { metadata: { b: 2, a: 1 } });
    expect(diffNotebooks(before, after).metadataChanged).toBe(false);
  });

  it("treats only explicit absence as an empty comparison side", () => {
    const text = notebook([cell("added")]);
    expect([
      diffNotebooks(null, text).cells.map(({ op }) => op),
      diffNotebooks(text, null).cells.map(({ op }) => op),
      diffNotebooks(null, null).cells,
    ]).toEqual([["added"], ["removed"], []]);
  });

  it("normalizes string and string-array sources without false changes", () => {
    const before = notebook([cell("a\nb")]);
    const after = notebook([cell("", { source: ["a\n", "b"] })]);
    expect(diffNotebooks(before, after).cells[0].op).toBe("unchanged");
  });

  it.each([
    { cell_type: "markdown", attachments: { image: { "image/png": "YWJj" } } },
    { execution_count: 1 },
    { outputs: [{ output_type: "stream", name: "stdout", text: "hello" }] },
    { extension: { data: true } },
  ])("retains changes to cell details and outputs: %j", (extra) => {
    const before = notebook([cell("x", { id: "same" })], { nbformat_minor: 5 });
    const after = notebook([cell("x", { id: "same", ...extra })], { nbformat_minor: 5 });
    expect(diffNotebooks(before, after).cells[0].op).toBe("modified");
  });

  it.each([
    notebook([cell("x")], { nbformat_minor: 5 }),
    notebook([cell("x", { id: "repeat" }), cell("y", { id: "repeat" })], { nbformat_minor: 5 }),
    notebook([], { nbformat: 3 }),
    notebook([cell("x", { outputs: "bad" })]),
  ])("rejects invalid or unsupported structures: %s", (text) => {
    expect(() => diffNotebooks(text, null)).toThrow("Before notebook:");
  });

  it("preserves both full sequences with duplicates, edits and reordered cells", () => {
    fc.assert(fc.property(
      fc.array(fc.constantFrom("a", "b", "c"), { maxLength: 30 }),
      fc.array(fc.constantFrom("a", "b", "c"), { maxLength: 30 }),
      (before, after) => {
        const result = diffNotebooks(notebook(before.map((source) => cell(source))), notebook(after.map((source) => cell(source))));
        expect({
          before: result.cells.filter(({ op }) => op !== "added").map((entry) => (entry.oldCell ?? entry.cell).source),
          after: result.cells.filter(({ op }) => op !== "removed").map(({ cell }) => cell.source),
        }).toEqual({ before, after });
      },
    ), { seed: 1207, numRuns: 300 });
  });

  it("compares 40,000 repeated legacy cells without a quadratic matrix", () => {
    const text = notebook(Array.from({ length: 40_000 }, (_, index) => cell(String(index % 2))));
    const result = diffNotebooks(text, text);
    expect(result.cells.filter(({ op }) => op === "unchanged")).toHaveLength(40_000);
  });
});
