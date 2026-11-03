/**
 * LSP protocol type definitions used by the renderer-side LSP client.
 *
 * These mirror the Language Server Protocol specification types but only
 * include the subset we actually consume. Full spec:
 * https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/
 */

// ── Positions & Ranges ──────────────────────────────────────────────

/** LSP positions are 0-indexed (both line and character). */
export interface LspPosition {
  line: number;
  character: number;
}

export interface LspRange {
  start: LspPosition;
  end: LspPosition;
}

export interface LspLocation {
  uri: string;
  range: LspRange;
}

export interface LspLocationLink {
  originSelectionRange?: LspRange;
  targetUri: string;
  targetRange: LspRange;
  targetSelectionRange: LspRange;
}

// ── Text Document Identifiers ───────────────────────────────────────

export interface LspTextDocumentIdentifier {
  uri: string;
}

export interface LspVersionedTextDocumentIdentifier {
  uri: string;
  version: number;
}

export interface LspTextDocumentPositionParams {
  textDocument: LspTextDocumentIdentifier;
  position: LspPosition;
}

// ── Diagnostics ─────────────────────────────────────────────────────

export enum LspDiagnosticSeverity {
  Error = 1,
  Warning = 2,
  Information = 3,
  Hint = 4,
}

export enum LspDiagnosticTag {
  Unnecessary = 1,
  Deprecated = 2,
}

export interface LspDiagnostic {
  range: LspRange;
  severity?: LspDiagnosticSeverity;
  code?: number | string;
  codeDescription?: { href: string };
  source?: string;
  message: string;
  tags?: LspDiagnosticTag[];
  relatedInformation?: LspDiagnosticRelatedInformation[];
}

export interface LspDiagnosticRelatedInformation {
  location: LspLocation;
  message: string;
}

export interface LspPublishDiagnosticsParams {
  uri: string;
  version?: number;
  diagnostics: LspDiagnostic[];
}

// ── Completion ──────────────────────────────────────────────────────

export enum LspCompletionItemKind {
  Text = 1,
  Method = 2,
  Function = 3,
  Constructor = 4,
  Field = 5,
  Variable = 6,
  Class = 7,
  Interface = 8,
  Module = 9,
  Property = 10,
  Unit = 11,
  Value = 12,
  Enum = 13,
  Keyword = 14,
  Snippet = 15,
  Color = 16,
  File = 17,
  Reference = 18,
  Folder = 19,
  EnumMember = 20,
  Constant = 21,
  Struct = 22,
  Event = 23,
  Operator = 24,
  TypeParameter = 25,
}

export enum LspInsertTextFormat {
  PlainText = 1,
  Snippet = 2,
}

export interface LspTextEdit {
  range: LspRange;
  newText: string;
}

export interface LspCompletionItem {
  label: string;
  labelDetails?: { detail?: string; description?: string };
  kind?: LspCompletionItemKind;
  detail?: string;
  documentation?: string | LspMarkupContent;
  deprecated?: boolean;
  preselect?: boolean;
  sortText?: string;
  filterText?: string;
  insertText?: string;
  insertTextFormat?: LspInsertTextFormat;
  textEdit?: LspTextEdit | LspInsertReplaceEdit;
  additionalTextEdits?: LspTextEdit[];
  commitCharacters?: string[];
  data?: unknown;
}

export interface LspInsertReplaceEdit {
  newText: string;
  insert: LspRange;
  replace: LspRange;
}

export interface LspCompletionList {
  isIncomplete: boolean;
  items: LspCompletionItem[];
}

// ── Hover ───────────────────────────────────────────────────────────

export interface LspMarkupContent {
  kind: "plaintext" | "markdown";
  value: string;
}

/** Legacy hover format — some servers still use this. */
export interface LspMarkedString {
  language: string;
  value: string;
}

export interface LspHover {
  contents: LspMarkupContent | LspMarkedString | string | Array<LspMarkedString | string>;
  range?: LspRange;
}

// ── Server Capabilities (subset we care about) ──────────────────────

export interface LspCompletionOptions {
  triggerCharacters?: string[];
  resolveProvider?: boolean;
}

export interface LspSignatureHelpOptions {
  triggerCharacters?: string[];
  retriggerCharacters?: string[];
}

export interface LspServerCapabilities {
  completionProvider?: LspCompletionOptions;
  hoverProvider?: boolean | object;
  definitionProvider?: boolean | object;
  typeDefinitionProvider?: boolean | object;
  implementationProvider?: boolean | object;
  referencesProvider?: boolean | object;
  documentSymbolProvider?: boolean | object;
  workspaceSymbolProvider?: boolean | object;
  codeActionProvider?: boolean | object;
  codeLensProvider?: boolean | { resolveProvider?: boolean };
  documentFormattingProvider?: boolean | object;
  documentRangeFormattingProvider?: boolean | object;
  documentOnTypeFormattingProvider?: {
    firstTriggerCharacter: string;
    moreTriggerCharacter?: string[];
  };
  renameProvider?: boolean | { prepareProvider?: boolean };
  signatureHelpProvider?: LspSignatureHelpOptions;
  foldingRangeProvider?: boolean | object;
  inlayHintProvider?: boolean | { resolveProvider?: boolean };
  documentHighlightProvider?: boolean | object;
  linkedEditingRangeProvider?: boolean | object;
  semanticTokensProvider?: object;
  textDocumentSync?: number | { openClose?: boolean; change?: number; save?: boolean | { includeText?: boolean } };
  [key: string]: unknown;
}

// ── Inlay Hints ─────────────────────────────────────────────────────

export enum LspInlayHintKind {
  Type = 1,
  Parameter = 2,
}

export interface LspInlayHint {
  position: LspPosition;
  label: string | Array<{ value: string; tooltip?: string | LspMarkupContent }>;
  kind?: LspInlayHintKind;
  tooltip?: string | LspMarkupContent;
  paddingLeft?: boolean;
  paddingRight?: boolean;
}

// ── Document Highlight ──────────────────────────────────────────────

export enum LspDocumentHighlightKind {
  Text = 1,
  Read = 2,
  Write = 3,
}

export interface LspDocumentHighlight {
  range: LspRange;
  kind?: LspDocumentHighlightKind;
}

// ── Workspace Symbol ────────────────────────────────────────────────

export interface LspWorkspaceSymbol {
  name: string;
  kind: number;
  location: LspLocation | { uri: string };
  containerName?: string;
}

// ── Selection Range ─────────────────────────────────────────────────

export interface LspSelectionRange {
  range: LspRange;
  parent?: LspSelectionRange;
}

// ── Document Color ──────────────────────────────────────────────────

export interface LspColor {
  red: number;
  green: number;
  blue: number;
  alpha: number;
}

export interface LspColorInformation {
  range: LspRange;
  color: LspColor;
}

export interface LspColorPresentation {
  label: string;
  textEdit?: LspTextEdit;
  additionalTextEdits?: LspTextEdit[];
}

// ── Code Lens ───────────────────────────────────────────────────────

export interface LspCommand {
  title: string;
  command: string;
  arguments?: unknown[];
}

export interface LspCodeLens {
  range: LspRange;
  command?: LspCommand;
  data?: unknown;
}

// ── Call Hierarchy ──────────────────────────────────────────────────

export interface LspCallHierarchyItem {
  name: string;
  kind: number;
  tags?: number[];
  detail?: string;
  uri: string;
  range: LspRange;
  selectionRange: LspRange;
  data?: unknown;
}

export interface LspCallHierarchyIncomingCall {
  from: LspCallHierarchyItem;
  fromRanges: LspRange[];
}

export interface LspCallHierarchyOutgoingCall {
  to: LspCallHierarchyItem;
  fromRanges: LspRange[];
}

// ── Code Actions ────────────────────────────────────────────────────

export enum LspCodeActionKind {
  QuickFix = "quickfix",
  Refactor = "refactor",
  RefactorExtract = "refactor.extract",
  RefactorInline = "refactor.inline",
  RefactorRewrite = "refactor.rewrite",
  Source = "source",
  SourceOrganizeImports = "source.organizeImports",
}

export interface LspCodeAction {
  title: string;
  kind?: string;
  diagnostics?: LspDiagnostic[];
  isPreferred?: boolean;
  disabled?: { reason: string };
  edit?: LspWorkspaceEdit;
  command?: LspCommand;
  data?: unknown;
}

export interface LspCommand {
  title: string;
  command: string;
  arguments?: unknown[];
}

// ── Workspace Edit ──────────────────────────────────────────────────

export interface LspWorkspaceEdit {
  changes?: Record<string, LspTextEdit[]>;
  documentChanges?: LspTextDocumentEdit[];
}

export interface LspTextDocumentEdit {
  textDocument: LspVersionedTextDocumentIdentifier;
  edits: LspTextEdit[];
}

// ── Rename ──────────────────────────────────────────────────────────

export interface LspPrepareRenameResult {
  range: LspRange;
  placeholder: string;
}

// ── Signature Help ──────────────────────────────────────────────────

export interface LspSignatureHelp {
  signatures: LspSignatureInformation[];
  activeSignature?: number;
  activeParameter?: number;
}

export interface LspSignatureInformation {
  label: string;
  documentation?: string | LspMarkupContent;
  parameters?: LspParameterInformation[];
  activeParameter?: number;
}

export interface LspParameterInformation {
  label: string | [number, number];
  documentation?: string | LspMarkupContent;
}

// ── Formatting ──────────────────────────────────────────────────────

export interface LspFormattingOptions {
  tabSize: number;
  insertSpaces: boolean;
  trimTrailingWhitespace?: boolean;
  insertFinalNewline?: boolean;
  trimFinalNewlines?: boolean;
}

// ── JSON-RPC Message Types ──────────────────────────────────────────

export interface LspRequestMessage {
  jsonrpc: "2.0";
  id: number;
  method: string;
  params?: unknown;
}

export interface LspResponseMessage {
  jsonrpc: "2.0";
  id: number;
  result?: unknown;
  error?: LspResponseError;
}

export interface LspResponseError {
  code: number;
  message: string;
  data?: unknown;
}

export interface LspNotificationMessage {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export type LspMessage = LspRequestMessage | LspResponseMessage | LspNotificationMessage;
