import { Language, Parser, Query, type Node } from "web-tree-sitter";
import treeSitterWasmUrl from "web-tree-sitter/web-tree-sitter.wasm?url";
import type {
  CodeSymbol,
  CodeSymbolLanguage,
  CodeSymbolRole,
} from "./code-symbols";

type ParseRequest = {
  requestId: number;
  language: CodeSymbolLanguage;
  source: string;
};

type LanguageBundle = {
  wasmUrl: string;
  querySource: string;
};

const languageLoaders: Record<
  CodeSymbolLanguage,
  () => Promise<LanguageBundle>
> = {
  c: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-c/tree-sitter-c.wasm?url"),
      import("tree-sitter-c/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  cpp: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-cpp/tree-sitter-cpp.wasm?url"),
      import("tree-sitter-cpp/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  go: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-go/tree-sitter-go.wasm?url"),
      import("tree-sitter-go/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  java: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-java/tree-sitter-java.wasm?url"),
      import("tree-sitter-java/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  python: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-python/tree-sitter-python.wasm?url"),
      import("tree-sitter-python/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  rust: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-rust/tree-sitter-rust.wasm?url"),
      import("tree-sitter-rust/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  javascript: async () => {
    const [wasm, tags] = await Promise.all([
      import("tree-sitter-javascript/tree-sitter-javascript.wasm?url"),
      import("tree-sitter-javascript/queries/tags.scm?raw"),
    ]);
    return { wasmUrl: wasm.default, querySource: tags.default };
  },
  typescript: async () => {
    const [wasm, javascriptTags, typescriptTags] = await Promise.all([
      import("tree-sitter-typescript/tree-sitter-tsx.wasm?url"),
      import("tree-sitter-javascript/queries/tags.scm?raw"),
      import("tree-sitter-typescript/queries/tags.scm?raw"),
    ]);
    return {
      wasmUrl: wasm.default,
      querySource: `${javascriptTags.default}\n${typescriptTags.default}`,
    };
  },
};

let initialized: Promise<void> | undefined;
const encoder = new TextEncoder();

function initializeParser() {
  initialized ??= Parser.init({ locateFile: () => treeSitterWasmUrl });
  return initialized;
}

function captureRole(
  name: string,
): { role: CodeSymbolRole; kind: string } | null {
  const [role, kind] = name.split(".");
  if ((role !== "definition" && role !== "reference") || !kind) return null;
  return { role, kind };
}

function containingName(node: Node, language: CodeSymbolLanguage) {
  let parent = node.parent;
  while (parent) {
    if (language === "rust") {
      if (parent.type === "impl_item") {
        const type = parent.childForFieldName("type")?.text;
        const trait = parent.childForFieldName("trait")?.text;
        if (type) return trait ? `impl ${trait} for ${type}` : `impl ${type}`;
      }
      if (parent.type === "trait_item") {
        const name = parent.childForFieldName("name")?.text;
        if (name) return `trait ${name}`;
      }
      if (parent.type === "mod_item") {
        const name = parent.childForFieldName("name")?.text;
        if (name) return `mod ${name}`;
      }
    } else if (
      [
        "class_declaration",
        "class",
        "class_definition",
        "class_specifier",
        "interface_declaration",
        "struct_specifier",
      ].includes(parent.type)
    ) {
      const name = parent.childForFieldName("name")?.text;
      if (name) return name;
    }
    parent = parent.parent;
  }
  return undefined;
}

function utf16Column(line: string, byteColumn: number) {
  let bytes = 0;
  let units = 0;
  for (const character of line) {
    const width = encoder.encode(character).length;
    if (bytes + width > byteColumn) break;
    bytes += width;
    units += character.length;
  }
  return units;
}

function kindPriority(kind: string) {
  if (kind === "method") return 2;
  if (kind === "function") return 1;
  return 0;
}

function extractSymbols(
  language: CodeSymbolLanguage,
  source: string,
  tree: ReturnType<Parser["parse"]> | undefined,
  query: Query,
) {
  if (!tree) return [];
  const lines = source.split("\n");
  const symbols = new Map<string, CodeSymbol>();
  for (const match of query.matches(tree.rootNode)) {
    const names = match.captures.filter((capture) => capture.name === "name");
    for (const capture of match.captures) {
      const metadata = captureRole(capture.name);
      if (!metadata) continue;
      const nameCapture = names.find(
        ({ node }) =>
          node.startIndex >= capture.node.startIndex &&
          node.endIndex <= capture.node.endIndex,
      );
      if (!nameCapture) continue;

      const node = nameCapture.node;
      const line = node.startPosition.row + 1;
      const column = utf16Column(
        lines[node.startPosition.row] ?? "",
        node.startPosition.column,
      );
      const endColumn = utf16Column(
        lines[node.endPosition.row] ?? "",
        node.endPosition.column,
      );
      const key = `${metadata.role}:${node.startIndex}:${node.endIndex}`;
      const symbol: CodeSymbol = {
        id: key,
        name: node.text,
        role: metadata.role,
        kind: metadata.kind,
        line,
        column,
        endColumn,
        container: containingName(capture.node, language),
      };
      const current = symbols.get(key);
      if (!current || kindPriority(symbol.kind) > kindPriority(current.kind)) {
        symbols.set(key, symbol);
      }
    }
  }
  return [...symbols.values()].sort(
    (left, right) =>
      left.line - right.line ||
      left.column - right.column ||
      Number(left.role === "reference") - Number(right.role === "reference"),
  );
}

self.onmessage = async (event: MessageEvent<ParseRequest>) => {
  const { requestId, language, source } = event.data;
  let parser: Parser | undefined;
  let tree: ReturnType<Parser["parse"]> | undefined;
  let query: Query | undefined;
  try {
    const bundle = await languageLoaders[language]();
    await initializeParser();
    const grammar = await Language.load(bundle.wasmUrl);
    parser = new Parser();
    parser.setLanguage(grammar);
    tree = parser.parse(source);
    query = new Query(grammar, bundle.querySource);
    self.postMessage({
      requestId,
      symbols: extractSymbols(language, source, tree, query),
    });
  } catch (error) {
    self.postMessage({
      requestId,
      error:
        error instanceof Error ? error.message : "Unable to parse symbols.",
    });
  } finally {
    query?.delete();
    tree?.delete();
    parser?.delete();
  }
};
