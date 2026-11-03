/**
 * Terminal font management — font family building, ligature detection,
 * and WebGL renderer lifecycle.
 *
 * Inspired by orca's pane-webgl-renderer.ts and terminal-appearance.ts
 * patterns: the WebGL addon's glyph texture atlas is built at activation
 * time, so font changes require a full dispose → reattach cycle.
 */

import type { Terminal } from "@xterm/xterm";
import { WebglAddon } from "@xterm/addon-webgl";

// ── Font Family Building ────────────────────────────────────────────

// Cross-platform fallback chain. The user's chosen font comes first;
// these are appended so text always renders even if the chosen font
// isn't installed. Matches orca's FALLBACK_FONTS pattern.
const FALLBACK_FONTS = [
  "SF Mono",           // macOS 10.12+
  "Menlo",             // macOS (older)
  "Monaco",            // macOS (legacy)
  "Cascadia Mono",     // Windows 11+
  "Consolas",          // Windows Vista+
  "DejaVu Sans Mono",  // Linux (common)
  "Liberation Mono",   // Linux (common)
  "monospace",         // ultimate generic fallback
] as const;

/**
 * Build a full font-family CSS string from a single font name.
 * Quotes the primary font, then appends cross-platform fallbacks,
 * deduplicating case-insensitively.
 */
export function buildFontFamily(fontFamily: string): string {
  const trimmed = fontFamily.trim();
  const parts = trimmed ? [`"${trimmed}"`] : [];
  const lowerParts = parts.map((p) => p.toLowerCase());

  for (const fallback of FALLBACK_FONTS) {
    const lower = fallback.toLowerCase();
    if (!lowerParts.some((p) => p.includes(lower))) {
      // Generic keywords like "monospace" are unquoted; named fonts are quoted.
      parts.push(fallback === "monospace" ? fallback : `"${fallback}"`);
    }
  }
  return parts.join(", ");
}

// ── Font Availability Detection ─────────────────────────────────────

// Reuse a single canvas context for all font measurements.
let sharedCtx: CanvasRenderingContext2D | null = null;
function getCanvasCtx(): CanvasRenderingContext2D | null {
  if (!sharedCtx) {
    sharedCtx = document.createElement("canvas").getContext("2d");
  }
  return sharedCtx;
}

/**
 * Check whether a font is installed and available for rendering.
 *
 * Uses the canvas measurement technique: render a test string in the
 * target font with a known fallback, then in the fallback alone. If
 * the widths differ, the target font was used (it's installed).
 */
export function isFontAvailable(family: string): boolean {
  const ctx = getCanvasCtx();
  if (!ctx) return false;

  const testString = "mmmmmmmmmmlli10OQ@#$%";
  const fallbacks = ["monospace", "sans-serif"] as const;

  for (const fallback of fallbacks) {
    ctx.font = `72px ${fallback}`;
    const fallbackWidth = ctx.measureText(testString).width;

    ctx.font = `72px "${family}", ${fallback}`;
    const testWidth = ctx.measureText(testString).width;

    if (Math.abs(testWidth - fallbackWidth) > 0.5) {
      return true;
    }
  }
  return false;
}

/** Common monospace fonts that ship with macOS, Windows, or Linux. */
export const COMMON_MONO_FONTS = [
  // macOS
  "Menlo",
  "Monaco",
  "SF Mono",
  "Courier New",
  "Andale Mono",
  // Windows
  "Cascadia Mono",
  "Cascadia Code",
  "Consolas",
  "Lucida Console",
  // Linux / cross-platform
  "DejaVu Sans Mono",
  "Liberation Mono",
  "Ubuntu Mono",
  // Popular programmer fonts (may or may not be installed)
  "Fira Code",
  "Fira Mono",
  "JetBrains Mono",
  "Source Code Pro",
  "IBM Plex Mono",
  "Iosevka",
  "Victor Mono",
  "Hack",
  "Inconsolata",
  "Operator Mono",
  "Anonymous Pro",
  "PT Mono",
  "Noto Sans Mono",
  "Roboto Mono",
] as const;

/** Return the subset of COMMON_MONO_FONTS that are installed on this system. */
export function detectInstalledMonoFonts(): string[] {
  return COMMON_MONO_FONTS.filter((f) => isFontAvailable(f));
}

// ── Ligature Detection ──────────────────────────────────────────────

// Fonts that ship with programming ligatures out of the box.
const LIGATURE_FONT_TOKENS = [
  "fira code",
  "fira mono",
  "jetbrains mono",
  "jetbrainsmono",
  "cascadia code",
  "iosevka",
  "victor mono",
  "hasklig",
  "monoid",
  "operator mono",
  "dank mono",
  "mononoki",
  "pragmatapro",
  "recursive",
  "monolisa",
  "commit mono",
  "geist mono",
  "maple mono",
] as const;

/**
 * Whether a font name is a known ligature-capable programming font.
 * Used for auto-enabling ligatures when the user picks one of these.
 */
export function fontHasKnownLigatures(fontFamily: string): boolean {
  const lower = fontFamily.toLowerCase().trim();
  if (!lower) return false;
  return LIGATURE_FONT_TOKENS.some((token) => lower.includes(token));
}

// ── WebGL Renderer Lifecycle ────────────────────────────────────────

/** Stored reference to a WebGL addon for a terminal instance. */
export interface WebglState {
  addon: WebglAddon | null;
  /** Set after context loss — don't retry WebGL for this terminal. */
  disabledAfterContextLoss: boolean;
}

/** Whether WebGL should be used (disabled on Linux/Wayland due to atlas corruption). */
function shouldUseWebgl(): boolean {
  if (typeof navigator === "undefined") return true;
  // Linux/Wayland GPU stacks can corrupt xterm's WebGL glyph atlas
  // without raising context loss. Disable by default on Linux.
  if (navigator.platform.includes("Linux") || navigator.userAgent.includes("Linux")) {
    return false;
  }
  return true;
}

let webglGloballyFailed = false;

/**
 * Attach the WebGL renderer to a terminal. Returns the addon if
 * successful, null if WebGL is unavailable or disabled.
 */
export function attachWebgl(
  terminal: Terminal,
  state: WebglState,
): WebglAddon | null {
  if (webglGloballyFailed || state.disabledAfterContextLoss || !shouldUseWebgl()) {
    state.addon = null;
    return null;
  }

  try {
    const addon = new WebglAddon();
    addon.onContextLoss(() => {
      console.warn("[terminal] WebGL context lost — falling back to canvas renderer");
      state.disabledAfterContextLoss = true;
      disposeWebgl(terminal, state);
    });
    terminal.loadAddon(addon);
    state.addon = addon;
    // Repaint immediately so the terminal doesn't look frozen after attach.
    terminal.refresh(0, terminal.rows - 1);
    return addon;
  } catch (e) {
    console.warn("[terminal] WebGL renderer unavailable, using canvas fallback:", e);
    webglGloballyFailed = true;
    state.addon = null;
    return null;
  }
}

/**
 * Dispose the WebGL addon for a terminal. Safe to call if already
 * disposed or if no addon is attached.
 */
export function disposeWebgl(terminal: Terminal, state: WebglState): void {
  if (!state.addon) return;
  try {
    state.addon.dispose();
  } catch { /* already disposed */ }
  state.addon = null;
  // DOM and WebGL cell metrics differ; refit after teardown.
  requestAnimationFrame(() => {
    try {
      terminal.refresh(0, terminal.rows - 1);
    } catch { /* terminal may be disposed */ }
  });
}

/**
 * Rebuild the WebGL renderer. Used after font changes since the glyph
 * texture atlas is built at activation time and doesn't update when
 * terminal.options.fontFamily changes.
 */
export function rebuildWebgl(terminal: Terminal, state: WebglState): void {
  if (!state.addon) return; // No WebGL active, nothing to rebuild
  disposeWebgl(terminal, state);
  // Small delay to let the dispose settle before reattaching.
  attachWebgl(terminal, state);
}

// ── Font Config Application ─────────────────────────────────────────

export interface FontConfig {
  /** Primary font name (single name, not a CSS font-family stack). */
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  letterSpacing: number;
  ligatures: boolean;
  /** Font weight (100-900). Default 400 (normal). */
  fontWeight: number;
  /** Bold font weight (100-900). Default 700. */
  fontWeightBold: number;
}

const LS_FONT_KEY = "crab.termFont";

export const DEFAULT_FONT_CONFIG: FontConfig = {
  // SF Mono is the macOS default and renders beautifully; Menlo is the
  // universal fallback. The fallback chain in buildFontFamily() handles
  // platforms where SF Mono isn't installed.
  fontFamily: "SF Mono",
  fontSize: 13,
  // 1.3 is the sweet spot for xterm.js — 1.0 is cramped, 1.2 is too tight
  // for descenders, 1.5+ wastes vertical space.
  lineHeight: 1.3,
  letterSpacing: 0,
  ligatures: false,
  fontWeight: 500,
  fontWeightBold: 700,
};

export function loadFontConfig(): FontConfig {
  try {
    const raw = localStorage.getItem(LS_FONT_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      return {
        fontFamily: typeof parsed.fontFamily === "string" ? parsed.fontFamily : DEFAULT_FONT_CONFIG.fontFamily,
        fontSize: typeof parsed.fontSize === "number" ? Math.max(8, Math.min(32, parsed.fontSize)) : DEFAULT_FONT_CONFIG.fontSize,
        lineHeight: typeof parsed.lineHeight === "number" ? Math.max(1, Math.min(2.5, parsed.lineHeight)) : DEFAULT_FONT_CONFIG.lineHeight,
        letterSpacing: typeof parsed.letterSpacing === "number" ? Math.max(-2, Math.min(5, parsed.letterSpacing)) : DEFAULT_FONT_CONFIG.letterSpacing,
        ligatures: typeof parsed.ligatures === "boolean" ? parsed.ligatures : DEFAULT_FONT_CONFIG.ligatures,
        fontWeight: typeof parsed.fontWeight === "number" ? clampWeight(parsed.fontWeight) : DEFAULT_FONT_CONFIG.fontWeight,
        fontWeightBold: typeof parsed.fontWeightBold === "number" ? clampWeight(parsed.fontWeightBold) : DEFAULT_FONT_CONFIG.fontWeightBold,
      };
    }
  } catch { /* fall through */ }
  return { ...DEFAULT_FONT_CONFIG };
}

function clampWeight(w: number): number {
  return Math.max(100, Math.min(900, Math.round(w / 100) * 100));
}

export function saveFontConfig(config: FontConfig): void {
  localStorage.setItem(LS_FONT_KEY, JSON.stringify(config));
}

/**
 * Apply font config to an xterm instance. Sets all font-related options
 * and updates CSS font-feature-settings for ligatures.
 *
 * Does NOT rebuild WebGL — call `rebuildWebgl()` separately after this
 * when the font family has changed.
 */
export function applyFontConfig(terminal: Terminal, config: FontConfig): void {
  terminal.options.fontFamily = buildFontFamily(config.fontFamily);
  terminal.options.fontSize = config.fontSize;
  terminal.options.lineHeight = config.lineHeight;
  terminal.options.letterSpacing = config.letterSpacing;
  terminal.options.fontWeight = config.fontWeight;
  terminal.options.fontWeightBold = config.fontWeightBold;

  // CSS font-feature-settings for ligature control.
  const el = terminal.element;
  if (el) {
    el.style.fontFeatureSettings = config.ligatures
      ? '"liga" 1, "calt" 1'
      : '"liga" 0, "calt" 0';
  }

  // Clear the texture atlas so the canvas fallback renderer also picks
  // up the new font metrics.
  terminal.clearTextureAtlas();
  terminal.refresh(0, terminal.rows - 1);
}
