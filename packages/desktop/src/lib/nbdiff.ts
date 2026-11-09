/** Structured nbformat 4 comparisons. Only null represents an absent file. */
export interface NotebookCell {
  id?: string;
  cell_type: string;
  source: string;
  outputs: unknown[];
  /** Preserve metadata, execution counts, attachments and extension fields. */
  details: Record<string, unknown>;
}

export type CellDiffOp = "added" | "removed" | "modified" | "unchanged";

export interface CellDiff {
  op: CellDiffOp;
  index: number;
  cell: NotebookCell;
  oldCell?: NotebookCell;
  sourceChanged: boolean;
  outputsChanged: boolean;
  detailsChanged: boolean;
}

export interface NotebookDiff {
  cells: CellDiff[];
  metadataChanged: boolean;
  beforeDetails: Record<string, unknown>;
  afterDetails: Record<string, unknown>;
}

interface Notebook {
  cells: NotebookCell[];
  details: Record<string, unknown>;
}

/** Compare complete documents; malformed content never means an empty side. */
export function diffNotebooks(before: string | null, after: string | null): NotebookDiff {
  const oldNb = readSide(before, "Before");
  const newNb = readSide(after, "After");
  if (oldNb.cells.length + newNb.cells.length > 100_000) {
    throw new Error("Structured comparison exceeds 100,000 combined cells. Open the files to inspect their complete content.");
  }
  const useIds = [oldNb, newNb].every((nb) => nb.cells.every((cell) => cell.id !== undefined));
  const key = (cell: NotebookCell) => useIds ? `id:${cell.id}` : `${cell.cell_type}:${cell.source}`;
  const matches = align(oldNb.cells.map(key), newNb.cells.map(key));
  const cells: CellDiff[] = [];
  let oldIndex = 0;
  let newIndex = 0;
  const removed = () => {
    const cell = oldNb.cells[oldIndex];
    cells.push({ op: "removed", index: oldIndex++, cell, sourceChanged: true, outputsChanged: cell.outputs.length > 0, detailsChanged: true });
  };
  const added = () => {
    const cell = newNb.cells[newIndex];
    cells.push({ op: "added", index: newIndex++, cell, sourceChanged: true, outputsChanged: cell.outputs.length > 0, detailsChanged: true });
  };
  for (const [oldMatch, newMatch] of matches) {
    while (oldIndex < oldMatch) removed();
    while (newIndex < newMatch) added();
    const oldCell = oldNb.cells[oldIndex++];
    const cell = newNb.cells[newIndex];
    const sourceChanged = oldCell.source !== cell.source;
    const outputsChanged = !jsonEqual(oldCell.outputs, cell.outputs);
    const detailsChanged = !jsonEqual(oldCell.details, cell.details);
    const changed = sourceChanged || outputsChanged || detailsChanged;
    cells.push({ op: changed ? "modified" : "unchanged", index: newIndex++, cell, oldCell: changed ? oldCell : undefined, sourceChanged, outputsChanged, detailsChanged });
  }
  while (oldIndex < oldNb.cells.length) removed();
  while (newIndex < newNb.cells.length) added();
  return {
    cells,
    metadataChanged: !jsonEqual(oldNb.details, newNb.details),
    beforeDetails: oldNb.details,
    afterDetails: newNb.details,
  };
}

function readSide(text: string | null, label: string): Notebook {
  if (text === null) return { cells: [], details: {} };
  try {
    return parseNotebook(text);
  } catch (error) {
    throw new Error(`${label} notebook: ${error instanceof Error ? error.message : String(error)}`);
  }
}

/** Read nbformat 4 without coercing malformed fields or discarding extensions. */
export function parseNotebook(text: string): Notebook {
  const raw: unknown = JSON.parse(text);
  if (!isRecord(raw) || raw.nbformat !== 4 || !Number.isInteger(raw.nbformat_minor) || (raw.nbformat_minor as number) < 0) {
    throw new Error("Expected an nbformat 4 notebook with a nonnegative minor version.");
  }
  if (!isRecord(raw.metadata) || !Array.isArray(raw.cells)) {
    throw new Error("Notebook metadata must be an object and cells must be an array.");
  }
  if (raw.cells.length > 100_000) throw new Error("Notebook exceeds 100,000 cells.");
  const ids = new Set<string>();
  const cells = raw.cells.map((value: unknown, index: number): NotebookCell => {
    if (!isRecord(value) || typeof value.cell_type !== "string" || !isRecord(value.metadata)) {
      throw new Error(`Cell ${index + 1} requires a cell type and metadata object.`);
    }
    if ((raw.nbformat_minor as number) >= 5 || value.id !== undefined) {
      if (typeof value.id !== "string" || !/^[a-zA-Z0-9_-]{1,64}$/.test(value.id) || ids.has(value.id)) {
        throw new Error(`Cell ${index + 1} requires a unique valid cell ID.`);
      }
      ids.add(value.id);
    }
    const { source, outputs, ...details } = value;
    const known = ["code", "markdown", "raw"].includes(value.cell_type);
    let text = "";
    if (typeof source === "string") text = source;
    else if (Array.isArray(source) && source.every((line: unknown) => typeof line === "string")) text = source.join("");
    else if (known || source !== undefined) throw new Error(`Cell ${index + 1} source must be a string or an array of strings.`);
    if ((value.cell_type === "code" || outputs !== undefined) && !Array.isArray(outputs)) {
      throw new Error(`Cell ${index + 1} outputs must be an array.`);
    }
    if (value.cell_type === "code" && value.execution_count !== null && (!Number.isInteger(value.execution_count) || (value.execution_count as number) < 0)) {
      throw new Error(`Cell ${index + 1} execution count must be a nonnegative integer or null.`);
    }
    return { id: value.id as string | undefined, cell_type: value.cell_type, source: text, outputs: Array.isArray(outputs) ? outputs : [], details };
  });
  const { cells: _cells, ...details } = raw;
  return { cells, details };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

// Object key order is not a notebook change. An iterative comparison also
// avoids blowing the worker's call stack on deeply nested output/metadata.
function jsonEqual(left: unknown, right: unknown): boolean {
  const pending: Array<[unknown, unknown]> = [[left, right]];
  while (pending.length) {
    const [a, b] = pending.pop()!;
    if (a === b) continue;
    if (a === null || b === null || typeof a !== "object" || typeof b !== "object" || Array.isArray(a) !== Array.isArray(b)) return false;
    const aKeys = Object.keys(a);
    const bKeys = Object.keys(b);
    if (aKeys.length !== bKeys.length) return false;
    for (const key of aKeys) {
      if (!Object.hasOwn(b, key)) return false;
      pending.push([(a as Record<string, unknown>)[key], (b as Record<string, unknown>)[key]]);
    }
  }
  return true;
}

// Match each key's occurrences in order, then choose an increasing subsequence.
// This deterministic alignment preserves both complete sequences in O(n log n)
// work/linear memory, including repeated legacy sources; it need not be a
// minimal edit script. IDs keep edits aligned in nbformat 4.5 and newer.
function align(before: string[], after: string[]): Array<[number, number]> {
  const positions = new Map<string, { indices: number[]; next: number }>();
  after.forEach((key, index) => {
    let entry = positions.get(key);
    if (!entry) { entry = { indices: [], next: 0 }; positions.set(key, entry); }
    entry.indices.push(index);
  });
  const candidates: Array<[number, number]> = [];
  before.forEach((key, index) => {
    const entry = positions.get(key);
    if (entry && entry.next < entry.indices.length) candidates.push([index, entry.indices[entry.next++]]);
  });
  const tails: number[] = [];
  const previous = new Int32Array(candidates.length).fill(-1);
  candidates.forEach((pair, index) => {
    let low = 0;
    let high = tails.length;
    while (low < high) {
      const mid = (low + high) >>> 1;
      if (candidates[tails[mid]][1] < pair[1]) low = mid + 1;
      else high = mid;
    }
    if (low > 0) previous[index] = tails[low - 1];
    tails[low] = index;
  });
  const result: Array<[number, number]> = [];
  for (let index = tails.at(-1) ?? -1; index >= 0; index = previous[index]) result.push(candidates[index]);
  return result.reverse();
}
