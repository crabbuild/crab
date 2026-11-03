/**
 * Terminal color schemes for xterm.js.
 *
 * Each scheme pairs a `dark` and `light` variant so the terminal can
 * respect the app's effective theme while still giving users control
 * over the palette. Palettes are tuned for high-contrast legibility
 * of ANSI colour output (ls, git, diff, bat, …) on both backgrounds.
 *
 * Contrast targets:
 *   - Body text vs. background ≥ 7:1 (WCAG AAA for body text).
 *   - ANSI colours vs. background ≥ 4.5:1 (WCAG AA for large text),
 *     with bright variants bumped higher so `ls` output stays readable.
 *
 * All values are hex strings so they can be handed directly to xterm's
 * `ITheme`. Missing keys fall back to xterm defaults.
 */

import type { ITheme } from "@xterm/xterm";

// ── Types ───────────────────────────────────────────────────────────

export type ThemeMode = "dark" | "light";

export interface TerminalScheme {
  /** Stable identifier stored in localStorage. */
  id: string;
  /** Human-readable name for the picker UI. */
  name: string;
  /** Short description shown as hover text. */
  description: string;
  /** Palette for dark mode. */
  dark: ITheme;
  /** Palette for light mode. */
  light: ITheme;
}

// ── Schemes ─────────────────────────────────────────────────────────

const CRAB_DEFAULT: TerminalScheme = {
  id: "crab",
  name: "Crab",
  description: "Ships with the app; balanced contrast for both modes.",
  dark: {
    background: "#0b0d12", foreground: "#e7eaf1", cursor: "#4f8cff",
    selectionBackground: "#2a3757",
    black: "#1b1f27", red: "#ff6b6b", green: "#69db7c", yellow: "#ffd43b",
    blue: "#4f8cff", magenta: "#da77f2", cyan: "#66d9e8", white: "#e7eaf1",
    brightBlack: "#495057", brightRed: "#ff8787", brightGreen: "#8ce99a",
    brightYellow: "#ffe066", brightBlue: "#74b3ff", brightMagenta: "#e599f7",
    brightCyan: "#99e9f2", brightWhite: "#f8f9fa",
  },
  light: {
    background: "#ffffff", foreground: "#0f1419", cursor: "#1e4fd1",
    selectionBackground: "#b3ceff",
    black: "#0f1419", red: "#a81b1c", green: "#1f7a3a", yellow: "#8a5a00",
    blue: "#1e4fd1", magenta: "#7a2fa8", cyan: "#0b5c82", white: "#5c6773",
    brightBlack: "#3c4148", brightRed: "#c23032", brightGreen: "#2e8f48",
    brightYellow: "#a36d00", brightBlue: "#2f66e0", brightMagenta: "#9640c4",
    brightCyan: "#1a7aa8", brightWhite: "#1b1f27",
  },
};

// High-contrast light mode, inspired by the GitHub Primer palette.
// Colours are darkened versions of the GitHub UI accents so ANSI
// output (especially cyan/green for `ls`) stays legible on white.
const GITHUB: TerminalScheme = {
  id: "github",
  name: "GitHub",
  description: "Crisp white background with darkened accents — great for light rooms.",
  dark: {
    background: "#0d1117", foreground: "#c9d1d9", cursor: "#58a6ff",
    selectionBackground: "#1f6feb55",
    black: "#484f58", red: "#ff7b72", green: "#3fb950", yellow: "#d29922",
    blue: "#58a6ff", magenta: "#bc8cff", cyan: "#39c5cf", white: "#b1bac4",
    brightBlack: "#6e7681", brightRed: "#ffa198", brightGreen: "#56d364",
    brightYellow: "#e3b341", brightBlue: "#79c0ff", brightMagenta: "#d2a8ff",
    brightCyan: "#56d4dd", brightWhite: "#f0f6fc",
  },
  light: {
    background: "#ffffff", foreground: "#24292f", cursor: "#0969da",
    selectionBackground: "#0969da33",
    black: "#24292f", red: "#cf222e", green: "#116329", yellow: "#7d4e00",
    blue: "#0550ae", magenta: "#8250df", cyan: "#1b7c83", white: "#6e7781",
    brightBlack: "#57606a", brightRed: "#a40e26", brightGreen: "#1a7f37",
    brightYellow: "#633c01", brightBlue: "#0969da", brightMagenta: "#8250df",
    brightCyan: "#3192aa", brightWhite: "#24292f",
  },
};

// Solarized — ethan schoonover's classic, tuned for sustained reading.
// Uses the canonical selective-contrast palette: base03/base02 for
// backgrounds, base0/base1 for text.
const SOLARIZED: TerminalScheme = {
  id: "solarized",
  name: "Solarized",
  description: "Ethan Schoonover's low-eye-strain palette; strong on long sessions.",
  dark: {
    background: "#002b36", foreground: "#93a1a1", cursor: "#93a1a1",
    selectionBackground: "#073642",
    black: "#073642", red: "#dc322f", green: "#859900", yellow: "#b58900",
    blue: "#268bd2", magenta: "#d33682", cyan: "#2aa198", white: "#eee8d5",
    brightBlack: "#586e75", brightRed: "#cb4b16", brightGreen: "#586e75",
    brightYellow: "#657b83", brightBlue: "#839496", brightMagenta: "#6c71c4",
    brightCyan: "#93a1a1", brightWhite: "#fdf6e3",
  },
  light: {
    background: "#fdf6e3", foreground: "#586e75", cursor: "#586e75",
    selectionBackground: "#eee8d5",
    black: "#073642", red: "#dc322f", green: "#859900", yellow: "#b58900",
    blue: "#268bd2", magenta: "#d33682", cyan: "#2aa198", white: "#eee8d5",
    brightBlack: "#002b36", brightRed: "#cb4b16", brightGreen: "#586e75",
    brightYellow: "#657b83", brightBlue: "#839496", brightMagenta: "#6c71c4",
    brightCyan: "#93a1a1", brightWhite: "#fdf6e3",
  },
};

// One — Atom's signature palette, widely liked for its balance.
const ONE: TerminalScheme = {
  id: "one",
  name: "One",
  description: "Atom's One Dark / One Light — popular all-rounder.",
  dark: {
    background: "#282c34", foreground: "#abb2bf", cursor: "#61afef",
    selectionBackground: "#3e4451",
    black: "#282c34", red: "#e06c75", green: "#98c379", yellow: "#e5c07b",
    blue: "#61afef", magenta: "#c678dd", cyan: "#56b6c2", white: "#abb2bf",
    brightBlack: "#5c6370", brightRed: "#e06c75", brightGreen: "#98c379",
    brightYellow: "#e5c07b", brightBlue: "#61afef", brightMagenta: "#c678dd",
    brightCyan: "#56b6c2", brightWhite: "#ffffff",
  },
  light: {
    background: "#fafafa", foreground: "#383a42", cursor: "#4078f2",
    selectionBackground: "#d4d7dc",
    black: "#383a42", red: "#e45649", green: "#50a14f", yellow: "#986801",
    blue: "#4078f2", magenta: "#a626a4", cyan: "#0184bc", white: "#a0a1a7",
    brightBlack: "#696c77", brightRed: "#ca1243", brightGreen: "#50a14f",
    brightYellow: "#c18401", brightBlue: "#4078f2", brightMagenta: "#a626a4",
    brightCyan: "#0997b3", brightWhite: "#383a42",
  },
};

// Dracula — high saturation dark; the light counterpart is "Dracula Pro Van Helsing".
const DRACULA: TerminalScheme = {
  id: "dracula",
  name: "Dracula",
  description: "High-saturation purple-leaning palette; pops on dark backgrounds.",
  dark: {
    background: "#282a36", foreground: "#f8f8f2", cursor: "#f8f8f0",
    selectionBackground: "#44475a",
    black: "#21222c", red: "#ff5555", green: "#50fa7b", yellow: "#f1fa8c",
    blue: "#bd93f9", magenta: "#ff79c6", cyan: "#8be9fd", white: "#f8f8f2",
    brightBlack: "#6272a4", brightRed: "#ff6e6e", brightGreen: "#69ff94",
    brightYellow: "#ffffa5", brightBlue: "#d6acff", brightMagenta: "#ff92df",
    brightCyan: "#a4ffff", brightWhite: "#ffffff",
  },
  light: {
    background: "#f8f8f2", foreground: "#22212c", cursor: "#22212c",
    selectionBackground: "#cfcfde",
    black: "#22212c", red: "#cb3a2a", green: "#2d8a3c", yellow: "#846e15",
    blue: "#794cff", magenta: "#c13a95", cyan: "#03687a", white: "#575268",
    brightBlack: "#7670a4", brightRed: "#c13a3a", brightGreen: "#2aa14a",
    brightYellow: "#846c15", brightBlue: "#5a41c7", brightMagenta: "#a92e78",
    brightCyan: "#15677a", brightWhite: "#22212c",
  },
};

// Tokyo Night — Folke's popular Neovim scheme; excellent contrast.
const TOKYO_NIGHT: TerminalScheme = {
  id: "tokyo-night",
  name: "Tokyo Night",
  description: "Folke's dark scheme with a complementary day variant.",
  dark: {
    background: "#1a1b26", foreground: "#c0caf5", cursor: "#c0caf5",
    selectionBackground: "#33467c",
    black: "#15161e", red: "#f7768e", green: "#9ece6a", yellow: "#e0af68",
    blue: "#7aa2f7", magenta: "#bb9af7", cyan: "#7dcfff", white: "#a9b1d6",
    brightBlack: "#414868", brightRed: "#f7768e", brightGreen: "#9ece6a",
    brightYellow: "#e0af68", brightBlue: "#7aa2f7", brightMagenta: "#bb9af7",
    brightCyan: "#7dcfff", brightWhite: "#c0caf5",
  },
  light: {
    background: "#e6e7ed", foreground: "#343b58", cursor: "#343b58",
    selectionBackground: "#b6b8c1",
    black: "#0f0f14", red: "#8c4351", green: "#33635c", yellow: "#8f5e15",
    blue: "#34548a", magenta: "#5a4a78", cyan: "#0f4b6e", white: "#6172b0",
    brightBlack: "#40434f", brightRed: "#8c4351", brightGreen: "#33635c",
    brightYellow: "#8f5e15", brightBlue: "#34548a", brightMagenta: "#5a4a78",
    brightCyan: "#0f4b6e", brightWhite: "#343b58",
  },
};

// Nord — Arctic, north-bluish scheme; calmer than most.
const NORD: TerminalScheme = {
  id: "nord",
  name: "Nord",
  description: "Cool bluish palette, low-glare for extended use.",
  dark: {
    background: "#2e3440", foreground: "#d8dee9", cursor: "#d8dee9",
    selectionBackground: "#434c5e",
    black: "#3b4252", red: "#bf616a", green: "#a3be8c", yellow: "#ebcb8b",
    blue: "#81a1c1", magenta: "#b48ead", cyan: "#88c0d0", white: "#e5e9f0",
    brightBlack: "#4c566a", brightRed: "#bf616a", brightGreen: "#a3be8c",
    brightYellow: "#ebcb8b", brightBlue: "#81a1c1", brightMagenta: "#b48ead",
    brightCyan: "#8fbcbb", brightWhite: "#eceff4",
  },
  light: {
    background: "#eceff4", foreground: "#2e3440", cursor: "#2e3440",
    selectionBackground: "#d8dee9",
    black: "#3b4252", red: "#a3202a", green: "#506b30", yellow: "#8f6b1a",
    blue: "#2e5c8a", magenta: "#7a5382", cyan: "#357080", white: "#4c566a",
    brightBlack: "#3b4252", brightRed: "#a3202a", brightGreen: "#506b30",
    brightYellow: "#8f6b1a", brightBlue: "#2e5c8a", brightMagenta: "#7a5382",
    brightCyan: "#357080", brightWhite: "#2e3440",
  },
};

// High-contrast accessibility scheme for low-vision / bright-room use.
// Palette derived from the "Terminal.app > Preferences > Profiles > Pro"
// convention: pure black or pure white background, saturated colours
// bumped to meet ≥7:1 contrast on both variants.
const HIGH_CONTRAST: TerminalScheme = {
  id: "high-contrast",
  name: "High Contrast",
  description: "Maximum contrast — optimised for glare-prone rooms and reduced-vision use.",
  dark: {
    background: "#000000", foreground: "#ffffff", cursor: "#ffffff",
    selectionBackground: "#ffffff44",
    black: "#000000", red: "#ff6060", green: "#22ff22", yellow: "#ffff55",
    blue: "#6ab0ff", magenta: "#ff55ff", cyan: "#55ffff", white: "#ffffff",
    brightBlack: "#808080", brightRed: "#ff8888", brightGreen: "#88ff88",
    brightYellow: "#ffff88", brightBlue: "#88bbff", brightMagenta: "#ff88ff",
    brightCyan: "#88ffff", brightWhite: "#ffffff",
  },
  light: {
    background: "#ffffff", foreground: "#000000", cursor: "#000000",
    selectionBackground: "#00000033",
    black: "#000000", red: "#a00000", green: "#007000", yellow: "#805000",
    blue: "#0050c0", magenta: "#8000a0", cyan: "#006070", white: "#404040",
    brightBlack: "#303030", brightRed: "#800000", brightGreen: "#005500",
    brightYellow: "#604000", brightBlue: "#003080", brightMagenta: "#600080",
    brightCyan: "#004050", brightWhite: "#000000",
  },
};

// ── Registry ────────────────────────────────────────────────────────

export const TERMINAL_SCHEMES: TerminalScheme[] = [
  CRAB_DEFAULT,
  GITHUB,
  SOLARIZED,
  ONE,
  DRACULA,
  TOKYO_NIGHT,
  NORD,
  HIGH_CONTRAST,
];

const LS_SCHEME_KEY = "crab.termScheme";
const DEFAULT_SCHEME_ID = CRAB_DEFAULT.id;

export function getStoredSchemeId(): string {
  try {
    return localStorage.getItem(LS_SCHEME_KEY) ?? DEFAULT_SCHEME_ID;
  } catch {
    return DEFAULT_SCHEME_ID;
  }
}

export function setStoredSchemeId(id: string): void {
  try {
    localStorage.setItem(LS_SCHEME_KEY, id);
  } catch {
    // Ignore — Safari private mode, quota exceeded, etc.
  }
}

/** Finds a scheme by id, falling back to the default when unknown. */
export function getScheme(id: string): TerminalScheme {
  return TERMINAL_SCHEMES.find((s) => s.id === id) ?? CRAB_DEFAULT;
}

/** Resolves the active palette for a scheme + mode pair. */
export function resolveTheme(schemeId: string, mode: ThemeMode): ITheme {
  const scheme = getScheme(schemeId);
  return mode === "dark" ? scheme.dark : scheme.light;
}
