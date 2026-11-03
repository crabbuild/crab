import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Codex `imageView` items.
 *
 * Renders the image inline so the user can see what the agent is
 * looking at. Falls back to a "broken image" hint when the path is
 * outside the worktree, the file is too large, or the format is not
 * a recognised image type.
 *
 * Path resolution:
 *   - Absolute path (`/tmp/screenshot.png`) → used as-is
 *   - Relative path (`out.png`) → joined with the active task's
 *     worktree root, read from `HunkActionsContext`.
 */

import { useEffect, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Image as ImageIcon,
  ExternalLink,
  AlertTriangle,
} from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { useHunkActions } from "./HunkActionsContext";

const SUPPORTED_EXT = new Set([
  ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp", ".ico", ".avif",
]);

export function ImageViewCard({ event, defaultExpanded = true, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const ctx = useHunkActions();
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const input = event.input as Record<string, unknown> | undefined;
  const rawPath = String(input?.file_path ?? input?.path ?? event.summary?.replace(/^ImageView:\s*/, "") ?? "");
  const shortPath = rawPath.split("/").pop() ?? rawPath;
  const ext = "." + (rawPath.split(".").pop()?.toLowerCase() ?? "");
  const isSupportedFormat = SUPPORTED_EXT.has(ext);

  const absolutePath = (() => {
    if (!rawPath) return "";
    if (rawPath.startsWith("/") || /^[A-Za-z]:\\/.test(rawPath)) return rawPath;
    if (!ctx?.worktreePath) return "";
    return `${ctx.worktreePath.replace(/\/$/, "")}/${rawPath}`;
  })();

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <ImageIcon size={11} className="shrink-0 text-gx-muted" />
        <span className="font-medium text-gx-text/80">Image</span>
        <span className="truncate text-gx-text/70">{shortPath}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Image: ${shortPath}`}
      />
    );
  }

  useEffect(() => {
    if (!expanded) return;
    if (!absolutePath || !isSupportedFormat) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (window as any).crab?.fsReadImage?.(absolutePath)
      .then((res: { exists: boolean; dataUrl: string | null; tooLarge?: boolean }) => {
        if (cancelled) return;
        if (!res?.exists) {
          setError("File not found");
          return;
        }
        if (res.tooLarge) {
          setError("Image too large to preview");
          return;
        }
        setDataUrl(res.dataUrl);
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [absolutePath, isSupportedFormat, expanded]);

  const canRender = isSupportedFormat && (!!dataUrl || !!error || loading);

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left hover:bg-gx-surface-hover transition cursor-pointer"
      >
        <ImageIcon size={11} className="shrink-0 text-gx-muted" />
        <span className="shrink-0 font-medium text-gx-muted">Image</span>
        <span className="min-w-0 flex-1 truncate">
          <span className="text-gx-text/90 font-medium">{shortPath}</span>
        </span>
        {!isSupportedFormat && rawPath && (
          <span className="shrink-0 inline-flex items-center gap-0.5 text-[8px] text-gx-warn">
            <AlertTriangle size={9} /> {ext || "no ext"}
          </span>
        )}
        {absolutePath && (
          <a
            href={absolutePath}
            onClick={(e) => e.stopPropagation()}
            className="shrink-0 text-gx-muted hover:text-gx-accent transition p-0.5"
            title="Open file location"
          >
            <ExternalLink size={9} />
          </a>
        )}
        {expanded
          ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
          : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />}
      </button>

      {expanded && (
        <div className="border-t border-gx-border/30 px-2.5 py-2 bg-gx-bg/30">
          {!isSupportedFormat ? (
            <p className="text-[10px] text-gx-muted italic">
              Unsupported format — only PNG, JPEG, GIF, WebP, SVG, BMP, ICO, AVIF render inline.
            </p>
          ) : !ctx?.worktreePath && !rawPath.startsWith("/") ? (
            <p className="text-[10px] text-gx-muted italic">
              No worktree available to resolve this path.
            </p>
          ) : loading ? (
            <p className="text-[10px] text-gx-muted">Loading…</p>
          ) : error ? (
            <p className="text-[10px] text-gx-error">⚠ {error}</p>
          ) : dataUrl ? (
            <img
              src={dataUrl}
              alt={shortPath}
              className="max-h-[280px] w-auto rounded border border-gx-border/40"
            />
          ) : null}
          {canRender && rawPath && (
            <p className="mt-1.5 truncate font-mono text-[9px] text-gx-muted/70">{rawPath}</p>
          )}
        </div>
      )}
    </div>
  );
}
