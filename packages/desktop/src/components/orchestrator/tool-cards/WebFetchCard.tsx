import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for WebFetch tool calls.
 *
 * Shows the URL with a globe icon, and optionally the fetched
 * content in a collapsible pane.
 *
 * Agent-agnostic: works with any adapter that provides:
 *   - input.url
 *   - detail (fetched content from tool_result)
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, Globe, ExternalLink } from "lucide-react";
import { Streamdown } from "streamdown";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Helpers ─────────────────────────────────────────────────────────

function extractDomain(url: string): string {
  try {
    return new URL(url).hostname;
  } catch {
    return url;
  }
}

/**
 * A loose heuristic: if the content has multiple markdown signals
 * (headings, lists, code fences, emphasis) we render it through
 * Streamdown. Otherwise we keep it as plain text — fetched HTML
 * already includes paragraph breaks, but raw transcripts shouldn't
 * pretend to be markdown.
 */
function looksLikeMarkdown(text: string): boolean {
  let signals = 0;
  if (/^#{1,6}\s+/m.test(text)) signals++;
  if (/^\s*[-*]\s+/m.test(text)) signals++;
  if (/^```/m.test(text)) signals++;
  if (/\[[^\]]+\]\([^)]+\)/.test(text)) signals++;
  if (/\*\*[^*]+\*\*|__[^_]+__/.test(text)) signals++;
  return signals >= 2;
}

const CONTENT_TRUNCATION_LIMIT = 5000;

// ── Component ───────────────────────────────────────────────────────

export function WebFetchCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const input = event.input as Record<string, unknown> | undefined;
  const url = String(input?.url ?? "");
  const query = String(input?.query ?? "");
  const content = event.detail ?? "";
  const hasContent = content.length > 0;

  // Determine what to display: URL-based fetch or query-based search.
  const isSearch = !url && !!query;
  const domain = url ? extractDomain(url) : "";

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Globe size={11} className="shrink-0 text-muted-foreground" />
        <span className="font-medium text-foreground/80">{isSearch ? "Search" : "Fetch"}</span>
        <span className="truncate text-foreground/70">{isSearch ? query : domain || url}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Web ${isSearch ? "Search" : "Fetch"}: ${isSearch ? query : url}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-border/40 overflow-hidden">
      {/* Header */}
      <button
        type="button"
        onClick={() => hasContent && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasContent && "hover:bg-accent cursor-pointer",
          !hasContent && "cursor-default",
        )}
      >
        <Globe size={11} className="text-muted-foreground shrink-0" />
        <span className="shrink-0 font-medium text-muted-foreground">
          {isSearch ? "Search" : "Fetch"}
        </span>

        {/* URL or query */}
        <span className="min-w-0 flex-1 flex items-center gap-1.5 truncate">
          {isSearch ? (
            <span className="text-foreground/80 italic truncate">&ldquo;{query}&rdquo;</span>
          ) : (
            <>
              <span className="text-foreground/80 font-medium">{domain}</span>
              {url && url !== domain && (
                <span className="text-[9px] text-muted-foreground/60 truncate">{url}</span>
              )}
            </>
          )}
        </span>

        {/* External link */}
        {url && url.startsWith("http") && (
          <a
            href={url}
            target="_blank"
            rel="noopener noreferrer"
            onClick={(e) => e.stopPropagation()}
            className="shrink-0 text-muted-foreground hover:text-primary transition p-0.5"
            aria-label="Open URL"
          >
            <ExternalLink size={9} />
          </a>
        )}

        {hasContent && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
            : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        )}
      </button>

      {/* Expanded: fetched content */}
      {expanded && hasContent && (
        <div className="border-t border-border/30 max-h-[300px] overflow-auto px-2.5 py-2">
          {looksLikeMarkdown(content) ? (
            <div className="prose-agent text-[11px]">
              <Streamdown shikiTheme={["github-light", "github-dark"]}>
                {content.length > CONTENT_TRUNCATION_LIMIT
                  ? content.slice(0, CONTENT_TRUNCATION_LIMIT) + "\n\n_… (truncated)_"
                  : content}
              </Streamdown>
            </div>
          ) : (
            <pre className="font-mono text-[10px] leading-[1.5] text-foreground/60 whitespace-pre-wrap break-words">
              {content.length > CONTENT_TRUNCATION_LIMIT
                ? content.slice(0, CONTENT_TRUNCATION_LIMIT) + "\n… (truncated)"
                : content}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}
