// Symbol tree parser with LRU cache keyed by (path, mtime, rev).
//
// Builds on the tree-sitter module (task 37.1) to extract language-aware
// symbol trees from source code. Used by the breadcrumb bar and outline view.

import type { SyntaxNode } from "web-tree-sitter";
import { getLanguageForFile, getParser, parseSource } from "./tree-sitter";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type SymbolKind =
  | "function"
  | "method"
  | "class"
  | "interface"
  | "enum"
  | "variable"
  | "constant"
  | "module"
  | "struct"
  | "trait"
  | "impl";

export interface SymbolNode {
  name: string;
  kind: SymbolKind;
  range: { startLine: number; endLine: number };
  children: SymbolNode[];
}

interface CacheKey {
  path: string;
  mtime: number; // Unix timestamp ms
  rev: string | null;
}

// ---------------------------------------------------------------------------
// LRU Cache
// ---------------------------------------------------------------------------

const MAX_CACHE_ENTRIES = 50;

interface CacheEntry {
  key: CacheKey;
  symbols: SymbolNode[];
}

// Ordered from least-recently-used (index 0) to most-recently-used (end).
const cache: CacheEntry[] = [];

function cacheKeyMatches(a: CacheKey, b: CacheKey): boolean {
  return a.path === b.path && a.mtime === b.mtime && a.rev === b.rev;
}

function cacheLookup(key: CacheKey): SymbolNode[] | null {
  const idx = cache.findIndex((e) => cacheKeyMatches(e.key, key));
  if (idx === -1) return null;
  // Move to end (most recently used).
  const [entry] = cache.splice(idx, 1);
  cache.push(entry);
  return entry.symbols;
}

function cacheInsert(key: CacheKey, symbols: SymbolNode[]): void {
  // Evict LRU if at capacity.
  if (cache.length >= MAX_CACHE_ENTRIES) {
    cache.shift();
  }
  cache.push({ key, symbols });
}

// ---------------------------------------------------------------------------
// Symbol extraction per language
// ---------------------------------------------------------------------------

function nameFromChild(node: SyntaxNode, type: string): string | null {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (child && child.type === type) return child.text;
  }
  return null;
}

function nodeRange(node: SyntaxNode): { startLine: number; endLine: number } {
  return {
    startLine: node.startPosition.row,
    endLine: node.endPosition.row,
  };
}

// --- TypeScript / JavaScript ---

function extractTypeScript(root: SyntaxNode): SymbolNode[] {
  const symbols: SymbolNode[] = [];
  walkTS(root, symbols);
  return symbols;
}

function walkTS(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child) continue;

    switch (child.type) {
      case "function_declaration": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "function", range: nodeRange(child), children: [] };
          walkTS(child, sym.children);
          out.push(sym);
        }
        break;
      }
      case "class_declaration": {
        const name = nameFromChild(child, "type_identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "class", range: nodeRange(child), children: [] };
          extractClassMembers(child, sym.children);
          out.push(sym);
        }
        break;
      }
      case "interface_declaration": {
        const name = nameFromChild(child, "type_identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "interface", range: nodeRange(child), children: [] };
          out.push(sym);
        }
        break;
      }
      case "enum_declaration": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "enum", range: nodeRange(child), children: [] };
          out.push(sym);
        }
        break;
      }
      case "export_statement": {
        // Recurse into the exported declaration.
        walkTS(child, out);
        break;
      }
      case "lexical_declaration": {
        // const/let/var at top level → variable symbols.
        extractLexicalDeclaration(child, out);
        break;
      }
      default:
        break;
    }
  }
}

function extractLexicalDeclaration(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child || child.type !== "variable_declarator") continue;
    const name = nameFromChild(child, "identifier");
    if (name) {
      // Determine if it's a const (constant) or let/var (variable).
      const declKind = node.child(0)?.text;
      const kind: SymbolKind = declKind === "const" ? "constant" : "variable";
      out.push({ name, kind, range: nodeRange(node), children: [] });
    }
  }
}

function extractClassMembers(classNode: SyntaxNode, out: SymbolNode[]): void {
  const body = classNode.childForFieldName("body");
  if (!body) return;
  for (let i = 0; i < body.childCount; i++) {
    const member = body.child(i);
    if (!member) continue;
    if (
      member.type === "method_definition" ||
      member.type === "public_field_definition"
    ) {
      const name = nameFromChild(member, "property_identifier");
      if (name) {
        out.push({ name, kind: "method", range: nodeRange(member), children: [] });
      }
    }
  }
}

// --- Python ---

function extractPython(root: SyntaxNode): SymbolNode[] {
  const symbols: SymbolNode[] = [];
  walkPython(root, symbols);
  return symbols;
}

function walkPython(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child) continue;

    switch (child.type) {
      case "function_definition": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "function", range: nodeRange(child), children: [] };
          walkPython(child, sym.children);
          out.push(sym);
        }
        break;
      }
      case "class_definition": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "class", range: nodeRange(child), children: [] };
          extractPythonClassMembers(child, sym.children);
          out.push(sym);
        }
        break;
      }
      default:
        break;
    }
  }
}

function extractPythonClassMembers(classNode: SyntaxNode, out: SymbolNode[]): void {
  const body = classNode.childForFieldName("body");
  if (!body) return;
  for (let i = 0; i < body.childCount; i++) {
    const member = body.child(i);
    if (!member) continue;
    if (member.type === "function_definition") {
      const name = nameFromChild(member, "identifier");
      if (name) {
        out.push({ name, kind: "method", range: nodeRange(member), children: [] });
      }
    }
  }
}

// --- Rust ---

function extractRust(root: SyntaxNode): SymbolNode[] {
  const symbols: SymbolNode[] = [];
  walkRust(root, symbols);
  return symbols;
}

function walkRust(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child) continue;

    switch (child.type) {
      case "function_item": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "function", range: nodeRange(child), children: [] };
          out.push(sym);
        }
        break;
      }
      case "struct_item": {
        const name = nameFromChild(child, "type_identifier");
        if (name) {
          out.push({ name, kind: "struct", range: nodeRange(child), children: [] });
        }
        break;
      }
      case "enum_item": {
        const name = nameFromChild(child, "type_identifier");
        if (name) {
          out.push({ name, kind: "enum", range: nodeRange(child), children: [] });
        }
        break;
      }
      case "trait_item": {
        const name = nameFromChild(child, "type_identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "trait", range: nodeRange(child), children: [] };
          extractRustImplOrTraitMembers(child, sym.children);
          out.push(sym);
        }
        break;
      }
      case "impl_item": {
        const name = nameFromChild(child, "type_identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "impl", range: nodeRange(child), children: [] };
          extractRustImplOrTraitMembers(child, sym.children);
          out.push(sym);
        }
        break;
      }
      case "mod_item": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "module", range: nodeRange(child), children: [] };
          walkRust(child, sym.children);
          out.push(sym);
        }
        break;
      }
      default:
        break;
    }
  }
}

function extractRustImplOrTraitMembers(node: SyntaxNode, out: SymbolNode[]): void {
  const body = node.childForFieldName("body");
  if (!body) return;
  for (let i = 0; i < body.childCount; i++) {
    const member = body.child(i);
    if (!member) continue;
    if (member.type === "function_item") {
      const name = nameFromChild(member, "identifier");
      if (name) {
        out.push({ name, kind: "method", range: nodeRange(member), children: [] });
      }
    }
  }
}

// --- Go ---

function extractGo(root: SyntaxNode): SymbolNode[] {
  const symbols: SymbolNode[] = [];
  walkGo(root, symbols);
  return symbols;
}

function walkGo(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child) continue;

    switch (child.type) {
      case "function_declaration": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          out.push({ name, kind: "function", range: nodeRange(child), children: [] });
        }
        break;
      }
      case "method_declaration": {
        const name = nameFromChild(child, "field_identifier");
        if (name) {
          out.push({ name, kind: "method", range: nodeRange(child), children: [] });
        }
        break;
      }
      case "type_declaration": {
        extractGoTypeDecl(child, out);
        break;
      }
      default:
        break;
    }
  }
}

function extractGoTypeDecl(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child || child.type !== "type_spec") continue;
    const name = nameFromChild(child, "type_identifier");
    if (!name) continue;
    // Determine if it's a struct or interface.
    const typeBody = child.childForFieldName("type");
    if (typeBody?.type === "struct_type") {
      out.push({ name, kind: "struct", range: nodeRange(child), children: [] });
    } else if (typeBody?.type === "interface_type") {
      out.push({ name, kind: "interface", range: nodeRange(child), children: [] });
    } else {
      out.push({ name, kind: "variable", range: nodeRange(child), children: [] });
    }
  }
}

// --- Java ---

function extractJava(root: SyntaxNode): SymbolNode[] {
  const symbols: SymbolNode[] = [];
  walkJava(root, symbols);
  return symbols;
}

function walkJava(node: SyntaxNode, out: SymbolNode[]): void {
  for (let i = 0; i < node.childCount; i++) {
    const child = node.child(i);
    if (!child) continue;

    switch (child.type) {
      case "class_declaration": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "class", range: nodeRange(child), children: [] };
          extractJavaClassMembers(child, sym.children);
          out.push(sym);
        }
        break;
      }
      case "interface_declaration": {
        const name = nameFromChild(child, "identifier");
        if (name) {
          const sym: SymbolNode = { name, kind: "interface", range: nodeRange(child), children: [] };
          out.push(sym);
        }
        break;
      }
      case "program": {
        walkJava(child, out);
        break;
      }
      default:
        break;
    }
  }
}

function extractJavaClassMembers(classNode: SyntaxNode, out: SymbolNode[]): void {
  const body = classNode.childForFieldName("body");
  if (!body) return;
  for (let i = 0; i < body.childCount; i++) {
    const member = body.child(i);
    if (!member) continue;
    if (member.type === "method_declaration") {
      const name = nameFromChild(member, "identifier");
      if (name) {
        out.push({ name, kind: "method", range: nodeRange(member), children: [] });
      }
    } else if (member.type === "class_declaration") {
      const name = nameFromChild(member, "identifier");
      if (name) {
        const sym: SymbolNode = { name, kind: "class", range: nodeRange(member), children: [] };
        extractJavaClassMembers(member, sym.children);
        out.push(sym);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Language → extractor dispatch
// ---------------------------------------------------------------------------

const EXTRACTORS: Record<string, (root: SyntaxNode) => SymbolNode[]> = {
  typescript: extractTypeScript,
  python: extractPython,
  rust: extractRust,
  go: extractGo,
  java: extractJava,
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Parse source code and return a symbol tree.
 *
 * Results are cached by (path, mtime, rev). If the cache key matches a
 * previous call, the cached tree is returned without re-parsing.
 */
export async function getSymbolTree(
  source: string,
  path: string,
  mtime: number,
  rev: string | null,
): Promise<SymbolNode[]> {
  const key: CacheKey = { path, mtime, rev };

  const cached = cacheLookup(key);
  if (cached) return cached;

  const language = getLanguageForFile(path);
  if (!language) return [];

  // Ensure the parser is initialized (async WASM load on first call).
  const parser = await getParser(language);
  if (!parser) return [];

  const tree = parseSource(source, language);
  if (!tree) return [];

  const extractor = EXTRACTORS[language];
  if (!extractor) return [];

  const symbols = extractor(tree.rootNode);
  cacheInsert(key, symbols);
  return symbols;
}

/**
 * Walk the symbol tree and return the chain of ancestors containing the
 * given line number. Used for breadcrumb rendering.
 *
 * Example: at line 42 inside `class Foo > method bar`, returns `[Foo, bar]`.
 */
export function getAncestorChain(
  symbols: SymbolNode[],
  line: number,
): SymbolNode[] {
  const chain: SymbolNode[] = [];
  findAncestors(symbols, line, chain);
  return chain;
}

function findAncestors(
  nodes: SymbolNode[],
  line: number,
  chain: SymbolNode[],
): boolean {
  for (const node of nodes) {
    if (line >= node.range.startLine && line <= node.range.endLine) {
      chain.push(node);
      // Try to go deeper into children.
      findAncestors(node.children, line, chain);
      return true;
    }
  }
  return false;
}
