import { describe, expect, it } from "vitest";
import {
  PRESET_LABEL_COLORS,
  contrastForeground,
  hexToHsv,
  hsvToHex,
  isValidHexColor,
  normalizeHexInput,
  relativeLuminance,
} from "./color";

function contrastRatio(firstHex: string, secondHex: string): number {
  const a = relativeLuminance(firstHex);
  const b = relativeLuminance(secondHex);
  const lighter = Math.max(a, b);
  const darker = Math.min(a, b);
  return (lighter + 0.05) / (darker + 0.05);
}

describe("label color utilities", () => {
  it("validates and normalizes 6-digit hex inputs", () => {
    expect(isValidHexColor("0969da")).toBe(true);
    expect(isValidHexColor("0969DA")).toBe(true);
    expect(isValidHexColor("0969d")).toBe(false);
    expect(isValidHexColor("#0969da")).toBe(false);
    expect(normalizeHexInput(" #3FB950 ")).toBe("3fb950");
  });

  it("round-trips standard and preset colors through HSV", () => {
    const cases = [
      "000000",
      "ffffff",
      "808080",
      "ff0000",
      "00ff00",
      "0000ff",
      ...PRESET_LABEL_COLORS.map((preset) => preset.hex),
    ];
    for (const hex of cases) {
      expect(hsvToHex(hexToHsv(hex))).toBe(hex);
    }
  });

  it("guarantees WCAG AA foreground contrast for all preset colors", () => {
    expect(PRESET_LABEL_COLORS).toHaveLength(14);
    for (const hex of [
      ...PRESET_LABEL_COLORS.map((preset) => preset.hex),
      "008672",
    ]) {
      const fg = contrastForeground(hex).slice(1);
      expect(
        contrastRatio(hex, fg),
        `Color #${hex} with #${fg}`,
      ).toBeGreaterThanOrEqual(4.5);
    }
  });
});
