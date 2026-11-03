/**
 * Renders text containing ANSI / CSI escape sequences as styled React
 * elements. Backed by `anser` (https://www.npmjs.com/package/anser),
 * which parses SGR escapes into a structured JSON tree we then map
 * onto `<span>`s with CSS class names.
 *
 * We intentionally avoid `dangerouslySetInnerHTML` / `ansiToHtml` —
 * the JSON path is safer (anser already escapes content for the HTML
 * path, but React handles text nodes correctly without us needing to
 * trust the library's escaping).
 *
 * Exposed helpers:
 *   - `<AnsiText>` — drop-in for rendering a string with colours preserved.
 *   - `ansiToReact(text)` — pure helper returning React nodes (used by
 *     the AnsiText component and unit tests).
 *   - `stripAnsiToText(text)` — strips all SGR sequences down to a clean
 *     string, useful for header summaries and clipboard copies.
 *
 * The class names produced by anser with `use_classes: true` follow the
 * pattern `ansi-{name}-fg`, `ansi-{name}-bg`, `ansi-bright-{name}-fg`,
 * etc., plus decoration classes like `ansi-bold` and `ansi-underline`.
 * The matching CSS lives in `src/index.css`.
 */

import Anser, { type AnserJsonEntry } from "anser";
import { Fragment, type ReactNode } from "react";

const COLOR_NAMES = [
  "black",
  "red",
  "green",
  "yellow",
  "blue",
  "magenta",
  "cyan",
  "white",
] as const;

/**
 * Builds the list of class names for a single anser entry.
 *
 * When called with `use_classes: true`, anser returns colour values like
 * `"ansi-red"` / `"ansi-bright-red"` directly in `entry.fg` and
 * `entry.bg`. We append `-fg` / `-bg` to produce the final class
 * (`ansi-red-fg`, `ansi-bright-red-bg`) and surface decorations
 * (`ansi-bold`, `ansi-underline`, …) verbatim.
 *
 * Anything that isn't one of the 16 named palette entries (24-bit
 * truecolor, 256-colour cube) is ignored — we don't generate CSS for
 * arbitrary RGB values; the segment renders unstyled, which is the
 * right fallback for noisy output.
 */
function classesForEntry(entry: AnserJsonEntry): string[] {
  const classes: string[] = [];

  const fgClass = paletteToFullClass(entry.fg, "fg");
  if (fgClass) classes.push(fgClass);

  const bgClass = paletteToFullClass(entry.bg, "bg");
  if (bgClass) classes.push(bgClass);

  if (entry.decorations && entry.decorations.length > 0) {
    for (const deco of entry.decorations) {
      classes.push(`ansi-${deco}`);
    }
  }

  return classes;
}

/**
 * Converts anser's palette identifier (`"ansi-red"` /
 * `"ansi-bright-cyan"`) into the final class name (`"ansi-red-fg"`)
 * or returns null if the value isn't a recognised palette entry.
 */
function paletteToFullClass(value: unknown, kind: "fg" | "bg"): string | null {
  if (typeof value !== "string" || !value) return null;
  if (!/^ansi-(?:bright-)?(?:black|red|green|yellow|blue|magenta|cyan|white)$/.test(value)) {
    return null;
  }
  return `${value}-${kind}`;
}

/**
 * Pure helper: parses an ANSI-bearing string and returns React nodes.
 * Empty / non-string input renders nothing.
 */
export function ansiToReact(text: string): ReactNode[] {
  if (!text) return [];

  const entries = Anser.ansiToJson(text, {
    json: true,
    use_classes: true,
    remove_empty: true,
  });

  const nodes: ReactNode[] = [];
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    if (!entry.content) continue;

    const classes = classesForEntry(entry);
    if (classes.length === 0) {
      // No styling — emit a bare text fragment so we don't litter the
      // DOM with unnecessary <span>s.
      nodes.push(<Fragment key={i}>{entry.content}</Fragment>);
    } else {
      nodes.push(
        <span key={i} className={classes.join(" ")}>
          {entry.content}
        </span>,
      );
    }
  }

  return nodes;
}

/**
 * Strips all ANSI / CSI escape sequences down to plain text. Useful for
 * single-line summaries and clipboard copies where the styling would
 * be more confusing than helpful.
 */
export function stripAnsiToText(text: string): string {
  if (!text) return "";
  return Anser.ansiToText(text);
}

interface AnsiTextProps {
  /** The raw text, possibly containing ANSI escape sequences. */
  children: string;
  /** Optional className applied to the outer wrapper. */
  className?: string;
}

/**
 * Renders ANSI-coloured terminal output as styled React spans.
 *
 * Wraps the parsed segments in a single fragment by default; pass a
 * className to wrap in a `<span>` for layout purposes.
 */
export function AnsiText({ children, className }: AnsiTextProps): JSX.Element {
  const nodes = ansiToReact(children);
  if (className) {
    return <span className={className}>{nodes}</span>;
  }
  return <>{nodes}</>;
}

// Exported for convenience — palette names referenced from CSS.
export { COLOR_NAMES };
