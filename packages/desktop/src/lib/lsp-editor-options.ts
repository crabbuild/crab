/**
 * Derive Monaco editor options from the active language and LSP
 * server capabilities.
 *
 * Two inputs drive the final option set:
 *   1. Language conventions (tab size, soft-tabs vs hard-tabs, etc.)
 *   2. LSP server capabilities (which features the server supports)
 *
 * The result is a partial Monaco IEditorOptions object that the
 * CodeEditor applies after each model swap or LSP capability change.
 */

import type * as monaco from "monaco-editor";
import type { LspServerCapabilities } from "./lsp-types";

// ── Language conventions ────────────────────────────────────────────

/**
 * Per-language indentation defaults matching the conventions each
 * community's formatter uses (rustfmt, prettier, black, gofmt).
 *
 * These can be overridden by EditorConfig — Monaco picks up
 * `.editorconfig` automatically when `detectIndentation` is true —
 * but we provide sensible fallbacks here.
 */
interface LanguageDefaults {
  tabSize: number;
  insertSpaces: boolean;
  /** How Monaco should fold by default (before the server reports ranges). */
  foldingStrategy?: "auto" | "indentation";
}

const LANGUAGE_DEFAULTS: Record<string, LanguageDefaults> = {
  typescript: { tabSize: 2, insertSpaces: true, foldingStrategy: "auto" },
  javascript: { tabSize: 2, insertSpaces: true, foldingStrategy: "auto" },
  tsx: { tabSize: 2, insertSpaces: true, foldingStrategy: "auto" },
  jsx: { tabSize: 2, insertSpaces: true, foldingStrategy: "auto" },
  json: { tabSize: 2, insertSpaces: true },
  jsonc: { tabSize: 2, insertSpaces: true },
  html: { tabSize: 2, insertSpaces: true },
  css: { tabSize: 2, insertSpaces: true },
  scss: { tabSize: 2, insertSpaces: true },
  less: { tabSize: 2, insertSpaces: true },
  yaml: { tabSize: 2, insertSpaces: true },
  markdown: { tabSize: 2, insertSpaces: true },

  rust: { tabSize: 4, insertSpaces: true, foldingStrategy: "auto" },
  python: { tabSize: 4, insertSpaces: true, foldingStrategy: "indentation" },
  java: { tabSize: 4, insertSpaces: true },
  csharp: { tabSize: 4, insertSpaces: true },
  cpp: { tabSize: 4, insertSpaces: true },
  c: { tabSize: 4, insertSpaces: true },

  // Go uses tabs — gofmt enforces this.
  go: { tabSize: 4, insertSpaces: false, foldingStrategy: "auto" },

  // Makefiles MUST use tabs.
  makefile: { tabSize: 4, insertSpaces: false },
};

/** Default for languages not explicitly listed. */
const FALLBACK_DEFAULTS: LanguageDefaults = { tabSize: 2, insertSpaces: true };

// ── Option derivation ───────────────────────────────────────────────

export interface DerivedEditorOptions extends monaco.editor.IStandaloneEditorConstructionOptions {
  tabSize?: number;
  insertSpaces?: boolean;
  detectIndentation?: boolean;
}

/**
 * Compute Monaco editor options from a language id and optional LSP
 * capabilities.
 *
 * When capabilities are provided, LSP-aware features light up:
 *   - parameterHints, inlayHints, codeLens, linkedEditing, formatOnPaste
 *
 * When no capabilities are available (no LSP running), safe defaults
 * are used that don't reference features the editor can't fulfil.
 */
export function deriveEditorOptions(
  language: string,
  capabilities: LspServerCapabilities | null,
): DerivedEditorOptions {
  const langDefaults = LANGUAGE_DEFAULTS[language] ?? FALLBACK_DEFAULTS;
  const hasLsp = capabilities !== null;

  const options: DerivedEditorOptions = {
    tabSize: langDefaults.tabSize,
    insertSpaces: langDefaults.insertSpaces,
    detectIndentation: true, // Honour .editorconfig / existing file indentation

    // Folding: LSP `foldingRangeProvider` gives syntactic folds; fall back
    // to Monaco's heuristic strategy when unavailable.
    folding: true,
    foldingStrategy: capabilities?.foldingRangeProvider
      ? "auto"
      : (langDefaults.foldingStrategy ?? "auto"),

    // Completions: when LSP is running, the server's completions are
    // better than Monaco's word-based ones. Turning off word-based
    // suggestions keeps the popup clean.
    wordBasedSuggestions: hasLsp ? "off" : "currentDocument",

    // Auto-trigger the suggestion widget on typing trigger chars.
    suggestOnTriggerCharacters: !!capabilities?.completionProvider,
    quickSuggestions: hasLsp
      ? { other: "on", comments: "off", strings: "off" }
      : { other: "on", comments: "off", strings: "off" },
    acceptSuggestionOnEnter: "on",
    tabCompletion: "on",

    // Parameter hints popup (signature help).
    parameterHints: {
      enabled: !!capabilities?.signatureHelpProvider,
      cycle: true,
    },

    // Hover widget — delay tuned to match VS Code.
    hover: {
      enabled: !!capabilities?.hoverProvider,
      delay: 300,
      sticky: true,
    },

    // Inlay hints — parameter names + inferred types inline.
    inlayHints: {
      enabled: capabilities?.inlayHintProvider ? "on" : "off",
    },

    // Code lens — reference counts, run-test buttons, etc.
    codeLens: !!capabilities?.codeLensProvider,

    // Linked editing — rename matching JSX/HTML tags as you type.
    linkedEditing: !!capabilities?.linkedEditingRangeProvider,

    // Formatting — triggered on paste and mid-typing when the server
    // advertises the capability. formatOnType is opt-in because it
    // can be jarring for languages that don't have great on-type support.
    formatOnPaste: !!(capabilities?.documentFormattingProvider || capabilities?.documentRangeFormattingProvider),
    formatOnType: !!capabilities?.documentOnTypeFormattingProvider,

    // Occurrence highlighting (document highlights).
    occurrencesHighlight: capabilities?.documentHighlightProvider ? "singleFile" : "off",

    // Semantic highlighting overlays Shiki tokens.
    "semanticHighlighting.enabled": !!capabilities?.semanticTokensProvider,

    // Reference/definition UX.
    definitionLinkOpensInPeek: false,
    gotoLocation: {
      multiple: "peek",
      multipleDefinitions: "peek",
      multipleReferences: "gotoAndPeek",
      multipleImplementations: "peek",
      multipleTypeDefinitions: "peek",
    },

    // Semantic smart-select (Opt+Shift+↑/↓).
    // Monaco has a built-in bracket-based strategy; when LSP provides
    // `selectionRangeProvider`, the provider gives us semantic boundaries
    // (expand from identifier → expression → statement → function body).
    smartSelect: { selectLeadingAndTrailingWhitespace: false },

    // Sticky scroll — shows enclosing scope (class/function) at top of viewport.
    stickyScroll: { enabled: true, maxLineCount: 5 },

    // Bracket guides + pair colorization stay on (already enabled globally).
  };

  return options;
}

/**
 * Compute the editor options to pass at *create* time. These are the
 * subset of options that Monaco honors during construction (some
 * options can only be set via updateOptions after creation).
 */
export function initialEditorOptions(
  language: string,
  capabilities: LspServerCapabilities | null,
): Partial<monaco.editor.IStandaloneEditorConstructionOptions> {
  return deriveEditorOptions(language, capabilities) as Partial<monaco.editor.IStandaloneEditorConstructionOptions>;
}
