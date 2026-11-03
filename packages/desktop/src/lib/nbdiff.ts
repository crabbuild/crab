// Cell-level diff engine for Jupyter notebooks (.ipynb).
//
// Parses both sides of a notebook as JSON and produces a cell-level
// patch using a simple LCS (Longest Common Subsequence) algorithm on
// cell source content. Notebooks typically have < 100 cells so the
// O(n*m) approach is fine.
//
// Pure functions, no side effects, no RPC calls. The renderer uses
// this to display structured notebook diffs with collapsible output
// sections.

// --- Types ---

/** A single cell parsed from a Jupyter notebook. */
export interface NotebookCell {
  cell_type: string;
  source: string[];
  outputs: unknown[];
  metadata: Record<string, unknown>;
}

/** The kind of change detected for a cell. */
export type CellDiffOp = "added" | "removed" | "modified" | "unchanged";

/** A single entry in the cell-level diff result. */
export interface CellDiff {
  op: CellDiffOp;
  /** Index in the new notebook for added/modified/unchanged cells,
   *  or index in the old notebook for removed cells. */
  index: number;
  /** The cell content (new side for added/modified/unchanged,
   *  old side for removed). */
  cell: NotebookCell;
  /** The old cell when op is "modified". */
  oldCell?: NotebookCell;
  /** Whether the source text differs between old and new. */
  sourceChanged: boolean;
  /** Whether the outputs differ between old and new. */
  outputsChanged: boolean;
}

/** Full diff result for a notebook comparison. */
export interface NotebookDiff {
  cells: CellDiff[];
  metadataChanged: boolean;
}

// --- Core function ---

/**
 * Diff two notebook JSON strings and produce a cell-level patch.
 *
 * Aligns cells using LCS on source content, then classifies each
 * cell as added, removed, modified, or unchanged.
 */
export function diffNotebooks(oldJson: string, newJson: string): NotebookDiff {
  const oldNb = parseNotebook(oldJson);
  const newNb = parseNotebook(newJson);

  const metadataChanged =
    JSON.stringify(oldNb.metadata) !== JSON.stringify(newNb.metadata);

  const oldTexts = oldNb.cells.map(cellSourceText);
  const newTexts = newNb.cells.map(cellSourceText);

  // Compute LCS alignment
  const lcs = computeLCS(oldTexts, newTexts);

  // Walk through the alignment to produce diff entries
  const cells: CellDiff[] = [];
  let oldIdx = 0;
  let newIdx = 0;

  for (const [lcsOldIdx, lcsNewIdx] of lcs) {
    // Emit removed cells before this match
    while (oldIdx < lcsOldIdx) {
      cells.push({
        op: "removed",
        index: oldIdx,
        cell: oldNb.cells[oldIdx],
        sourceChanged: true,
        outputsChanged: true,
      });
      oldIdx++;
    }

    // Emit added cells before this match
    while (newIdx < lcsNewIdx) {
      cells.push({
        op: "added",
        index: newIdx,
        cell: newNb.cells[newIdx],
        sourceChanged: true,
        outputsChanged: true,
      });
      newIdx++;
    }

    // Emit the matched pair
    const oldCell = oldNb.cells[oldIdx];
    const newCell = newNb.cells[newIdx];
    const sourceChanged = !cellsEqual(oldCell, newCell);
    const outputChanged = !outputsEqual(oldCell.outputs, newCell.outputs);

    if (sourceChanged || outputChanged) {
      cells.push({
        op: "modified",
        index: newIdx,
        cell: newCell,
        oldCell,
        sourceChanged,
        outputsChanged: outputChanged,
      });
    } else {
      cells.push({
        op: "unchanged",
        index: newIdx,
        cell: newCell,
        sourceChanged: false,
        outputsChanged: false,
      });
    }

    oldIdx++;
    newIdx++;
  }

  // Remaining removed cells after the last LCS match
  while (oldIdx < oldNb.cells.length) {
    cells.push({
      op: "removed",
      index: oldIdx,
      cell: oldNb.cells[oldIdx],
      sourceChanged: true,
      outputsChanged: true,
    });
    oldIdx++;
  }

  // Remaining added cells after the last LCS match
  while (newIdx < newNb.cells.length) {
    cells.push({
      op: "added",
      index: newIdx,
      cell: newNb.cells[newIdx],
      sourceChanged: true,
      outputsChanged: true,
    });
    newIdx++;
  }

  return { cells, metadataChanged };
}

// --- Helper functions ---

/**
 * Parse a JSON string as a Jupyter notebook and normalise cells.
 * Handles both nbformat 4 (cells at top level) and gracefully
 * falls back to an empty notebook on parse failure.
 */
export function parseNotebook(json: string): { cells: NotebookCell[]; metadata: Record<string, unknown> } {
  let raw: Record<string, unknown>;
  try {
    raw = JSON.parse(json) as Record<string, unknown>;
  } catch {
    return { cells: [], metadata: {} };
  }

  const metadata = (raw.metadata ?? {}) as Record<string, unknown>;
  const rawCells = Array.isArray(raw.cells) ? raw.cells : [];

  const cells: NotebookCell[] = rawCells.map((c: unknown) => {
    const cell = c as Record<string, unknown>;
    const source = normaliseSource(cell.source);
    const outputs = Array.isArray(cell.outputs) ? cell.outputs : [];
    const cellMeta = (cell.metadata ?? {}) as Record<string, unknown>;
    const cellType = typeof cell.cell_type === "string" ? cell.cell_type : "raw";

    return {
      cell_type: cellType,
      source,
      outputs: outputs as unknown[],
      metadata: cellMeta,
    };
  });

  return { cells, metadata };
}

/** Join a cell's source lines into a single string for comparison. */
export function cellSourceText(cell: NotebookCell): string {
  return cell.source.join("");
}

/** Deep equality check on cell source content. */
export function cellsEqual(a: NotebookCell, b: NotebookCell): boolean {
  return cellSourceText(a) === cellSourceText(b);
}

/** Compare outputs using JSON serialisation (sufficient for v1). */
export function outputsEqual(a: unknown[], b: unknown[]): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

// --- LCS implementation ---

/**
 * Compute the Longest Common Subsequence of two string arrays.
 * Returns an array of [oldIndex, newIndex] pairs representing
 * matched positions.
 *
 * Uses the standard O(n*m) dynamic programming approach. Fine for
 * notebooks which typically have < 100 cells.
 */
function computeLCS(oldTexts: string[], newTexts: string[]): Array<[number, number]> {
  const n = oldTexts.length;
  const m = newTexts.length;

  // Build the DP table
  const dp: number[][] = Array.from({ length: n + 1 }, () =>
    Array.from({ length: m + 1 }, () => 0),
  );

  for (let i = 1; i <= n; i++) {
    for (let j = 1; j <= m; j++) {
      if (oldTexts[i - 1] === newTexts[j - 1]) {
        dp[i][j] = dp[i - 1][j - 1] + 1;
      } else {
        dp[i][j] = Math.max(dp[i - 1][j], dp[i][j - 1]);
      }
    }
  }

  // Backtrack to find the actual subsequence indices
  const result: Array<[number, number]> = [];
  let i = n;
  let j = m;

  while (i > 0 && j > 0) {
    if (oldTexts[i - 1] === newTexts[j - 1]) {
      result.push([i - 1, j - 1]);
      i--;
      j--;
    } else if (dp[i - 1][j] > dp[i][j - 1]) {
      i--;
    } else {
      j--;
    }
  }

  result.reverse();
  return result;
}

// --- Internal helpers ---

/** Normalise the `source` field which can be a string or string[]. */
function normaliseSource(raw: unknown): string[] {
  if (Array.isArray(raw)) {
    return raw.map((line) => (typeof line === "string" ? line : String(line)));
  }
  if (typeof raw === "string") {
    return raw.split("\n").map((line, i, arr) =>
      i < arr.length - 1 ? line + "\n" : line,
    );
  }
  return [];
}
