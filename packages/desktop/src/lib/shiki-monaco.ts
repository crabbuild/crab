/**
 * Shiki ↔ Monaco integration — all themes bundled eagerly.
 *
 * Since this is a desktop app (Electron), we bundle ALL themes and
 * hot languages statically. No network requests, no dynamic imports
 * that could fail in asar. Everything is available instantly from disk.
 */
import { createHighlighterCore } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import { shikiToMonaco } from "@shikijs/monaco";
import * as monaco from "monaco-editor";
import type { HighlighterCore } from "shiki/core";

// ─── ALL themes (statically imported, bundled into the app) ─────────
const ALL_THEME_IMPORTS = [
  import("@shikijs/themes/github-dark"),
  import("@shikijs/themes/github-light"),
  import("@shikijs/themes/github-dark-default"),
  import("@shikijs/themes/github-dark-dimmed"),
  import("@shikijs/themes/github-dark-high-contrast"),
  import("@shikijs/themes/github-light-default"),
  import("@shikijs/themes/github-light-high-contrast"),
  import("@shikijs/themes/one-dark-pro"),
  import("@shikijs/themes/one-light"),
  import("@shikijs/themes/dracula"),
  import("@shikijs/themes/dracula-soft"),
  import("@shikijs/themes/monokai"),
  import("@shikijs/themes/nord"),
  import("@shikijs/themes/night-owl"),
  import("@shikijs/themes/night-owl-light"),
  import("@shikijs/themes/tokyo-night"),
  import("@shikijs/themes/vitesse-dark"),
  import("@shikijs/themes/vitesse-black"),
  import("@shikijs/themes/vitesse-light"),
  import("@shikijs/themes/catppuccin-mocha"),
  import("@shikijs/themes/catppuccin-macchiato"),
  import("@shikijs/themes/catppuccin-frappe"),
  import("@shikijs/themes/catppuccin-latte"),
  import("@shikijs/themes/rose-pine"),
  import("@shikijs/themes/rose-pine-moon"),
  import("@shikijs/themes/rose-pine-dawn"),
  import("@shikijs/themes/material-theme"),
  import("@shikijs/themes/material-theme-ocean"),
  import("@shikijs/themes/material-theme-palenight"),
  import("@shikijs/themes/material-theme-lighter"),
  import("@shikijs/themes/material-theme-darker"),
  import("@shikijs/themes/solarized-dark"),
  import("@shikijs/themes/solarized-light"),
  import("@shikijs/themes/dark-plus"),
  import("@shikijs/themes/light-plus"),
  import("@shikijs/themes/ayu-dark"),
  import("@shikijs/themes/ayu-light"),
  import("@shikijs/themes/everforest-dark"),
  import("@shikijs/themes/everforest-light"),
  import("@shikijs/themes/gruvbox-dark-medium"),
  import("@shikijs/themes/gruvbox-light-medium"),
  import("@shikijs/themes/poimandres"),
  import("@shikijs/themes/synthwave-84"),
  import("@shikijs/themes/houston"),
  import("@shikijs/themes/kanagawa-wave"),
  import("@shikijs/themes/kanagawa-lotus"),
  import("@shikijs/themes/slack-dark"),
  import("@shikijs/themes/slack-ochin"),
  import("@shikijs/themes/snazzy-light"),
  import("@shikijs/themes/min-dark"),
  import("@shikijs/themes/min-light"),
];

// ─── Hot languages (loaded eagerly) ─────────────────────────────────
const HOT_LANG_IMPORTS = [
  import("@shikijs/langs/javascript"),
  import("@shikijs/langs/typescript"),
  import("@shikijs/langs/tsx"),
  import("@shikijs/langs/json"),
  import("@shikijs/langs/python"),
  import("@shikijs/langs/rust"),
  import("@shikijs/langs/markdown"),
  import("@shikijs/langs/yaml"),
  import("@shikijs/langs/bash"),
  import("@shikijs/langs/toml"),
  import("@shikijs/langs/css"),
  import("@shikijs/langs/html"),
];

// ─── Cold languages (loaded in background after init) ───────────────
const LAZY_LANG_LOADERS: Array<() => Promise<unknown>> = [
  () => import("@shikijs/langs/jsx"),
  () => import("@shikijs/langs/scss"),
  () => import("@shikijs/langs/less"),
  () => import("@shikijs/langs/vue"),
  () => import("@shikijs/langs/svelte"),
  () => import("@shikijs/langs/astro"),
  () => import("@shikijs/langs/jsonc"),
  () => import("@shikijs/langs/xml"),
  () => import("@shikijs/langs/graphql"),
  () => import("@shikijs/langs/c"),
  () => import("@shikijs/langs/cpp"),
  () => import("@shikijs/langs/go"),
  () => import("@shikijs/langs/zig"),
  () => import("@shikijs/langs/java"),
  () => import("@shikijs/langs/kotlin"),
  () => import("@shikijs/langs/scala"),
  () => import("@shikijs/langs/groovy"),
  () => import("@shikijs/langs/csharp"),
  () => import("@shikijs/langs/fsharp"),
  () => import("@shikijs/langs/ruby"),
  () => import("@shikijs/langs/lua"),
  () => import("@shikijs/langs/php"),
  () => import("@shikijs/langs/perl"),
  () => import("@shikijs/langs/r"),
  () => import("@shikijs/langs/julia"),
  () => import("@shikijs/langs/elixir"),
  () => import("@shikijs/langs/erlang"),
  () => import("@shikijs/langs/clojure"),
  () => import("@shikijs/langs/dart"),
  () => import("@shikijs/langs/swift"),
  () => import("@shikijs/langs/objective-c"),
  () => import("@shikijs/langs/powershell"),
  () => import("@shikijs/langs/bat"),
  () => import("@shikijs/langs/fish"),
  () => import("@shikijs/langs/latex"),
  () => import("@shikijs/langs/bibtex"),
  () => import("@shikijs/langs/dockerfile"),
  () => import("@shikijs/langs/hcl"),
  () => import("@shikijs/langs/nginx"),
  () => import("@shikijs/langs/prisma"),
  () => import("@shikijs/langs/protobuf"),
  () => import("@shikijs/langs/ini"),
  () => import("@shikijs/langs/dotenv"),
  () => import("@shikijs/langs/sql"),
  () => import("@shikijs/langs/diff"),
  () => import("@shikijs/langs/makefile"),
  () => import("@shikijs/langs/cmake"),
  () => import("@shikijs/langs/csv"),
  () => import("@shikijs/langs/mermaid"),
  () => import("@shikijs/langs/regexp"),
];

// All language IDs we support.
const ALL_LANG_IDS = [
  "javascript", "typescript", "tsx", "json", "python", "rust",
  "markdown", "yaml", "bash", "toml", "css", "html",
  "jsx", "scss", "less", "vue", "svelte", "astro", "jsonc", "xml",
  "graphql", "c", "cpp", "go", "zig", "java", "kotlin", "scala",
  "groovy", "csharp", "fsharp", "ruby", "lua", "php", "perl", "r",
  "julia", "elixir", "erlang", "clojure", "dart", "swift",
  "objective-c", "powershell", "bat", "fish", "latex", "bibtex",
  "dockerfile", "hcl", "nginx", "prisma", "protobuf", "ini",
  "dotenv", "sql", "diff", "makefile", "cmake", "csv", "mermaid",
  "regexp", "shell", "plaintext",
];

let highlighterPromise: Promise<HighlighterCore> | null = null;
let shikiWired = false;

/**
 * Returns true if shikiToMonaco has been called (themes are registered).
 */
export function isShikiWired(): boolean {
  return shikiWired;
}

/**
 * Load a theme by ID. Since all themes are bundled eagerly, this just
 * checks if it's already loaded in the highlighter (it always will be).
 */
export async function loadTheme(themeId: string): Promise<string> {
  await initShikiMonaco();
  return themeId;
}

/**
 * Initialize the Shiki highlighter and wire it into Monaco.
 * All themes are loaded eagerly (bundled). Safe to call multiple times.
 */
export function initShikiMonaco(): Promise<HighlighterCore> {
  if (highlighterPromise) return highlighterPromise;

  highlighterPromise = (async () => {
    const highlighter = await createHighlighterCore({
      themes: ALL_THEME_IMPORTS,
      langs: HOT_LANG_IMPORTS,
      engine: createJavaScriptRegexEngine(),
    });

    // Register all language IDs with Monaco.
    for (const id of ALL_LANG_IDS) {
      monaco.languages.register({ id });
    }

    // Wire Shiki into Monaco — all themes are now available.
    shikiToMonaco(highlighter, monaco);
    shikiWired = true;

    // Load remaining languages in the background.
    setTimeout(() => void loadLazyLanguages(highlighter), 200);

    return highlighter;
  })();

  return highlighterPromise;
}

/**
 * Returns the shared Shiki highlighter, initialising it on first call.
 *
 * This is a thin alias around `initShikiMonaco()` for callers that
 * want to render arbitrary code-to-HTML (chat tool cards, message
 * previews) without depending on Monaco. The same singleton drives
 * every highlighting surface in the app, so themes and grammars stay
 * consistent across the editor and the chat tool cards.
 */
export function getShikiHighlighter(): Promise<HighlighterCore> {
  return initShikiMonaco();
}

async function loadLazyLanguages(highlighter: HighlighterCore) {
  const BATCH = 4;
  for (let i = 0; i < LAZY_LANG_LOADERS.length; i += BATCH) {
    const batch = LAZY_LANG_LOADERS.slice(i, i + BATCH);
    const modules = await Promise.all(batch.map(fn => fn()));
    for (const mod of modules) {
      try {
        const lang = (mod as { default?: unknown }).default ?? mod;
        await highlighter.loadLanguage(lang as Parameters<typeof highlighter.loadLanguage>[0]);
      } catch {
        // Skip languages that fail.
      }
    }
    await new Promise(r => setTimeout(r, 16));
  }
}
