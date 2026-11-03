import type { FileDiffOptions } from "@pierre/diffs";
import { preloadHighlighter } from "@pierre/diffs";

/**
 * Shared theme options for `<PatchDiff />` / `<FileDiff />`.
 *
 * @pierre/diffs ships `pierre-dark` and `pierre-light` as first-
 * class Shiki themes. We pass both and explicitly set `themeType`
 * to the user's effective theme so the diff never lags behind a
 * system theme change.
 *
 * `unsafeCSS` is a small override that tightens the contrast of the
 * addition/deletion row backgrounds in the dark theme — the shipped
 * default uses very pale tints that bleed into the code cell and
 * make the "+/−" column the only signal. Boosting the background
 * alpha makes hunks visible even at a glance.
 */
export function diffOptions(effective: "dark" | "light"): FileDiffOptions<unknown> {
  return {
    theme: { dark: "pierre-dark", light: "pierre-light" },
    themeType: effective,
    unsafeCSS: UNSAFE_CSS,
  };
}

/**
 * Trigger theme loading up front so the first `<PatchDiff/>` render
 * has both `pierre-dark` and `pierre-light` ready. Without this the
 * first diff briefly paints with an unstyled (near-white) background
 * while Shiki fetches the theme grammar, which reads as "the theme
 * is broken" in dark mode.
 *
 * Call once at app bootstrap. `preloadHighlighter` dedupes internally
 * so extra calls are cheap.
 */
export function preloadDiffThemes(): void {
  void preloadHighlighter({
    themes: ["pierre-dark", "pierre-light"],
    langs: [],
  });
}

// Custom CSS layered on top of the bundled theme.
//
// The diff component uses shadow DOM and exposes its colour tokens
// via CSS custom properties named after the VS Code diff theme
// surface. We tune the handful that drive hunk readability:
//
//   --pd-* — @pierre/diffs' own prefix for add/del row colours.
//   --ph-* — highlight colours (selection, line hover).
//
// Values stay data-attribute scoped so light theme doesn't inherit
// the darker surface colours.
const UNSAFE_CSS = `
:host([data-theme-type="dark"]) {
  --pd-addition-bg: rgba(46, 160, 67, 0.18);
  --pd-addition-word-bg: rgba(46, 160, 67, 0.42);
  --pd-deletion-bg: rgba(248, 81, 73, 0.18);
  --pd-deletion-word-bg: rgba(248, 81, 73, 0.42);
  --pd-gutter-fg: rgba(230, 237, 243, 0.55);
  --pd-gutter-bg: rgba(255, 255, 255, 0.02);
  --pd-hunk-separator-bg: rgba(255, 255, 255, 0.04);
}
:host([data-theme-type="light"]) {
  --pd-addition-bg: rgba(46, 160, 67, 0.12);
  --pd-addition-word-bg: rgba(46, 160, 67, 0.32);
  --pd-deletion-bg: rgba(248, 81, 73, 0.12);
  --pd-deletion-word-bg: rgba(248, 81, 73, 0.32);
  --pd-gutter-fg: rgba(36, 41, 47, 0.62);
  --pd-gutter-bg: rgba(0, 0, 0, 0.02);
  --pd-hunk-separator-bg: rgba(0, 0, 0, 0.04);
}
`;
