/**
 * Conversion functions between LSP types and Monaco types.
 *
 * Key differences:
 * - Position indexing: LSP is 0-indexed, Monaco is 1-indexed
 * - Severity mapping: LSP 1-4 → Monaco MarkerSeverity
 * - CompletionItemKind: LSP enum → Monaco enum (mostly 1:1 with offset)
 * - Markup content: LSP MarkupContent → Monaco IMarkdownString
 */

import type * as monaco from "monaco-editor";
import {
  type LspPosition,
  type LspRange,
  type LspDiagnostic,
  type LspCompletionItem,
  type LspHover,
  type LspMarkupContent,
  type LspMarkedString,
  type LspTextEdit,
  type LspLocation,
  type LspLocationLink,
  type LspSignatureHelp,
  type LspSignatureInformation,
  type LspParameterInformation,
  type LspCodeAction,
  LspDiagnosticSeverity,
  LspDiagnosticTag,
  LspCompletionItemKind,
  LspInsertTextFormat,
} from "./lsp-types";

// ── Position Conversion ─────────────────────────────────────────────

/** Convert LSP 0-indexed position to Monaco 1-indexed position. */
export function lspToMonacoPosition(pos: LspPosition): monaco.IPosition {
  return {
    lineNumber: pos.line + 1,
    column: pos.character + 1,
  };
}

/** Convert Monaco 1-indexed position to LSP 0-indexed position. */
export function monacoToLspPosition(pos: monaco.IPosition): LspPosition {
  return {
    line: pos.lineNumber - 1,
    character: pos.column - 1,
  };
}

/** Convert LSP range to Monaco range. */
export function lspToMonacoRange(range: LspRange): monaco.IRange {
  return {
    startLineNumber: range.start.line + 1,
    startColumn: range.start.character + 1,
    endLineNumber: range.end.line + 1,
    endColumn: range.end.character + 1,
  };
}

/** Convert Monaco range to LSP range. */
export function monacoToLspRange(range: monaco.IRange): LspRange {
  return {
    start: { line: range.startLineNumber - 1, character: range.startColumn - 1 },
    end: { line: range.endLineNumber - 1, character: range.endColumn - 1 },
  };
}

// ── Severity Mapping ────────────────────────────────────────────────

// Monaco MarkerSeverity values (avoid importing monaco in pure logic)
const MONACO_SEVERITY_HINT = 1;
const MONACO_SEVERITY_INFO = 2;
const MONACO_SEVERITY_WARNING = 4;
const MONACO_SEVERITY_ERROR = 8;

/** Map LSP DiagnosticSeverity to Monaco MarkerSeverity. */
export function lspSeverityToMonaco(severity: LspDiagnosticSeverity | undefined): number {
  switch (severity) {
    case LspDiagnosticSeverity.Error:
      return MONACO_SEVERITY_ERROR;
    case LspDiagnosticSeverity.Warning:
      return MONACO_SEVERITY_WARNING;
    case LspDiagnosticSeverity.Information:
      return MONACO_SEVERITY_INFO;
    case LspDiagnosticSeverity.Hint:
      return MONACO_SEVERITY_HINT;
    default:
      return MONACO_SEVERITY_INFO;
  }
}

// ── Diagnostic Tag Mapping ──────────────────────────────────────────

// Monaco MarkerTag values
const MONACO_TAG_UNNECESSARY = 1;
const MONACO_TAG_DEPRECATED = 2;

/** Map LSP diagnostic tags to Monaco marker tags. */
export function lspTagsToMonaco(tags: LspDiagnosticTag[] | undefined): number[] {
  if (!tags || tags.length === 0) return [];
  const result: number[] = [];
  for (const tag of tags) {
    if (tag === LspDiagnosticTag.Unnecessary) result.push(MONACO_TAG_UNNECESSARY);
    if (tag === LspDiagnosticTag.Deprecated) result.push(MONACO_TAG_DEPRECATED);
  }
  return result;
}

// ── Diagnostic Conversion ───────────────────────────────────────────

export interface MonacoMarkerData {
  severity: number;
  message: string;
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
  source?: string;
  code?: string | { value: string; target: string };
  tags?: number[];
}

/** Convert an LSP diagnostic to a Monaco marker data object. */
export function lspDiagnosticToMonacoMarker(diag: LspDiagnostic): MonacoMarkerData {
  const range = lspToMonacoRange(diag.range);
  const marker: MonacoMarkerData = {
    severity: lspSeverityToMonaco(diag.severity),
    message: diag.message,
    startLineNumber: range.startLineNumber,
    startColumn: range.startColumn,
    endLineNumber: range.endLineNumber,
    endColumn: range.endColumn,
  };

  if (diag.source) {
    marker.source = diag.source;
  }

  if (diag.code != null) {
    if (diag.codeDescription?.href) {
      marker.code = { value: String(diag.code), target: diag.codeDescription.href };
    } else {
      marker.code = String(diag.code);
    }
  }

  const tags = lspTagsToMonaco(diag.tags);
  if (tags.length > 0) {
    marker.tags = tags;
  }

  return marker;
}

// ── Completion Kind Mapping ─────────────────────────────────────────

// Monaco CompletionItemKind values
const MONACO_COMPLETION_KIND = {
  Method: 0,
  Function: 1,
  Constructor: 2,
  Field: 3,
  Variable: 4,
  Class: 5,
  Struct: 6,
  Interface: 7,
  Module: 8,
  Property: 9,
  Event: 10,
  Operator: 11,
  Unit: 12,
  Value: 13,
  Constant: 14,
  Enum: 15,
  EnumMember: 16,
  Keyword: 17,
  Text: 18,
  Color: 19,
  File: 20,
  Reference: 21,
  Customcolor: 22,
  Folder: 23,
  TypeParameter: 24,
  User: 25,
  Issue: 26,
  Snippet: 27,
} as const;

/** Map LSP CompletionItemKind to Monaco CompletionItemKind. */
export function lspCompletionKindToMonaco(kind: LspCompletionItemKind | undefined): number {
  switch (kind) {
    case LspCompletionItemKind.Text:
      return MONACO_COMPLETION_KIND.Text;
    case LspCompletionItemKind.Method:
      return MONACO_COMPLETION_KIND.Method;
    case LspCompletionItemKind.Function:
      return MONACO_COMPLETION_KIND.Function;
    case LspCompletionItemKind.Constructor:
      return MONACO_COMPLETION_KIND.Constructor;
    case LspCompletionItemKind.Field:
      return MONACO_COMPLETION_KIND.Field;
    case LspCompletionItemKind.Variable:
      return MONACO_COMPLETION_KIND.Variable;
    case LspCompletionItemKind.Class:
      return MONACO_COMPLETION_KIND.Class;
    case LspCompletionItemKind.Interface:
      return MONACO_COMPLETION_KIND.Interface;
    case LspCompletionItemKind.Module:
      return MONACO_COMPLETION_KIND.Module;
    case LspCompletionItemKind.Property:
      return MONACO_COMPLETION_KIND.Property;
    case LspCompletionItemKind.Unit:
      return MONACO_COMPLETION_KIND.Unit;
    case LspCompletionItemKind.Value:
      return MONACO_COMPLETION_KIND.Value;
    case LspCompletionItemKind.Enum:
      return MONACO_COMPLETION_KIND.Enum;
    case LspCompletionItemKind.Keyword:
      return MONACO_COMPLETION_KIND.Keyword;
    case LspCompletionItemKind.Snippet:
      return MONACO_COMPLETION_KIND.Snippet;
    case LspCompletionItemKind.Color:
      return MONACO_COMPLETION_KIND.Color;
    case LspCompletionItemKind.File:
      return MONACO_COMPLETION_KIND.File;
    case LspCompletionItemKind.Reference:
      return MONACO_COMPLETION_KIND.Reference;
    case LspCompletionItemKind.Folder:
      return MONACO_COMPLETION_KIND.Folder;
    case LspCompletionItemKind.EnumMember:
      return MONACO_COMPLETION_KIND.EnumMember;
    case LspCompletionItemKind.Constant:
      return MONACO_COMPLETION_KIND.Constant;
    case LspCompletionItemKind.Struct:
      return MONACO_COMPLETION_KIND.Struct;
    case LspCompletionItemKind.Event:
      return MONACO_COMPLETION_KIND.Event;
    case LspCompletionItemKind.Operator:
      return MONACO_COMPLETION_KIND.Operator;
    case LspCompletionItemKind.TypeParameter:
      return MONACO_COMPLETION_KIND.TypeParameter;
    default:
      return MONACO_COMPLETION_KIND.Text;
  }
}

// ── Insert Text Format ──────────────────────────────────────────────

// Monaco CompletionItemInsertTextRule
const MONACO_INSERT_AS_SNIPPET = 4;

/** Map LSP InsertTextFormat to Monaco insertTextRules. */
export function lspInsertTextFormatToMonaco(format: LspInsertTextFormat | undefined): number {
  if (format === LspInsertTextFormat.Snippet) {
    return MONACO_INSERT_AS_SNIPPET;
  }
  return 0; // No special rules for plain text
}

// ── Markup Content Rendering ────────────────────────────────────────

export interface MonacoMarkdownString {
  value: string;
  isTrusted?: boolean;
}

/** Convert LSP MarkupContent to Monaco IMarkdownString. */
export function lspMarkupToMonaco(content: LspMarkupContent): MonacoMarkdownString {
  if (content.kind === "markdown") {
    return { value: content.value, isTrusted: true };
  }
  // Plaintext: wrap in code fence to preserve formatting
  return { value: "```\n" + content.value + "\n```" };
}

/** Convert a MarkedString (legacy hover format) to Monaco markdown. */
export function lspMarkedStringToMonaco(ms: LspMarkedString | string): MonacoMarkdownString {
  if (typeof ms === "string") {
    return { value: ms };
  }
  return { value: "```" + ms.language + "\n" + ms.value + "\n```" };
}

/**
 * Convert LSP hover contents (which can be many formats) to an array
 * of Monaco markdown strings suitable for the hover widget.
 */
export function lspHoverToMonacoContents(
  hover: LspHover,
): MonacoMarkdownString[] {
  const { contents } = hover;

  // Single MarkupContent
  if (isMarkupContent(contents)) {
    return [lspMarkupToMonaco(contents)];
  }

  // Single string
  if (typeof contents === "string") {
    return contents ? [{ value: contents }] : [];
  }

  // Array of MarkedString | string
  if (Array.isArray(contents)) {
    return contents
      .map((item) => lspMarkedStringToMonaco(item))
      .filter((item) => item.value.length > 0);
  }

  // Single MarkedString object
  if (isMarkedString(contents)) {
    return [lspMarkedStringToMonaco(contents)];
  }

  return [];
}

// ── Completion Item Conversion ──────────────────────────────────────

export interface MonacoCompletionItem {
  label: string | { label: string; detail?: string; description?: string };
  kind: number;
  detail?: string;
  documentation?: MonacoMarkdownString;
  sortText?: string;
  filterText?: string;
  insertText: string;
  insertTextRules?: number;
  range?: monaco.IRange;
  additionalTextEdits?: Array<{ range: monaco.IRange; text: string }>;
  commitCharacters?: string[];
  preselect?: boolean;
  /** Stash the original LSP item for resolve requests. */
  _lspItem?: LspCompletionItem;
}

/** Convert an LSP CompletionItem to a Monaco CompletionItem. */
export function lspCompletionItemToMonaco(
  item: LspCompletionItem,
  defaultRange: monaco.IRange,
): MonacoCompletionItem {
  const label = item.labelDetails
    ? { label: item.label, detail: item.labelDetails.detail, description: item.labelDetails.description }
    : item.label;

  let insertText = item.insertText ?? item.label;
  let range = defaultRange;

  // Handle textEdit
  if (item.textEdit) {
    if ("range" in item.textEdit) {
      insertText = item.textEdit.newText;
      range = lspToMonacoRange(item.textEdit.range);
    } else if ("insert" in item.textEdit) {
      // InsertReplaceEdit — use the replace range
      insertText = item.textEdit.newText;
      range = lspToMonacoRange(item.textEdit.replace);
    }
  }

  const result: MonacoCompletionItem = {
    label,
    kind: lspCompletionKindToMonaco(item.kind),
    insertText,
    range,
    _lspItem: item,
  };

  if (item.detail) result.detail = item.detail;
  if (item.sortText) result.sortText = item.sortText;
  if (item.filterText) result.filterText = item.filterText;
  if (item.preselect) result.preselect = item.preselect;
  if (item.commitCharacters) result.commitCharacters = item.commitCharacters;

  // Insert text format (snippet support)
  const insertRules = lspInsertTextFormatToMonaco(item.insertTextFormat);
  if (insertRules) result.insertTextRules = insertRules;

  // Documentation
  if (item.documentation) {
    if (typeof item.documentation === "string") {
      result.documentation = { value: item.documentation };
    } else {
      result.documentation = lspMarkupToMonaco(item.documentation);
    }
  }

  // Additional text edits
  if (item.additionalTextEdits && item.additionalTextEdits.length > 0) {
    result.additionalTextEdits = item.additionalTextEdits.map((edit) => ({
      range: lspToMonacoRange(edit.range),
      text: edit.newText,
    }));
  }

  return result;
}

/** Convert an LSP TextEdit to a Monaco single edit operation. */
export function lspTextEditToMonaco(edit: LspTextEdit): { range: monaco.IRange; text: string } {
  return {
    range: lspToMonacoRange(edit.range),
    text: edit.newText,
  };
}

// ── Type Guards ─────────────────────────────────────────────────────

function isMarkupContent(value: unknown): value is LspMarkupContent {
  return (
    typeof value === "object" &&
    value !== null &&
    "kind" in value &&
    "value" in value &&
    ((value as LspMarkupContent).kind === "markdown" || (value as LspMarkupContent).kind === "plaintext")
  );
}

function isMarkedString(value: unknown): value is LspMarkedString {
  return (
    typeof value === "object" &&
    value !== null &&
    "language" in value &&
    "value" in value
  );
}

// ── Location Conversion ─────────────────────────────────────────────

export interface MonacoLocationResult {
  uri: string;
  range: monaco.IRange;
}

/** Convert an LSP Location to a Monaco location-like result. */
export function lspLocationToMonaco(loc: LspLocation): MonacoLocationResult {
  return {
    uri: loc.uri,
    range: lspToMonacoRange(loc.range),
  };
}

/** Convert an LSP LocationLink to a Monaco location-like result. */
export function lspLocationLinkToMonaco(link: LspLocationLink): MonacoLocationResult {
  return {
    uri: link.targetUri,
    range: lspToMonacoRange(link.targetSelectionRange),
  };
}

/**
 * Normalize a definition response (Location | Location[] | LocationLink[])
 * into a uniform array of Monaco location results.
 */
export function lspDefinitionResultToMonaco(
  result: LspLocation | LspLocation[] | LspLocationLink[] | null,
): MonacoLocationResult[] {
  if (!result) return [];

  if (Array.isArray(result)) {
    if (result.length === 0) return [];
    // Detect LocationLink[] vs Location[]
    const first = result[0];
    if ("targetUri" in first) {
      return (result as LspLocationLink[]).map(lspLocationLinkToMonaco);
    }
    return (result as LspLocation[]).map(lspLocationToMonaco);
  }

  // Single Location
  return [lspLocationToMonaco(result as LspLocation)];
}

// ── Signature Help Conversion ───────────────────────────────────────

export interface MonacoSignatureHelp {
  signatures: MonacoSignatureInformation[];
  activeSignature: number;
  activeParameter: number;
}

export interface MonacoSignatureInformation {
  label: string;
  documentation?: MonacoMarkdownString;
  parameters: MonacoParameterInformation[];
  activeParameter?: number;
}

export interface MonacoParameterInformation {
  label: string | [number, number];
  documentation?: MonacoMarkdownString;
}

/** Convert LSP SignatureHelp to Monaco SignatureHelp. */
export function lspSignatureHelpToMonaco(help: LspSignatureHelp): MonacoSignatureHelp {
  return {
    signatures: help.signatures.map(lspSignatureInfoToMonaco),
    activeSignature: help.activeSignature ?? 0,
    activeParameter: help.activeParameter ?? 0,
  };
}

function lspSignatureInfoToMonaco(sig: LspSignatureInformation): MonacoSignatureInformation {
  const result: MonacoSignatureInformation = {
    label: sig.label,
    parameters: (sig.parameters ?? []).map(lspParameterInfoToMonaco),
  };

  if (sig.documentation) {
    if (typeof sig.documentation === "string") {
      result.documentation = { value: sig.documentation };
    } else {
      result.documentation = lspMarkupToMonaco(sig.documentation);
    }
  }

  if (sig.activeParameter != null) {
    result.activeParameter = sig.activeParameter;
  }

  return result;
}

function lspParameterInfoToMonaco(param: LspParameterInformation): MonacoParameterInformation {
  const result: MonacoParameterInformation = {
    label: param.label,
  };

  if (param.documentation) {
    if (typeof param.documentation === "string") {
      result.documentation = { value: param.documentation };
    } else {
      result.documentation = lspMarkupToMonaco(param.documentation);
    }
  }

  return result;
}

// ── Code Action Conversion ──────────────────────────────────────────

export interface MonacoCodeAction {
  title: string;
  kind?: string;
  diagnostics?: MonacoMarkerData[];
  isPreferred?: boolean;
  disabled?: { reason: string };
  edit?: { edits: Array<{ resource: string; textEdit: { range: monaco.IRange; text: string } }> };
  _lspCodeAction?: LspCodeAction;
}

/** Convert an LSP CodeAction to a Monaco-compatible code action. */
export function lspCodeActionToMonaco(action: LspCodeAction): MonacoCodeAction {
  const result: MonacoCodeAction = {
    title: action.title,
    _lspCodeAction: action,
  };

  if (action.kind) result.kind = action.kind;
  if (action.isPreferred) result.isPreferred = action.isPreferred;
  if (action.disabled) result.disabled = action.disabled;

  if (action.diagnostics) {
    result.diagnostics = action.diagnostics.map(lspDiagnosticToMonacoMarker);
  }

  return result;
}
