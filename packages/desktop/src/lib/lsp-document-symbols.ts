/**
 * LSP Document Symbol provider.
 *
 * Sends textDocument/documentSymbol and converts the response to the
 * existing SymbolNode format used by the breadcrumb bar and outline view.
 *
 * When LSP symbols are available, they take priority over tree-sitter
 * symbols (more accurate, language-server-aware).
 */

import { LspConnection } from "./lsp-connection";
import type { SymbolNode, SymbolKind } from "./symbol-tree";

// ── LSP Document Symbol Types ───────────────────────────────────────

/** LSP SymbolKind enum values. */
export enum LspSymbolKind {
  File = 1,
  Module = 2,
  Namespace = 3,
  Package = 4,
  Class = 5,
  Method = 6,
  Property = 7,
  Field = 8,
  Constructor = 9,
  Enum = 10,
  Interface = 11,
  Function = 12,
  Variable = 13,
  Constant = 14,
  String = 15,
  Number = 16,
  Boolean = 17,
  Array = 18,
  Object = 19,
  Key = 20,
  Null = 21,
  EnumMember = 22,
  Struct = 23,
  Event = 24,
  Operator = 25,
  TypeParameter = 26,
}

/** LSP DocumentSymbol (hierarchical). */
interface LspDocumentSymbol {
  name: string;
  detail?: string;
  kind: LspSymbolKind;
  range: { start: { line: number; character: number }; end: { line: number; character: number } };
  selectionRange: { start: { line: number; character: number }; end: { line: number; character: number } };
  children?: LspDocumentSymbol[];
}

/** LSP SymbolInformation (flat, legacy). */
interface LspSymbolInformation {
  name: string;
  kind: LspSymbolKind;
  location: {
    uri: string;
    range: { start: { line: number; character: number }; end: { line: number; character: number } };
  };
  containerName?: string;
}

// ── Provider ────────────────────────────────────────────────────────

/**
 * Fetch document symbols from the LSP server and convert to SymbolNode[].
 *
 * Returns null if the request fails or the server doesn't support it.
 */
export async function fetchLspDocumentSymbols(
  connection: LspConnection,
  uri: string,
): Promise<SymbolNode[] | null> {
  try {
    const result = await connection.request(
      "textDocument/documentSymbol",
      { textDocument: { uri } },
      5000,
    ) as LspDocumentSymbol[] | LspSymbolInformation[] | null;

    if (!result || !Array.isArray(result) || result.length === 0) {
      return null;
    }

    // Detect response format: DocumentSymbol[] has `selectionRange`,
    // SymbolInformation[] has `location`.
    const first = result[0];
    if ("selectionRange" in first) {
      // Hierarchical DocumentSymbol[]
      return (result as LspDocumentSymbol[]).map(convertDocumentSymbol);
    }

    // Flat SymbolInformation[] — build a flat list
    return (result as LspSymbolInformation[]).map(convertSymbolInformation);
  } catch {
    return null;
  }
}

// ── Conversion ──────────────────────────────────────────────────────

function convertDocumentSymbol(sym: LspDocumentSymbol): SymbolNode {
  return {
    name: sym.name,
    kind: mapLspSymbolKind(sym.kind),
    range: {
      startLine: sym.range.start.line,
      endLine: sym.range.end.line,
    },
    children: (sym.children ?? []).map(convertDocumentSymbol),
  };
}

function convertSymbolInformation(sym: LspSymbolInformation): SymbolNode {
  return {
    name: sym.name,
    kind: mapLspSymbolKind(sym.kind),
    range: {
      startLine: sym.location.range.start.line,
      endLine: sym.location.range.end.line,
    },
    children: [],
  };
}

/**
 * Map LSP SymbolKind to our internal SymbolKind type.
 */
function mapLspSymbolKind(kind: LspSymbolKind): SymbolKind {
  switch (kind) {
    case LspSymbolKind.Function:
      return "function";
    case LspSymbolKind.Method:
    case LspSymbolKind.Constructor:
      return "method";
    case LspSymbolKind.Class:
      return "class";
    case LspSymbolKind.Interface:
      return "interface";
    case LspSymbolKind.Enum:
    case LspSymbolKind.EnumMember:
      return "enum";
    case LspSymbolKind.Variable:
    case LspSymbolKind.Field:
    case LspSymbolKind.Property:
      return "variable";
    case LspSymbolKind.Constant:
      return "constant";
    case LspSymbolKind.Module:
    case LspSymbolKind.Namespace:
    case LspSymbolKind.Package:
      return "module";
    case LspSymbolKind.Struct:
      return "struct";
    default:
      return "variable";
  }
}

// ── Integration with existing outline ───────────────────────────────

/**
 * Get document symbols, preferring LSP when available, falling back to
 * tree-sitter. This is the main entry point for the breadcrumb bar and
 * outline view.
 */
export async function getDocumentSymbols(
  connection: LspConnection | null,
  uri: string,
  treeSitterFallback: () => Promise<SymbolNode[]>,
): Promise<SymbolNode[]> {
  // Try LSP first
  if (connection) {
    const lspSymbols = await fetchLspDocumentSymbols(connection, uri);
    if (lspSymbols && lspSymbols.length > 0) {
      return lspSymbols;
    }
  }

  // Fall back to tree-sitter
  return treeSitterFallback();
}
