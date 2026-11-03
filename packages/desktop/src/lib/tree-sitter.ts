// Tree-sitter WASM initialization and parser management.
//
// Lazily loads the web-tree-sitter runtime and grammar WASM files on first
// use. Parsers are cached per language so subsequent calls are instant.
// Grammar WASM files are expected under `public/grammars/` (copied at build
// time or downloaded separately).

import type Parser from "web-tree-sitter";

// Re-export the Tree type for consumers that need it.
export type Tree = Parser.Tree;

// Extension → grammar language key mapping.
const EXT_TO_LANGUAGE: Record<string, string> = {
  ts: "typescript",
  tsx: "typescript",
  js: "typescript",
  jsx: "typescript",
  mts: "typescript",
  cts: "typescript",
  mjs: "typescript",
  cjs: "typescript",
  py: "python",
  pyi: "python",
  rs: "rust",
  go: "go",
  java: "java",
};

// Grammar WASM file paths relative to the public root.
// These files must be placed in `public/grammars/` — they can be sourced
// from tree-sitter grammar npm packages or built from source.
const GRAMMAR_PATHS: Record<string, string> = {
  typescript: "/grammars/tree-sitter-typescript.wasm",
  python: "/grammars/tree-sitter-python.wasm",
  rust: "/grammars/tree-sitter-rust.wasm",
  go: "/grammars/tree-sitter-go.wasm",
  java: "/grammars/tree-sitter-java.wasm",
};

let treeSitterModule: typeof Parser | null = null;
const languageCache = new Map<string, Parser.Language>();
const parserCache = new Map<string, Parser>();

/**
 * Initialize the web-tree-sitter WASM runtime.
 *
 * Safe to call multiple times — subsequent calls are no-ops.
 * Called automatically by `getParser` so most consumers don't need this.
 */
export async function initTreeSitter(): Promise<void> {
  if (treeSitterModule) return;

  const TreeSitter = (await import("web-tree-sitter")).default;
  await TreeSitter.init({
    locateFile: (scriptName: string) => {
      // The tree-sitter.wasm runtime file is resolved from the package.
      // Vite will handle the asset path in production builds.
      return scriptName;
    },
  });
  treeSitterModule = TreeSitter as unknown as typeof Parser;
}

/**
 * Determine the tree-sitter language key for a file path based on extension.
 *
 * Returns null for unsupported file types.
 */
export function getLanguageForFile(path: string): string | null {
  const dot = path.lastIndexOf(".");
  if (dot === -1) return null;
  const ext = path.slice(dot + 1).toLowerCase();
  return EXT_TO_LANGUAGE[ext] ?? null;
}

/**
 * Get (or create) a parser configured for the given language.
 *
 * Initializes the WASM runtime on first call. Returns null if the language
 * is not supported or the grammar WASM file cannot be loaded.
 */
export async function getParser(language: string): Promise<Parser | null> {
  const grammarPath = GRAMMAR_PATHS[language];
  if (!grammarPath) return null;

  await initTreeSitter();
  if (!treeSitterModule) return null;

  const cached = parserCache.get(language);
  if (cached) return cached;

  let lang = languageCache.get(language);
  if (!lang) {
    try {
      lang = await treeSitterModule.Language.load(grammarPath);
    } catch {
      // Grammar file not available — this is expected until grammars are
      // bundled or downloaded.
      return null;
    }
    languageCache.set(language, lang);
  }

  const parser = new treeSitterModule();
  parser.setLanguage(lang);
  parserCache.set(language, parser);
  return parser;
}

/**
 * Parse source code synchronously using a cached parser.
 *
 * Returns null if the language is unsupported or the parser hasn't been
 * initialized yet (call `getParser` first to ensure async init is done).
 */
export function parseSource(source: string, language: string): Tree | null {
  const parser = parserCache.get(language);
  if (!parser) return null;
  return parser.parse(source);
}
