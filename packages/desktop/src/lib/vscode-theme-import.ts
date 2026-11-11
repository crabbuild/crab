/**
 * VS Code theme import utility.
 *
 * Translates a documented subset of VS Code's workbench colors and
 * tokenColors into our semantic CSS variable format. Best-effort:
 * not every VS Code theme will map perfectly, but the most impactful
 * colors (background, foreground, accent, borders, diagnostics) are
 * covered.
 */

// ---- Types -----------------------------------------------------------

export interface DesktopTheme {
  id: string;
  name: string;
  colorScheme: "dark" | "light";
  colors: Record<string, string>; // key = "background", value = "15 17 21"
}

/** Minimal shape of a VS Code theme JSON we care about. */
interface VsCodeThemeJson {
  name?: string;
  type?: "dark" | "light" | "hc" | "hcLight";
  colors?: Record<string, string>;
  tokenColors?: unknown[];
}

// ---- Hex → RGB -------------------------------------------------------

/**
 * Converts a hex color string to an RGB triple suitable for CSS
 * variables: `"#4f8cff"` → `"79 140 255"`.
 *
 * Handles 3-char shorthand (`#abc` → `#aabbcc`), 6-char, and
 * 8-char (with alpha, which is discarded). Returns `null` for
 * invalid input.
 */
export function hexToRgbTriple(hex: string): string | null {
  if (typeof hex !== "string") return null;
  let h = hex.trim();
  if (!h.startsWith("#")) return null;
  h = h.slice(1);

  // Expand shorthand: #abc → aabbcc
  if (h.length === 3 || h.length === 4) {
    h = h
      .slice(0, 3)
      .split("")
      .map((c) => c + c)
      .join("");
  }

  // Strip alpha channel if present (#RRGGBBAA → RRGGBB)
  if (h.length === 8) h = h.slice(0, 6);

  if (h.length !== 6) return null;
  if (!/^[0-9a-fA-F]{6}$/.test(h)) return null;

  const r = parseInt(h.slice(0, 2), 16);
  const g = parseInt(h.slice(2, 4), 16);
  const b = parseInt(h.slice(4, 6), 16);
  return `${r} ${g} ${b}`;
}

// ---- Color scheme detection ------------------------------------------

function detectColorScheme(theme: VsCodeThemeJson): "dark" | "light" {
  if (theme.type === "light" || theme.type === "hcLight") return "light";
  if (theme.type === "dark" || theme.type === "hc") return "dark";

  // Heuristic: if editor.background is light, treat as light theme
  const bg = theme.colors?.["editor.background"];
  if (bg) {
    const rgb = hexToRgbTriple(bg);
    if (rgb) {
      const parts = rgb.split(" ").map(Number);
      const luminance = (parts[0] * 0.299 + parts[1] * 0.587 + parts[2] * 0.114) / 255;
      return luminance > 0.5 ? "light" : "dark";
    }
  }

  return "dark";
}

// ---- Theme name → ID -------------------------------------------------

function slugify(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48) || "imported-theme";
}

// ---- Mapping ---------------------------------------------------------

/**
 * Maps VS Code workbench color keys to our semantic token names.
 * Each entry is [token, ...vsCodeKeys] — we try keys in order and
 * use the first one that exists in the theme.
 */
const COLOR_MAP: Array<[string, ...string[]]> = [
  ["background", "editor.background"],
  ["card", "sideBar.background", "editorGroupHeader.tabsBackground"],
  ["sidebar", "activityBar.background", "titleBar.activeBackground"],
  ["foreground", "editor.foreground", "foreground"],
  ["primary", "focusBorder", "button.background", "activityBarBadge.background"],
  ["destructive", "editorError.foreground", "errorForeground"],
  ["warning", "editorWarning.foreground"],
  ["border", "panel.border", "editorGroup.border", "sideBar.border"],
  ["muted-foreground", "descriptionForeground", "editorLineNumber.foreground"],
  ["accent", "list.hoverBackground", "editorHoverWidget.background"],
  ["primary-hover", "button.hoverBackground"],
  ["success", "terminal.ansiGreen", "gitDecoration.addedResourceForeground"],
  ["info", "editorInfo.foreground", "terminal.ansiCyan"],
];

// ---- Main import function --------------------------------------------

/**
 * Parses a VS Code theme JSON and maps it to a `DesktopTheme`.
 *
 * Returns `null` if the input is not a recognizable VS Code theme
 * (missing `colors` object or no usable color mappings).
 */
export function importVsCodeTheme(themeJson: unknown): DesktopTheme | null {
  if (!themeJson || typeof themeJson !== "object") return null;

  const theme = themeJson as VsCodeThemeJson;

  // A VS Code theme must have a `colors` object with workbench colors
  if (!theme.colors || typeof theme.colors !== "object") return null;

  const colors: Record<string, string> = {};
  let mapped = 0;

  for (const [token, ...vsKeys] of COLOR_MAP) {
    for (const vsKey of vsKeys) {
      const hex = theme.colors[vsKey];
      if (hex) {
        const rgb = hexToRgbTriple(hex);
        if (rgb) {
          colors[token] = rgb;
          mapped++;
          break;
        }
      }
    }
  }

  // Require at least 3 mapped colors to consider this a valid import
  if (mapped < 3) return null;

  const name = typeof theme.name === "string" && theme.name.trim()
    ? theme.name.trim()
    : "Imported Theme";

  return {
    id: `vscode-${slugify(name)}`,
    name,
    colorScheme: detectColorScheme(theme),
    colors,
  };
}
