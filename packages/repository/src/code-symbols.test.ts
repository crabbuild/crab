import { describe, expect, it } from "vitest";
import {
  codeSymbolLanguage,
  findSymbolAt,
  type CodeSymbol,
} from "./code-symbols";

const definition: CodeSymbol = {
  id: "definition:4:8",
  name: "open",
  role: "definition",
  kind: "method",
  line: 3,
  column: 9,
  endColumn: 13,
};

describe("code symbols", () => {
  it.each([
    ["src/lib.rs", "rust"],
    ["src/app.tsx", "typescript"],
    ["src/module.mts", "typescript"],
    ["src/index.jsx", "javascript"],
    ["src/index.cjs", "javascript"],
    ["README.md", null],
  ])("selects the parser for %s", (name, language) => {
    expect(codeSymbolLanguage(name)).toBe(language);
  });

  it("matches a rendered token to its captured source range", () => {
    expect(findSymbolAt([definition], 3, 10, "pen")).toBe(definition);
    expect(findSymbolAt([definition], 4, 9, "open")).toBeUndefined();
  });
});
