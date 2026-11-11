import { describe, expect, it } from "vitest";
import palettes from "../theme-tokens.json";
import { resolveTheme } from "../src/lib/terminal-themes";
import { importVsCodeTheme } from "../src/lib/vscode-theme-import";

function luminance(value: string) {
  const channels = value
    .split(" ")
    .map(Number)
    .map((v) => {
      const c = v / 255;
      return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
    });
  return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
}
function contrast(a: string, b: string) {
  const values = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (values[0] + 0.05) / (values[1] + 0.05);
}

describe.each(Object.entries(palettes))("%s palette", (_mode, palette) => {
  it("provides legible content and semantic action pairs", () => {
    const pairs = [
      "card",
      "popover",
      "primary",
      "secondary",
      "accent",
      "destructive",
      "success",
      "warning",
      "info",
      "thinking",
      "sidebar",
    ];
    const colors = palette as Record<string, string>;
    for (const role of pairs)
      expect(
        contrast(colors[role], colors[`${role}-foreground`]),
        role,
      ).toBeGreaterThanOrEqual(4.5);
    for (const surface of ["background", "card", "muted", "accent"]) {
      expect(
        contrast(colors.foreground, colors[surface]),
        `body on ${surface}`,
      ).toBeGreaterThanOrEqual(7);
      expect(
        contrast(colors["muted-foreground"], colors[surface]),
        `muted on ${surface}`,
      ).toBeGreaterThanOrEqual(4.5);
    }
  });
});

it.each(["dark", "light"] as const)(
  "default %s terminal uses app chrome and keeps ANSI output",
  (mode) => {
    const theme = resolveTheme("crab", mode);
    expect(theme.background).toBe(
      `rgb(${palettes[mode].background.replaceAll(" ", ", ")})`,
    );
    expect(theme.foreground).toBe(
      `rgb(${palettes[mode].foreground.replaceAll(" ", ", ")})`,
    );
    expect(
      new Set([theme.red, theme.green, theme.yellow, theme.blue]).size,
    ).toBe(4);
    expect(resolveTheme("solarized", mode).background).not.toBe(
      theme.background,
    );
  },
);

it("imports editor and sidebar colors without overwriting the editor background", () => {
  const theme = importVsCodeTheme({
    name: "Workbench",
    colors: {
      "editor.background": "#101010",
      "editor.foreground": "#eeeeee",
      "activityBar.background": "#202020",
      focusBorder: "#cccccc",
    },
  });
  expect(theme?.colors).toMatchObject({
    background: "16 16 16",
    sidebar: "32 32 32",
    foreground: "238 238 238",
    primary: "204 204 204",
  });
});
