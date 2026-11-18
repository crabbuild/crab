import { Language, Parser, Query } from "web-tree-sitter";
import treeSitterWasmUrl from "web-tree-sitter/web-tree-sitter.wasm?url";
import type { CodeSymbolLanguage } from ".";
import { extractSymbols } from "./extract";

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

function initializeParser() {
  initialized ??= Parser.init({ locateFile: () => treeSitterWasmUrl });
  return initialized;
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
      symbols: extractSymbols(language, tree, query),
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
