import { describe, it, expect } from "vitest";
import {
  diffNotebooks,
  parseNotebook,
  cellSourceText,
  cellsEqual,
  outputsEqual,
  type NotebookCell,
} from "./nbdiff";

// --- Helper to build a minimal notebook JSON ---

function makeNotebook(
  cells: Array<{ type?: string; source: string[]; outputs?: unknown[] }>,
  metadata?: Record<string, unknown>,
): string {
  return JSON.stringify({
    nbformat: 4,
    nbformat_minor: 5,
    metadata: metadata ?? { kernelspec: { name: "python3" } },
    cells: cells.map((c) => ({
      cell_type: c.type ?? "code",
      source: c.source,
      outputs: c.outputs ?? [],
      metadata: {},
    })),
  });
}

describe("parseNotebook", () => {
  it("parses a valid notebook", () => {
    const json = makeNotebook([
      { source: ["print('hello')\n"] },
      { type: "markdown", source: ["# Title\n"] },
    ]);
    const nb = parseNotebook(json);
    expect(nb.cells).toHaveLength(2);
    expect(nb.cells[0].cell_type).toBe("code");
    expect(nb.cells[0].source).toEqual(["print('hello')\n"]);
    expect(nb.cells[1].cell_type).toBe("markdown");
  });

  it("returns empty cells on invalid JSON", () => {
    const nb = parseNotebook("not json at all");
    expect(nb.cells).toEqual([]);
    expect(nb.metadata).toEqual({});
  });

  it("handles source as a single string", () => {
    const json = JSON.stringify({
      cells: [{ cell_type: "code", source: "x = 1\ny = 2", outputs: [], metadata: {} }],
      metadata: {},
    });
    const nb = parseNotebook(json);
    expect(nb.cells[0].source).toEqual(["x = 1\n", "y = 2"]);
  });

  it("handles missing outputs gracefully", () => {
    const json = JSON.stringify({
      cells: [{ cell_type: "code", source: ["a = 1\n"], metadata: {} }],
      metadata: {},
    });
    const nb = parseNotebook(json);
    expect(nb.cells[0].outputs).toEqual([]);
  });
});

describe("cellSourceText", () => {
  it("joins source lines", () => {
    const cell: NotebookCell = {
      cell_type: "code",
      source: ["line1\n", "line2\n"],
      outputs: [],
      metadata: {},
    };
    expect(cellSourceText(cell)).toBe("line1\nline2\n");
  });
});

describe("cellsEqual", () => {
  it("returns true for identical source", () => {
    const a: NotebookCell = { cell_type: "code", source: ["x = 1\n"], outputs: [], metadata: {} };
    const b: NotebookCell = { cell_type: "code", source: ["x = 1\n"], outputs: [], metadata: {} };
    expect(cellsEqual(a, b)).toBe(true);
  });

  it("returns false for different source", () => {
    const a: NotebookCell = { cell_type: "code", source: ["x = 1\n"], outputs: [], metadata: {} };
    const b: NotebookCell = { cell_type: "code", source: ["x = 2\n"], outputs: [], metadata: {} };
    expect(cellsEqual(a, b)).toBe(false);
  });
});

describe("outputsEqual", () => {
  it("returns true for identical outputs", () => {
    const out = [{ output_type: "stream", text: ["hello\n"] }];
    expect(outputsEqual(out, [...out])).toBe(true);
  });

  it("returns false for different outputs", () => {
    const a = [{ output_type: "stream", text: ["hello\n"] }];
    const b = [{ output_type: "stream", text: ["world\n"] }];
    expect(outputsEqual(a, b)).toBe(false);
  });

  it("returns true for both empty", () => {
    expect(outputsEqual([], [])).toBe(true);
  });
});

describe("diffNotebooks", () => {
  it("detects no changes for identical notebooks", () => {
    const json = makeNotebook([
      { source: ["x = 1\n"] },
      { source: ["y = 2\n"] },
    ]);
    const diff = diffNotebooks(json, json);
    expect(diff.metadataChanged).toBe(false);
    expect(diff.cells).toHaveLength(2);
    expect(diff.cells.every((c) => c.op === "unchanged")).toBe(true);
  });

  it("detects added cells", () => {
    const oldJson = makeNotebook([{ source: ["x = 1\n"] }]);
    const newJson = makeNotebook([
      { source: ["x = 1\n"] },
      { source: ["y = 2\n"] },
    ]);
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.cells).toHaveLength(2);
    expect(diff.cells[0].op).toBe("unchanged");
    expect(diff.cells[1].op).toBe("added");
    expect(diff.cells[1].index).toBe(1);
  });

  it("detects removed cells", () => {
    const oldJson = makeNotebook([
      { source: ["x = 1\n"] },
      { source: ["y = 2\n"] },
    ]);
    const newJson = makeNotebook([{ source: ["x = 1\n"] }]);
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.cells).toHaveLength(2);
    expect(diff.cells[0].op).toBe("unchanged");
    expect(diff.cells[1].op).toBe("removed");
  });

  it("treats completely rewritten cells as removed + added", () => {
    // LCS aligns on source text — when source is entirely different,
    // the cell can't be matched and appears as removed + added.
    const oldJson = makeNotebook([{ source: ["x = 1\n"] }]);
    const newJson = makeNotebook([{ source: ["x = 99\n"] }]);
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.cells).toHaveLength(2);
    expect(diff.cells[0].op).toBe("removed");
    expect(diff.cells[1].op).toBe("added");
  });

  it("detects modified cells (outputs changed only)", () => {
    const oldJson = makeNotebook([
      { source: ["x = 1\n"], outputs: [{ output_type: "stream", text: ["1\n"] }] },
    ]);
    const newJson = makeNotebook([
      { source: ["x = 1\n"], outputs: [{ output_type: "stream", text: ["2\n"] }] },
    ]);
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.cells).toHaveLength(1);
    expect(diff.cells[0].op).toBe("modified");
    expect(diff.cells[0].sourceChanged).toBe(false);
    expect(diff.cells[0].outputsChanged).toBe(true);
  });

  it("detects metadata changes", () => {
    const oldJson = makeNotebook([], { kernelspec: { name: "python3" } });
    const newJson = makeNotebook([], { kernelspec: { name: "python2" } });
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.metadataChanged).toBe(true);
  });

  it("handles reordered cells via LCS", () => {
    const oldJson = makeNotebook([
      { source: ["a\n"] },
      { source: ["b\n"] },
      { source: ["c\n"] },
    ]);
    // Remove middle cell
    const newJson = makeNotebook([
      { source: ["a\n"] },
      { source: ["c\n"] },
    ]);
    const diff = diffNotebooks(oldJson, newJson);
    const ops = diff.cells.map((c) => c.op);
    expect(ops).toContain("unchanged");
    expect(ops).toContain("removed");
    // "a" and "c" should be unchanged, "b" removed
    expect(diff.cells.filter((c) => c.op === "unchanged")).toHaveLength(2);
    expect(diff.cells.filter((c) => c.op === "removed")).toHaveLength(1);
  });

  it("handles empty notebooks", () => {
    const oldJson = makeNotebook([]);
    const newJson = makeNotebook([]);
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.cells).toHaveLength(0);
    expect(diff.metadataChanged).toBe(false);
  });

  it("handles old empty, new with cells", () => {
    const oldJson = makeNotebook([]);
    const newJson = makeNotebook([{ source: ["x = 1\n"] }]);
    const diff = diffNotebooks(oldJson, newJson);
    expect(diff.cells).toHaveLength(1);
    expect(diff.cells[0].op).toBe("added");
  });

  it("handles invalid JSON gracefully", () => {
    const diff = diffNotebooks("bad", "also bad");
    expect(diff.cells).toHaveLength(0);
    expect(diff.metadataChanged).toBe(false);
  });
});
