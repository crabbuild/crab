import { describe, expect, it } from "vitest";
import { codeSymbolLanguage } from ".";

describe("code symbols", () => {
  it.each([
    ["src/main.c", "c"],
    ["include/crab.h", "c"],
    ["src/main.cpp", "cpp"],
    ["include/crab.hpp", "cpp"],
    ["cmd/crab/main.go", "go"],
    ["src/main.java", "java"],
    ["src/lib.rs", "rust"],
    ["src/main.py", "python"],
    ["src/types.pyi", "python"],
    ["src/app.tsx", "typescript"],
    ["src/module.mts", "typescript"],
    ["src/index.jsx", "javascript"],
    ["src/index.cjs", "javascript"],
    ["README.md", null],
  ])("selects the parser for %s", (name, language) => {
    expect(codeSymbolLanguage(name)).toBe(language);
  });
});
