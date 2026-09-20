import { describe, expect, it } from "vitest";
import {
  codeThemeChoices,
  codeThemeFrom,
  codeThemeNamesFor,
} from "./code-theme";

describe("code themes", () => {
  it("includes a light and dark highlighter for every choice", () => {
    for (const theme of Object.keys(codeThemeChoices)) {
      const names = codeThemeNamesFor(theme as keyof typeof codeThemeChoices);
      expect(names).toHaveLength(2);
      expect(names.every(Boolean)).toBe(true);
    }
  });

  it("falls back to GitHub for an unknown persisted value", () => {
    expect(codeThemeFrom("missing-theme")).toBe("github");
  });
});
