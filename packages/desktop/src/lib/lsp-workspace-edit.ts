/**
 * Utility for applying LSP WorkspaceEdits to Monaco models and disk.
 *
 * Handles both `changes` (uri → TextEdit[]) and `documentChanges`
 * (TextDocumentEdit[]) formats. Edits are applied to open Monaco models
 * and written to disk for files that are open in the editor.
 */

import * as monaco from "monaco-editor";
import type { LspWorkspaceEdit, LspTextEdit } from "./lsp-types";
import { lspToMonacoRange } from "./lsp-converters";

export interface WorkspaceEditResult {
  /** Number of files affected. */
  filesChanged: number;
  /** URIs of files that were modified. */
  modifiedUris: string[];
  /** Whether all edits were applied successfully. */
  success: boolean;
}

/**
 * Apply a workspace edit to Monaco models.
 *
 * For each file in the edit:
 * 1. If a Monaco model exists for the URI, apply edits to it.
 * 2. Track which files were modified for the caller to handle saving.
 *
 * Edits within a single file are applied in reverse order (bottom-up)
 * to preserve line/column offsets.
 */
export function applyWorkspaceEdit(edit: LspWorkspaceEdit): WorkspaceEditResult {
  const modifiedUris: string[] = [];

  // Normalize: prefer documentChanges over changes
  const editsByUri = new Map<string, LspTextEdit[]>();

  if (edit.documentChanges && edit.documentChanges.length > 0) {
    for (const docChange of edit.documentChanges) {
      const uri = docChange.textDocument.uri;
      const existing = editsByUri.get(uri) ?? [];
      existing.push(...docChange.edits);
      editsByUri.set(uri, existing);
    }
  } else if (edit.changes) {
    for (const [uri, edits] of Object.entries(edit.changes)) {
      editsByUri.set(uri, edits);
    }
  }

  for (const [uri, edits] of editsByUri) {
    if (edits.length === 0) continue;

    const model = findModelForUri(uri);
    if (!model) {
      // File not open in editor — skip (caller may handle out-of-editor edits)
      continue;
    }

    // Sort edits in reverse order (bottom-up) so earlier edits don't
    // shift the positions of later ones.
    const sortedEdits = [...edits].sort((a, b) => {
      const lineDiff = b.range.start.line - a.range.start.line;
      if (lineDiff !== 0) return lineDiff;
      return b.range.start.character - a.range.start.character;
    });

    // Apply all edits as a single undo-stop group
    const monacoEdits = sortedEdits.map((e) => ({
      range: lspToMonacoRange(e.range),
      text: e.newText,
    }));

    model.pushEditOperations(
      [],
      monacoEdits.map((e) => ({
        range: monaco.Range.lift(e.range),
        text: e.text,
        forceMoveMarkers: true,
      })),
      () => null,
    );

    modifiedUris.push(uri);
  }

  return {
    filesChanged: modifiedUris.length,
    modifiedUris,
    success: true,
  };
}

/**
 * Find a Monaco model matching the given URI string.
 * Handles both exact match and path-based matching.
 */
function findModelForUri(uri: string): monaco.editor.ITextModel | null {
  // Try exact URI match first
  const models = monaco.editor.getModels();
  for (const model of models) {
    if (model.uri.toString() === uri) return model;
  }

  // Try matching by file path (strip file:// prefix)
  const path = uri.replace(/^file:\/\//, "");
  for (const model of models) {
    const modelPath = model.uri.path || model.uri.toString();
    if (modelPath === path || modelPath.endsWith(path)) return model;
  }

  return null;
}
