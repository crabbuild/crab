/**
 * Map LSP SymbolKind numeric codes to the renderer's `SymbolKind` type.
 *
 * The LSP protocol defines 26 symbol kinds
 * (https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#symbolKind)
 * but our internal outline / icon system uses a smaller set.
 * This module is the single place that knows how to bridge the two.
 */

import type { SymbolKind } from "./symbol-tree";

// LSP SymbolKind values (see LSP spec, 1-indexed).
export const LSP_SYMBOL_KIND = {
  File: 1,
  Module: 2,
  Namespace: 3,
  Package: 4,
  Class: 5,
  Method: 6,
  Property: 7,
  Field: 8,
  Constructor: 9,
  Enum: 10,
  Interface: 11,
  Function: 12,
  Variable: 13,
  Constant: 14,
  String: 15,
  Number: 16,
  Boolean: 17,
  Array: 18,
  Object: 19,
  Key: 20,
  Null: 21,
  EnumMember: 22,
  Struct: 23,
  Event: 24,
  Operator: 25,
  TypeParameter: 26,
} as const;

/** Map an LSP SymbolKind code to the app's internal `SymbolKind`. */
export function lspSymbolKindToSymbolKind(kind: number): SymbolKind {
  switch (kind) {
    case LSP_SYMBOL_KIND.Function:
      return "function";
    case LSP_SYMBOL_KIND.Method:
    case LSP_SYMBOL_KIND.Constructor:
      return "method";
    case LSP_SYMBOL_KIND.Class:
      return "class";
    case LSP_SYMBOL_KIND.Interface:
      return "interface";
    case LSP_SYMBOL_KIND.Enum:
    case LSP_SYMBOL_KIND.EnumMember:
      return "enum";
    case LSP_SYMBOL_KIND.Variable:
    case LSP_SYMBOL_KIND.Field:
    case LSP_SYMBOL_KIND.Property:
      return "variable";
    case LSP_SYMBOL_KIND.Constant:
      return "constant";
    case LSP_SYMBOL_KIND.Module:
    case LSP_SYMBOL_KIND.Namespace:
    case LSP_SYMBOL_KIND.Package:
      return "module";
    case LSP_SYMBOL_KIND.Struct:
      return "struct";
    default:
      return "variable";
  }
}
