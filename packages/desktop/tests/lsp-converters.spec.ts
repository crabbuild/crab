/**
 * Unit tests for LSP ↔ Monaco type converters.
 *
 * Covers position indexing (0-indexed ↔ 1-indexed), severity mapping,
 * completion kind mapping, diagnostic tag mapping, and markup content rendering.
 */

import { describe, it, expect } from "vitest";
import {
  lspToMonacoPosition,
  monacoToLspPosition,
  lspToMonacoRange,
  monacoToLspRange,
  lspSeverityToMonaco,
  lspTagsToMonaco,
  lspCompletionKindToMonaco,
  lspInsertTextFormatToMonaco,
  lspMarkupToMonaco,
  lspMarkedStringToMonaco,
  lspHoverToMonacoContents,
  lspDiagnosticToMonacoMarker,
  lspCompletionItemToMonaco,
  lspTextEditToMonaco,
} from "../src/lib/lsp-converters";
import {
  LspDiagnosticSeverity,
  LspDiagnosticTag,
  LspCompletionItemKind,
  LspInsertTextFormat,
} from "../src/lib/lsp-types";

// ── Position Conversion ─────────────────────────────────────────────

describe("position conversion", () => {
  it("converts LSP 0-indexed position to Monaco 1-indexed", () => {
    expect(lspToMonacoPosition({ line: 0, character: 0 })).toEqual({
      lineNumber: 1,
      column: 1,
    });
    expect(lspToMonacoPosition({ line: 9, character: 4 })).toEqual({
      lineNumber: 10,
      column: 5,
    });
  });

  it("converts Monaco 1-indexed position to LSP 0-indexed", () => {
    expect(monacoToLspPosition({ lineNumber: 1, column: 1 })).toEqual({
      line: 0,
      character: 0,
    });
    expect(monacoToLspPosition({ lineNumber: 10, column: 5 })).toEqual({
      line: 9,
      character: 4,
    });
  });

  it("round-trips position conversion", () => {
    const lspPos = { line: 42, character: 17 };
    const monacoPos = lspToMonacoPosition(lspPos);
    const backToLsp = monacoToLspPosition(monacoPos);
    expect(backToLsp).toEqual(lspPos);
  });

  it("handles zero position correctly", () => {
    const monacoPos = { lineNumber: 1, column: 1 };
    const lspPos = monacoToLspPosition(monacoPos);
    expect(lspPos).toEqual({ line: 0, character: 0 });
    expect(lspToMonacoPosition(lspPos)).toEqual(monacoPos);
  });
});

// ── Range Conversion ────────────────────────────────────────────────

describe("range conversion", () => {
  it("converts LSP range to Monaco range", () => {
    const lspRange = {
      start: { line: 0, character: 5 },
      end: { line: 2, character: 10 },
    };
    expect(lspToMonacoRange(lspRange)).toEqual({
      startLineNumber: 1,
      startColumn: 6,
      endLineNumber: 3,
      endColumn: 11,
    });
  });

  it("converts Monaco range to LSP range", () => {
    const monacoRange = {
      startLineNumber: 1,
      startColumn: 6,
      endLineNumber: 3,
      endColumn: 11,
    };
    expect(monacoToLspRange(monacoRange)).toEqual({
      start: { line: 0, character: 5 },
      end: { line: 2, character: 10 },
    });
  });

  it("round-trips range conversion", () => {
    const lspRange = {
      start: { line: 10, character: 0 },
      end: { line: 10, character: 20 },
    };
    const monacoRange = lspToMonacoRange(lspRange);
    const backToLsp = monacoToLspRange(monacoRange);
    expect(backToLsp).toEqual(lspRange);
  });
});

// ── Severity Mapping ────────────────────────────────────────────────

describe("severity mapping", () => {
  it("maps LSP Error to Monaco Error (8)", () => {
    expect(lspSeverityToMonaco(LspDiagnosticSeverity.Error)).toBe(8);
  });

  it("maps LSP Warning to Monaco Warning (4)", () => {
    expect(lspSeverityToMonaco(LspDiagnosticSeverity.Warning)).toBe(4);
  });

  it("maps LSP Information to Monaco Info (2)", () => {
    expect(lspSeverityToMonaco(LspDiagnosticSeverity.Information)).toBe(2);
  });

  it("maps LSP Hint to Monaco Hint (1)", () => {
    expect(lspSeverityToMonaco(LspDiagnosticSeverity.Hint)).toBe(1);
  });

  it("defaults to Info for undefined severity", () => {
    expect(lspSeverityToMonaco(undefined)).toBe(2);
  });
});

// ── Diagnostic Tag Mapping ──────────────────────────────────────────

describe("diagnostic tag mapping", () => {
  it("maps Unnecessary tag to Monaco tag 1", () => {
    expect(lspTagsToMonaco([LspDiagnosticTag.Unnecessary])).toEqual([1]);
  });

  it("maps Deprecated tag to Monaco tag 2", () => {
    expect(lspTagsToMonaco([LspDiagnosticTag.Deprecated])).toEqual([2]);
  });

  it("maps both tags", () => {
    expect(lspTagsToMonaco([LspDiagnosticTag.Unnecessary, LspDiagnosticTag.Deprecated])).toEqual([1, 2]);
  });

  it("returns empty array for undefined tags", () => {
    expect(lspTagsToMonaco(undefined)).toEqual([]);
  });

  it("returns empty array for empty tags", () => {
    expect(lspTagsToMonaco([])).toEqual([]);
  });
});

// ── Completion Kind Mapping ─────────────────────────────────────────

describe("completion kind mapping", () => {
  it("maps Function to Monaco Function (1)", () => {
    expect(lspCompletionKindToMonaco(LspCompletionItemKind.Function)).toBe(1);
  });

  it("maps Class to Monaco Class (5)", () => {
    expect(lspCompletionKindToMonaco(LspCompletionItemKind.Class)).toBe(5);
  });

  it("maps Snippet to Monaco Snippet (27)", () => {
    expect(lspCompletionKindToMonaco(LspCompletionItemKind.Snippet)).toBe(27);
  });

  it("maps Variable to Monaco Variable (4)", () => {
    expect(lspCompletionKindToMonaco(LspCompletionItemKind.Variable)).toBe(4);
  });

  it("defaults to Text (18) for undefined kind", () => {
    expect(lspCompletionKindToMonaco(undefined)).toBe(18);
  });
});

// ── Insert Text Format ──────────────────────────────────────────────

describe("insert text format mapping", () => {
  it("maps Snippet format to insertTextRules 4", () => {
    expect(lspInsertTextFormatToMonaco(LspInsertTextFormat.Snippet)).toBe(4);
  });

  it("maps PlainText format to 0", () => {
    expect(lspInsertTextFormatToMonaco(LspInsertTextFormat.PlainText)).toBe(0);
  });

  it("returns 0 for undefined format", () => {
    expect(lspInsertTextFormatToMonaco(undefined)).toBe(0);
  });
});

// ── Markup Content Rendering ────────────────────────────────────────

describe("markup content rendering", () => {
  it("renders markdown content as-is with isTrusted", () => {
    const result = lspMarkupToMonaco({ kind: "markdown", value: "**bold**" });
    expect(result).toEqual({ value: "**bold**", isTrusted: true });
  });

  it("wraps plaintext in code fence", () => {
    const result = lspMarkupToMonaco({ kind: "plaintext", value: "fn main()" });
    expect(result.value).toBe("```\nfn main()\n```");
  });

  it("converts MarkedString with language to fenced code block", () => {
    const result = lspMarkedStringToMonaco({ language: "rust", value: "fn foo() -> i32" });
    expect(result.value).toBe("```rust\nfn foo() -> i32\n```");
  });

  it("converts plain string MarkedString directly", () => {
    const result = lspMarkedStringToMonaco("hello world");
    expect(result).toEqual({ value: "hello world" });
  });
});

// ── Hover Content Conversion ────────────────────────────────────────

describe("hover content conversion", () => {
  it("converts MarkupContent hover", () => {
    const hover = { contents: { kind: "markdown" as const, value: "# Title" } };
    const result = lspHoverToMonacoContents(hover);
    expect(result).toEqual([{ value: "# Title", isTrusted: true }]);
  });

  it("converts string hover", () => {
    const hover = { contents: "simple text" };
    const result = lspHoverToMonacoContents(hover);
    expect(result).toEqual([{ value: "simple text" }]);
  });

  it("converts array of MarkedString hover", () => {
    const hover = {
      contents: [
        { language: "typescript", value: "const x: number" },
        "A numeric variable",
      ],
    };
    const result = lspHoverToMonacoContents(hover);
    expect(result.length).toBe(2);
    expect(result[0].value).toBe("```typescript\nconst x: number\n```");
    expect(result[1].value).toBe("A numeric variable");
  });

  it("returns empty array for empty string content", () => {
    const hover = { contents: "" };
    const result = lspHoverToMonacoContents(hover);
    expect(result).toEqual([]);
  });

  it("filters out empty strings from array content", () => {
    const hover = { contents: ["hello", ""] };
    const result = lspHoverToMonacoContents(hover);
    expect(result.length).toBe(1);
    expect(result[0].value).toBe("hello");
  });
});

// ── Diagnostic Conversion ───────────────────────────────────────────

describe("diagnostic to marker conversion", () => {
  it("converts a full diagnostic to marker data", () => {
    const diag = {
      range: { start: { line: 4, character: 2 }, end: { line: 4, character: 10 } },
      severity: LspDiagnosticSeverity.Error,
      code: "E0308",
      source: "rust-analyzer",
      message: "mismatched types",
      tags: [LspDiagnosticTag.Unnecessary],
    };

    const marker = lspDiagnosticToMonacoMarker(diag);

    expect(marker.severity).toBe(8); // Error
    expect(marker.message).toBe("mismatched types");
    expect(marker.startLineNumber).toBe(5);
    expect(marker.startColumn).toBe(3);
    expect(marker.endLineNumber).toBe(5);
    expect(marker.endColumn).toBe(11);
    expect(marker.source).toBe("rust-analyzer");
    expect(marker.code).toBe("E0308");
    expect(marker.tags).toEqual([1]); // Unnecessary
  });

  it("handles diagnostic with code description URL", () => {
    const diag = {
      range: { start: { line: 0, character: 0 }, end: { line: 0, character: 5 } },
      severity: LspDiagnosticSeverity.Warning,
      code: "W001",
      codeDescription: { href: "https://example.com/W001" },
      message: "unused variable",
    };

    const marker = lspDiagnosticToMonacoMarker(diag);
    expect(marker.code).toEqual({ value: "W001", target: "https://example.com/W001" });
  });

  it("handles diagnostic without optional fields", () => {
    const diag = {
      range: { start: { line: 0, character: 0 }, end: { line: 0, character: 1 } },
      message: "error",
    };

    const marker = lspDiagnosticToMonacoMarker(diag);
    expect(marker.severity).toBe(2); // Default Info
    expect(marker.source).toBeUndefined();
    expect(marker.code).toBeUndefined();
    expect(marker.tags).toBeUndefined();
  });
});

// ── Completion Item Conversion ──────────────────────────────────────

describe("completion item conversion", () => {
  const defaultRange = {
    startLineNumber: 1,
    startColumn: 1,
    endLineNumber: 1,
    endColumn: 5,
  };

  it("converts a basic completion item", () => {
    const item = {
      label: "toString",
      kind: LspCompletionItemKind.Method,
      detail: "(): string",
    };

    const result = lspCompletionItemToMonaco(item, defaultRange);
    expect(result.label).toBe("toString");
    expect(result.kind).toBe(0); // Method
    expect(result.detail).toBe("(): string");
    expect(result.insertText).toBe("toString");
  });

  it("uses insertText when provided", () => {
    const item = {
      label: "forEach",
      kind: LspCompletionItemKind.Method,
      insertText: "forEach(${1:callback})",
      insertTextFormat: LspInsertTextFormat.Snippet,
    };

    const result = lspCompletionItemToMonaco(item, defaultRange);
    expect(result.insertText).toBe("forEach(${1:callback})");
    expect(result.insertTextRules).toBe(4); // InsertAsSnippet
  });

  it("uses textEdit range when provided", () => {
    const item = {
      label: "myFunc",
      kind: LspCompletionItemKind.Function,
      textEdit: {
        range: { start: { line: 0, character: 0 }, end: { line: 0, character: 3 } },
        newText: "myFunc",
      },
    };

    const result = lspCompletionItemToMonaco(item, defaultRange);
    expect(result.insertText).toBe("myFunc");
    expect(result.range).toEqual({
      startLineNumber: 1,
      startColumn: 1,
      endLineNumber: 1,
      endColumn: 4,
    });
  });

  it("handles labelDetails", () => {
    const item = {
      label: "useState",
      labelDetails: { detail: "<T>", description: "React Hook" },
      kind: LspCompletionItemKind.Function,
    };

    const result = lspCompletionItemToMonaco(item, defaultRange);
    expect(result.label).toEqual({
      label: "useState",
      detail: "<T>",
      description: "React Hook",
    });
  });

  it("preserves _lspItem for resolve", () => {
    const item = {
      label: "test",
      kind: LspCompletionItemKind.Text,
      data: { id: 42 },
    };

    const result = lspCompletionItemToMonaco(item, defaultRange);
    expect(result._lspItem).toBe(item);
  });
});

// ── Text Edit Conversion ────────────────────────────────────────────

describe("text edit conversion", () => {
  it("converts LSP text edit to Monaco edit", () => {
    const edit = {
      range: { start: { line: 5, character: 0 }, end: { line: 5, character: 10 } },
      newText: "replacement",
    };

    const result = lspTextEditToMonaco(edit);
    expect(result.range).toEqual({
      startLineNumber: 6,
      startColumn: 1,
      endLineNumber: 6,
      endColumn: 11,
    });
    expect(result.text).toBe("replacement");
  });
});
