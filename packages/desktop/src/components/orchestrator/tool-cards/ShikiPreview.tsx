import { Button } from "@/components/ui/button";
/**
 * Renders a code snippet with Shiki syntax highlighting.
 *
 * Uses a dedicated lightweight Shiki instance (`shiki-highlight.ts`)
 * that is completely independent of Monaco — importing this component
 * does NOT pull in the Monaco editor bundle.
 *
 * Behaviour:
 *   - Shows a plain `<pre>` placeholder immediately (zero layout shift).
 *   - Swaps in the Shiki-rendered HTML once highlighting completes
 *     (typically < 50 ms for hot languages, < 200 ms for cold ones).
 *   - Re-highlights automatically when the effective theme changes.
 *   - Truncates long files to `maxLines` with a "Show N more" button.
 *
 * Used by: EditCard (new-file Write), ReadCard (file content),
 * and anywhere else in the chat surface that needs themed code.
 */

import clsx from "clsx";
import { useEffect, useRef, useState } from "react";
import { useEffectiveTheme } from "../../../hooks/useEffectiveTheme";
import { languageForPath } from "../../../lib/codex-diff";
import { highlight } from "../../../lib/shiki-highlight";

interface Props {
  /** Source code to highlight. */
  code: string;
  /** File path used to infer the language. Optional — pass `lang` directly when path is unavailable. */
  path?: string;
  /** Explicit Shiki language id override. Wins over `path`. */
  lang?: string;
  /** Maximum lines to render before truncating. Defaults to 200. */
  maxLines?: number;
  /** Optional extra className applied to the outer wrapper. */
  className?: string;
}

const DEFAULT_MAX_LINES = 200;

export function ShikiPreview({ code, path, lang, maxLines = DEFAULT_MAX_LINES, className }: Props) {
  const effective = useEffectiveTheme();
  const language = lang ?? (path ? languageForPath(path) : "plaintext");
  const theme = effective === "dark" ? "github-dark" : "github-light";

  const [html, setHtml] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);
  const mountedRef = useRef(true);

  const lines = code.split("\n");
  const truncated = lines.length > maxLines && !showAll;
  const displayCode = truncated ? lines.slice(0, maxLines).join("\n") : code;
  const hiddenLines = lines.length - maxLines;

  useEffect(() => {
    mountedRef.current = true;
    // Reset html so the plain-pre placeholder shows immediately while
    // the new highlight is in flight (avoids stale coloured output
    // flashing when the file or theme changes).
    setHtml(null);

    void highlight(displayCode, language, theme).then((result) => {
      if (mountedRef.current) setHtml(result);
    });

    return () => {
      mountedRef.current = false;
    };
  }, [displayCode, language, theme]);

  return (
    <div className={clsx("flex flex-col", className)}>
      {html ? (
        <div
          className={clsx(
            // Shiki wraps output in <pre><code>. We override the pre's
            // background so it inherits the card surface rather than
            // Shiki's theme background (which would create a double-
            // tinted box inside the already-tinted card body).
            "shiki-preview overflow-auto",
            "[&_.shiki]:!bg-transparent",
            "[&_pre]:!m-0 [&_pre]:!p-2.5 [&_pre]:!bg-transparent [&_pre]:!rounded-none",
            "[&_code]:text-[11px] [&_code]:leading-[1.55] [&_code]:font-mono",
          )}
          // SAFETY: HTML is produced by Shiki's own renderer which
          // escapes all user content before tokenising. No XSS risk.
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        // Plain-text placeholder shown while Shiki is loading.
        <pre className="font-mono text-[11px] leading-[1.55] text-foreground/70 whitespace-pre overflow-auto px-2.5 py-2">
          {displayCode}
        </pre>
      )}
      {truncated && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setShowAll(true)}
          className="mt-1 self-start text-[9px] text-primary hover:text-primary-hover transition px-2.5 pb-1"
        >
          Show {hiddenLines} more lines…
        </Button>
      )}
    </div>
  );
}
