/**
 * Lightweight Shiki highlighter for `codeToHtml` — Monaco-free.
 *
 * This module provides a standalone Shiki instance used exclusively
 * for rendering syntax-highlighted HTML in the chat tool cards
 * (ShikiPreview, EditCard, ReadCard). It is intentionally separate
 * from `shiki-monaco.ts` so that importing it does NOT pull in the
 * Monaco editor bundle.
 *
 * The instance is memoised — the first call initialises it and every
 * subsequent call returns the same promise. Themes and hot languages
 * are loaded eagerly (same set as shiki-monaco) so the first render
 * is instant once the promise resolves.
 */

import { createHighlighterCore } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import type { HighlighterCore } from "shiki/core";

// ── Themes ──────────────────────────────────────────────────────────
// We only need the two themes used by the chat surface. Loading all
// 40+ themes here would bloat the initial chunk unnecessarily.
const THEME_IMPORTS = [
  import("@shikijs/themes/github-dark"),
  import("@shikijs/themes/github-light"),
];

// ── Hot languages (loaded eagerly) ──────────────────────────────────
// These cover the vast majority of files agents touch. Cold languages
// are loaded on-demand in `highlight()` below.
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

// ── Cold language loaders ────────────────────────────────────────────
// Loaded on first use via `ensureLanguage()`.
const COLD_LANG_LOADERS: Record<string, () => Promise<unknown>> = {
  jsx:          () => import("@shikijs/langs/jsx"),
  scss:         () => import("@shikijs/langs/scss"),
  less:         () => import("@shikijs/langs/less"),
  vue:          () => import("@shikijs/langs/vue"),
  svelte:       () => import("@shikijs/langs/svelte"),
  astro:        () => import("@shikijs/langs/astro"),
  jsonc:        () => import("@shikijs/langs/jsonc"),
  xml:          () => import("@shikijs/langs/xml"),
  graphql:      () => import("@shikijs/langs/graphql"),
  c:            () => import("@shikijs/langs/c"),
  cpp:          () => import("@shikijs/langs/cpp"),
  go:           () => import("@shikijs/langs/go"),
  zig:          () => import("@shikijs/langs/zig"),
  java:         () => import("@shikijs/langs/java"),
  kotlin:       () => import("@shikijs/langs/kotlin"),
  scala:        () => import("@shikijs/langs/scala"),
  csharp:       () => import("@shikijs/langs/csharp"),
  fsharp:       () => import("@shikijs/langs/fsharp"),
  ruby:         () => import("@shikijs/langs/ruby"),
  lua:          () => import("@shikijs/langs/lua"),
  php:          () => import("@shikijs/langs/php"),
  perl:         () => import("@shikijs/langs/perl"),
  r:            () => import("@shikijs/langs/r"),
  julia:        () => import("@shikijs/langs/julia"),
  elixir:       () => import("@shikijs/langs/elixir"),
  erlang:       () => import("@shikijs/langs/erlang"),
  clojure:      () => import("@shikijs/langs/clojure"),
  dart:         () => import("@shikijs/langs/dart"),
  swift:        () => import("@shikijs/langs/swift"),
  "objective-c":() => import("@shikijs/langs/objective-c"),
  powershell:   () => import("@shikijs/langs/powershell"),
  bat:          () => import("@shikijs/langs/bat"),
  fish:         () => import("@shikijs/langs/fish"),
  latex:        () => import("@shikijs/langs/latex"),
  bibtex:       () => import("@shikijs/langs/bibtex"),
  dockerfile:   () => import("@shikijs/langs/dockerfile"),
  hcl:          () => import("@shikijs/langs/hcl"),
  nginx:        () => import("@shikijs/langs/nginx"),
  prisma:       () => import("@shikijs/langs/prisma"),
  protobuf:     () => import("@shikijs/langs/protobuf"),
  ini:          () => import("@shikijs/langs/ini"),
  dotenv:       () => import("@shikijs/langs/dotenv"),
  sql:          () => import("@shikijs/langs/sql"),
  diff:         () => import("@shikijs/langs/diff"),
  makefile:     () => import("@shikijs/langs/makefile"),
  cmake:        () => import("@shikijs/langs/cmake"),
  groovy:       () => import("@shikijs/langs/groovy"),
  kotlin2:      () => import("@shikijs/langs/kotlin"),
};

let _promise: Promise<HighlighterCore> | null = null;

function getHighlighter(): Promise<HighlighterCore> {
  if (_promise) return _promise;
  _promise = createHighlighterCore({
    themes: THEME_IMPORTS,
    langs: HOT_LANG_IMPORTS,
    engine: createJavaScriptRegexEngine(),
  });
  return _promise;
}

/**
 * Ensure `lang` is loaded in the highlighter. No-ops for hot languages
 * and unknown languages (falls back to plaintext).
 */
async function ensureLanguage(highlighter: HighlighterCore, lang: string): Promise<string> {
  if (lang === "plaintext" || lang === "text") return "plaintext";
  if (highlighter.getLoadedLanguages().includes(lang)) return lang;
  const loader = COLD_LANG_LOADERS[lang];
  if (!loader) return "plaintext";
  try {
    const mod = await loader();
    const grammar = (mod as { default?: unknown }).default ?? mod;
    await highlighter.loadLanguage(grammar as Parameters<typeof highlighter.loadLanguage>[0]);
    return lang;
  } catch {
    return "plaintext";
  }
}

/**
 * Render `code` as syntax-highlighted HTML.
 *
 * Returns `null` on failure so callers can fall back to a plain `<pre>`.
 * The returned HTML string is safe to inject via `dangerouslySetInnerHTML`
 * — Shiki escapes all user content before tokenising.
 */
export async function highlight(
  code: string,
  lang: string,
  theme: "github-dark" | "github-light",
): Promise<string | null> {
  try {
    const highlighter = await getHighlighter();
    const resolvedLang = await ensureLanguage(highlighter, lang);
    return highlighter.codeToHtml(code, { lang: resolvedLang, theme });
  } catch {
    return null;
  }
}
