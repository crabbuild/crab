import { diffNotebooks, type CellDiff, type NotebookCell, type NotebookDiff } from "./nbdiff";

interface RenderCell {
  cell_type: string;
  source: string;
  outputs: string | null;
  details: string;
}

export interface RenderNotebookDiff extends Omit<NotebookDiff, "cells" | "beforeDetails" | "afterDetails"> {
  cells: Array<Omit<CellDiff, "cell" | "oldCell"> & { cell: RenderCell; oldCell?: RenderCell }>;
  beforeDetails: string;
  afterDetails: string;
}

export type NotebookDiffReply = { diff: RenderNotebookDiff } | { error: string };

self.onmessage = (event: MessageEvent<{ before: string | null; after: string | null }>) => {
  let reply: NotebookDiffReply;
  try {
    const diff = diffNotebooks(event.data.before, event.data.after);
    const renderCell = (cell: NotebookCell): RenderCell => ({
      cell_type: cell.cell_type,
      source: cell.source,
      outputs: cell.outputs.length ? JSON.stringify(cell.outputs) : null,
      details: JSON.stringify(cell.details),
    });
    // Serialize in the worker once. The UI receives bounded text and renders
    // pages; expanding rich output never parses or mounts notebook HTML/code.
    reply = { diff: {
      ...diff,
      beforeDetails: JSON.stringify(diff.beforeDetails),
      afterDetails: JSON.stringify(diff.afterDetails),
      cells: diff.cells.map((entry) => ({ ...entry, cell: renderCell(entry.cell), oldCell: entry.oldCell ? renderCell(entry.oldCell) : undefined })),
    } };
  } catch (error) {
    reply = { error: error instanceof Error ? error.message : String(error) };
  }
  self.postMessage(reply);
};
