import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { beforeAll, describe, expect, it } from "vitest";
import { Language, Parser, Query } from "web-tree-sitter";
import type { CodeSymbolLanguage } from ".";
import { extractSymbols } from "./extract";

const require = createRequire(import.meta.url);

beforeAll(async () => {
  await Parser.init();
});

async function symbolsFor(language: CodeSymbolLanguage, source: string) {
  const packageName = `tree-sitter-${language}`;
  const wasm =
    language === "typescript" ? "tree-sitter-tsx.wasm" : `${packageName}.wasm`;
  const grammar = await Language.load(
    require.resolve(`${packageName}/${wasm}`),
  );
  const tags = readFileSync(
    require.resolve(`${packageName}/queries/tags.scm`),
    "utf8",
  );
  const querySource =
    language === "typescript"
      ? readFileSync(
          require.resolve("tree-sitter-javascript/queries/tags.scm"),
          "utf8",
        ) + tags
      : tags;
  const parser = new Parser();
  parser.setLanguage(grammar);
  const tree = parser.parse(source);
  const query = new Query(grammar, querySource);
  try {
    return extractSymbols(language, tree, query);
  } finally {
    query.delete();
    tree?.delete();
    parser.delete();
  }
}

describe("current-file symbol extraction", () => {
  it.each<[CodeSymbolLanguage, string]>([
    [
      "c",
      "void target(void) {}\nvoid caller(void) { target(); void (*callback)(void) = target; }",
    ],
    [
      "cpp",
      "void target() {}\nvoid caller() { target(); auto callback = target; }",
    ],
    [
      "go",
      "package main\nfunc target() {}\nfunc caller() { target(); callback := target; _ = callback }",
    ],
    [
      "java",
      "class Repository { static void target() {} void caller() { target(); var callback = Repository::target; } }",
    ],
    [
      "javascript",
      "function target() {}\nfunction caller() { target(); const callback = target; }",
    ],
    [
      "python",
      "def target():\n    pass\n\ndef caller():\n    target()\n    callback = target",
    ],
    [
      "rust",
      "fn target() {}\nfn caller() { target(); let callback = target; }",
    ],
    [
      "typescript",
      "function target() {}\nfunction caller() { target(); const callback = target; }",
    ],
  ])(
    "includes calls and value uses in %s without duplicating declarations",
    async (language, source) => {
      const symbols = await symbolsFor(language, source);
      expect(
        symbols
          .filter((symbol) => symbol.name === "target")
          .map((symbol) => symbol.role),
      ).toEqual(["definition", "reference", "reference"]);
    },
  );

  it("excludes comments, strings and longer identifiers", async () => {
    const source =
      'fn target() {}\nfn caller() { let callback = target; let target_suffix = "target"; /* target */ }';
    const symbols = await symbolsFor("rust", source);
    expect(symbols.filter((symbol) => symbol.name === "target")).toHaveLength(
      2,
    );
  });

  it("keeps UTF-16 columns after multibyte text and emoji", async () => {
    const source = 'fn café() {}\nfn caller() { let text = "🦀é"; café(); }';
    const symbols = await symbolsFor("rust", source);
    const lines = source.split("\n");
    expect(
      symbols
        .filter((symbol) => symbol.name === "café")
        .map((symbol) =>
          lines[symbol.line - 1].slice(symbol.column, symbol.endColumn),
        ),
    ).toEqual(["café", "café"]);
  });

  it("counts a Go type declaration once and keeps its uses", async () => {
    const symbols = await symbolsFor(
      "go",
      "package main\ntype Repository struct{}\nvar repository Repository",
    );
    expect(
      symbols
        .filter((symbol) => symbol.name === "Repository")
        .map((symbol) => symbol.role),
    ).toEqual(["definition", "reference"]);
  });
});
