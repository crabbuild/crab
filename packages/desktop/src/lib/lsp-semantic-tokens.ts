/**
 * Semantic tokens provider for Monaco, backed by LSP.
 *
 * Registers a DocumentSemanticTokensProvider that overlays semantic
 * token classifications on top of Shiki syntax highlighting (additive).
 *
 * LSP semantic token types are mapped to Monaco token classifications.
 */

import * as monaco from "monaco-editor";
import { LspConnection } from "./lsp-connection";

// ── LSP Semantic Token Types ────────────────────────────────────────

/**
 * Standard LSP semantic token types (order matters — index is the type ID).
 */
export const LSP_TOKEN_TYPES = [
  "namespace",
  "type",
  "class",
  "enum",
  "interface",
  "struct",
  "typeParameter",
  "parameter",
  "variable",
  "property",
  "enumMember",
  "event",
  "function",
  "method",
  "macro",
  "keyword",
  "modifier",
  "comment",
  "string",
  "number",
  "regexp",
  "operator",
  "decorator",
] as const;

/**
 * Standard LSP semantic token modifiers (bit flags).
 */
export const LSP_TOKEN_MODIFIERS = [
  "declaration",
  "definition",
  "readonly",
  "static",
  "deprecated",
  "abstract",
  "async",
  "modification",
  "documentation",
  "defaultLibrary",
] as const;

// ── Monaco Token Type Mapping ───────────────────────────────────────

/**
 * Map LSP semantic token type indices to Monaco token type strings.
 * Monaco uses these for theming via `editor.tokenColorCustomizations`.
 */
const TOKEN_TYPE_MAP: Record<number, string> = {
  0: "namespace",
  1: "type",
  2: "class",
  3: "enum",
  4: "interface",
  5: "struct",
  6: "typeParameter",
  7: "parameter",
  8: "variable",
  9: "property",
  10: "enumMember",
  11: "event",
  12: "function",
  13: "method",
  14: "macro",
  15: "keyword",
  16: "modifier",
  17: "comment",
  18: "string",
  19: "number",
  20: "regexp",
  21: "operator",
  22: "decorator",
};

// ── Provider ────────────────────────────────────────────────────────

/**
 * Creates and registers a Monaco DocumentSemanticTokensProvider backed by LSP.
 *
 * Returns a disposable that unregisters the provider.
 */
export function registerLspSemanticTokensProvider(
  language: string,
  connection: LspConnection,
): monaco.IDisposable {
  // Register the token legend with Monaco
  const legend: monaco.languages.SemanticTokensLegend = {
    tokenTypes: [...LSP_TOKEN_TYPES],
    tokenModifiers: [...LSP_TOKEN_MODIFIERS],
  };

  const provider: monaco.languages.DocumentSemanticTokensProvider = {
    getLegend(): monaco.languages.SemanticTokensLegend {
      return legend;
    },

    provideDocumentSemanticTokens: async (
      model: monaco.editor.ITextModel,
      _lastResultId: string | null,
      _token: monaco.CancellationToken,
    ): Promise<monaco.languages.SemanticTokens | null> => {
      const uri = model.uri.toString();

      try {
        const result = await connection.request(
          "textDocument/semanticTokens/full",
          { textDocument: { uri } },
          10000,
        ) as SemanticTokensResult | null;

        if (!result || !result.data || result.data.length === 0) {
          return null;
        }

        return {
          resultId: result.resultId,
          data: new Uint32Array(result.data),
        };
      } catch {
        return null;
      }
    },

    releaseDocumentSemanticTokens(_resultId: string | undefined): void {
      // Nothing to release
    },
  };

  return monaco.languages.registerDocumentSemanticTokensProvider(language, provider);
}

// ── Types ───────────────────────────────────────────────────────────

interface SemanticTokensResult {
  resultId?: string;
  data: number[];
}

// ── Utility ─────────────────────────────────────────────────────────

/**
 * Check if server capabilities include semantic tokens support.
 */
export function supportsSemanticTokens(capabilities: Record<string, unknown>): boolean {
  const provider = capabilities.semanticTokensProvider;
  if (!provider) return false;
  if (typeof provider === "object" && provider !== null) {
    // Check for full support
    const full = (provider as { full?: unknown }).full;
    return full === true || (typeof full === "object" && full !== null);
  }
  return false;
}

/**
 * Decode the delta-encoded semantic tokens data into absolute positions.
 * Useful for debugging and testing.
 */
export function decodeSemanticTokens(
  data: number[],
): Array<{
  line: number;
  startChar: number;
  length: number;
  tokenType: string;
  tokenModifiers: string[];
}> {
  const tokens: Array<{
    line: number;
    startChar: number;
    length: number;
    tokenType: string;
    tokenModifiers: string[];
  }> = [];

  let currentLine = 0;
  let currentChar = 0;

  for (let i = 0; i < data.length; i += 5) {
    const deltaLine = data[i];
    const deltaStartChar = data[i + 1];
    const length = data[i + 2];
    const tokenTypeIdx = data[i + 3];
    const tokenModifierBits = data[i + 4];

    if (deltaLine > 0) {
      currentLine += deltaLine;
      currentChar = deltaStartChar;
    } else {
      currentChar += deltaStartChar;
    }

    const tokenType = TOKEN_TYPE_MAP[tokenTypeIdx] ?? `unknown(${tokenTypeIdx})`;
    const tokenModifiers: string[] = [];
    for (let bit = 0; bit < LSP_TOKEN_MODIFIERS.length; bit++) {
      if (tokenModifierBits & (1 << bit)) {
        tokenModifiers.push(LSP_TOKEN_MODIFIERS[bit]);
      }
    }

    tokens.push({
      line: currentLine,
      startChar: currentChar,
      length,
      tokenType,
      tokenModifiers,
    });
  }

  return tokens;
}
