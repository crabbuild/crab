// Tests for the VS Code theme import utility.
//
// Validates:
//   1. hexToRgbTriple converts hex colors correctly.
//   2. importVsCodeTheme maps VS Code workbench colors to DesktopTheme.
//   3. Color scheme detection (dark/light).
//   4. Fallback key resolution.
//   5. Invalid input handling.
//   6. ID slugification.
//   7. warn/warning alias sync.
//
// Run via: npx tsx src/lib/vscode-theme-import.test.ts

import { hexToRgbTriple, importVsCodeTheme } from "./vscode-theme-import";

/* eslint-disable no-console */

function assertEq<T>(actual: T, expected: T, msg: string): void {
  if (actual !== expected) {
    console.error(
      `FAIL: ${msg}\n  expected: ${JSON.stringify(expected)}\n  actual:   ${JSON.stringify(actual)}`,
    );
    process.exit(1);
  }
}

function assertNotNull<T>(value: T | null, msg: string): asserts value is T {
  if (value === null) {
    console.error(`FAIL: ${msg}\n  expected non-null, got null`);
    process.exit(1);
  }
}

function assertNull(value: unknown, msg: string): void {
  if (value !== null) {
    console.error(`FAIL: ${msg}\n  expected null, got: ${JSON.stringify(value)}`);
    process.exit(1);
  }
}

// ---------- hexToRgbTriple: standard 6-char hex ----------

{
  assertEq(hexToRgbTriple("#4f8cff"), "79 140 255", "6-char hex → RGB triple");
  assertEq(hexToRgbTriple("#000000"), "0 0 0", "black");
  assertEq(hexToRgbTriple("#ffffff"), "255 255 255", "white");
  assertEq(hexToRgbTriple("#FF0000"), "255 0 0", "uppercase hex");
}

// ---------- hexToRgbTriple: 3-char shorthand ----------

{
  assertEq(hexToRgbTriple("#abc"), "170 187 204", "3-char shorthand");
  assertEq(hexToRgbTriple("#fff"), "255 255 255", "3-char white");
}

// ---------- hexToRgbTriple: 8-char with alpha ----------

{
  assertEq(hexToRgbTriple("#4f8cff80"), "79 140 255", "8-char strips alpha");
  assertEq(hexToRgbTriple("#000000ff"), "0 0 0", "8-char full alpha");
}

// ---------- hexToRgbTriple: whitespace trimming ----------

{
  assertEq(hexToRgbTriple("  #ff0000  "), "255 0 0", "trims whitespace");
}

// ---------- hexToRgbTriple: invalid input ----------

{
  assertNull(hexToRgbTriple(""), "empty string → null");
  assertNull(hexToRgbTriple("not-a-color"), "non-hex → null");
  assertNull(hexToRgbTriple("#xyz"), "invalid chars → null");
  assertNull(hexToRgbTriple("#12"), "too short → null");
  assertNull(hexToRgbTriple("#1234567890"), "too long → null");
}

console.log("hexToRgbTriple: OK");

// ---------- importVsCodeTheme: valid dark theme ----------

{
  const vsTheme = {
    name: "My Dark Theme",
    type: "dark" as const,
    colors: {
      "editor.background": "#1e1e2e",
      "editor.foreground": "#cdd6f4",
      "sideBar.background": "#181825",
      "activityBar.background": "#11111b",
      "focusBorder": "#89b4fa",
      "editorError.foreground": "#f38ba8",
      "editorWarning.foreground": "#f9e2af",
      "panel.border": "#313244",
      "descriptionForeground": "#a6adc8",
    },
  };

  const result = importVsCodeTheme(vsTheme);
  assertNotNull(result, "valid theme returns non-null");
  assertEq(result.id, "vscode-my-dark-theme", "ID slugified");
  assertEq(result.name, "My Dark Theme", "name preserved");
  assertEq(result.colorScheme, "dark", "dark scheme detected");
  assertEq(result.colors["background"], "30 30 46", "editor.background → background");
  assertEq(result.colors["foreground"], "205 214 244", "editor.foreground → foreground");
  assertEq(result.colors["card"], "24 24 37", "sideBar.background → card");
  assertEq(result.colors["sidebar"], "17 17 27", "activityBar.background → sidebar");
  assertEq(result.colors["primary"], "137 180 250", "focusBorder → primary");
  assertEq(result.colors["destructive"], "243 139 168", "editorError.foreground → destructive");
  assertEq(result.colors["warning"], "249 226 175", "editorWarning.foreground → warning");
  assertEq(result.colors["border"], "49 50 68", "panel.border → border");
  assertEq(result.colors["muted-foreground"], "166 173 200", "descriptionForeground → muted-foreground");
}

// ---------- importVsCodeTheme: light theme detection ----------

{
  const lightTheme = {
    name: "Light Theme",
    type: "light" as const,
    colors: {
      "editor.background": "#ffffff",
      "editor.foreground": "#333333",
      "sideBar.background": "#f0f0f0",
      "focusBorder": "#0066cc",
    },
  };
  const result = importVsCodeTheme(lightTheme);
  assertNotNull(result, "light theme returns non-null");
  assertEq(result.colorScheme, "light", "type=light → light scheme");
}

// ---------- importVsCodeTheme: luminance-based detection ----------

{
  const inferredLight = {
    name: "Inferred Light",
    colors: {
      "editor.background": "#fafafa",
      "editor.foreground": "#333333",
      "sideBar.background": "#f0f0f0",
      "focusBorder": "#0066cc",
    },
  };
  const result = importVsCodeTheme(inferredLight);
  assertNotNull(result, "inferred light returns non-null");
  assertEq(result.colorScheme, "light", "bright background → light scheme");
}

// ---------- importVsCodeTheme: null for invalid input ----------

{
  assertNull(importVsCodeTheme(null), "null input → null");
  assertNull(importVsCodeTheme(undefined), "undefined input → null");
  assertNull(importVsCodeTheme("string"), "string input → null");
  assertNull(importVsCodeTheme(42), "number input → null");
  assertNull(importVsCodeTheme({ name: "No Colors" }), "missing colors → null");
  assertNull(importVsCodeTheme({ name: "Empty", colors: {} }), "empty colors → null");
}

// ---------- importVsCodeTheme: too few mappable colors ----------

{
  const sparse = {
    name: "Sparse",
    colors: {
      "editor.background": "#1e1e2e",
      "some.unknown.key": "#ff0000",
    },
  };
  assertNull(importVsCodeTheme(sparse), "fewer than 3 mapped → null");
}

// ---------- importVsCodeTheme: fallback keys ----------

{
  const theme = {
    name: "Fallback Test",
    type: "dark" as const,
    colors: {
      "editor.background": "#1e1e2e",
      "foreground": "#cccccc",
      "editorGroupHeader.tabsBackground": "#2d2d2d",
      "button.background": "#007acc",
    },
  };
  const result = importVsCodeTheme(theme);
  assertNotNull(result, "fallback keys produce valid theme");
  assertEq(result.colors["foreground"], "204 204 204", "foreground fallback for editor.foreground");
  assertEq(result.colors["card"], "45 45 45", "tabsBackground fallback for sideBar.background");
  assertEq(result.colors["primary"], "0 122 204", "button.background fallback for focusBorder");
}

// ---------- importVsCodeTheme: ID slugification ----------

{
  const theme = {
    name: "One Dark Pro (Vivid)",
    type: "dark" as const,
    colors: {
      "editor.background": "#282c34",
      "editor.foreground": "#abb2bf",
      "sideBar.background": "#21252b",
      "focusBorder": "#528bff",
    },
  };
  const result = importVsCodeTheme(theme);
  assertNotNull(result, "theme with special chars in name");
  assertEq(result.id, "vscode-one-dark-pro-vivid", "special chars slugified");
}

// ---------- importVsCodeTheme: empty name fallback ----------

{
  const theme = {
    name: "",
    type: "dark" as const,
    colors: {
      "editor.background": "#1e1e2e",
      "editor.foreground": "#cdd6f4",
      "sideBar.background": "#181825",
      "focusBorder": "#89b4fa",
    },
  };
  const result = importVsCodeTheme(theme);
  assertNotNull(result, "empty name still produces theme");
  assertEq(result.name, "Imported Theme", "empty name → default name");
  assertEq(result.id, "vscode-imported-theme", "empty name → default ID");
}

console.log("importVsCodeTheme: OK");
console.log("vscode-theme-import: ALL TESTS PASSED");
