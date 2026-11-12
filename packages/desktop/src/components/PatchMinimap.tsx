import { useCallback, useEffect, useRef, useState } from "react";

/**
 * Lightweight minimap for `<PatchDiff />`.
 *
 * Renders a narrow vertical strip showing colored ticks for additions
 * (green) and deletions (red) at their proportional positions within
 * the diff. A semi-transparent viewport indicator shows which portion
 * of the diff is currently visible; clicking the minimap scrolls the
 * container to that position.
 */

export interface PatchMinimapLine {
  /** 0-based index within the total line list. */
  index: number;
  kind: "add" | "del" | "ctx";
}

/**
 * Extract line-level add/delete/context information from a unified
 * patch string. Only body lines (those starting with +, -, or space)
 * are included — headers and metadata are skipped.
 */
export function extractPatchLines(patch: string): PatchMinimapLine[] {
  const lines = patch.split("\n");
  const result: PatchMinimapLine[] = [];
  let inBody = false;

  for (const line of lines) {
    // Skip diff headers until we hit the first hunk header.
    if (line.startsWith("@@")) {
      inBody = true;
      continue;
    }
    if (line.startsWith("diff --git ")) {
      inBody = false;
      continue;
    }
    if (!inBody) continue;

    // Skip the "no newline" marker.
    if (line.startsWith("\\ ")) continue;

    const prefix = line[0];
    if (prefix === "+") {
      result.push({ index: result.length, kind: "add" });
    } else if (prefix === "-") {
      result.push({ index: result.length, kind: "del" });
    } else if (prefix === " " || prefix === undefined) {
      // Context line or empty trailing line.
      result.push({ index: result.length, kind: "ctx" });
    }
  }

  return result;
}

interface Props {
  /** The raw unified patch string. */
  patch: string;
  /** The scroll container element to track and control. */
  scrollContainer: HTMLElement | null;
}

const MINIMAP_WIDTH = 40;
const TICK_COLOR_ADD = "rgba(63, 185, 80, 0.85)";
const TICK_COLOR_DEL = "rgba(248, 81, 73, 0.85)";
const VIEWPORT_BG = "rgba(150, 150, 150, 0.18)";
const VIEWPORT_BORDER = "rgba(150, 150, 150, 0.4)";

export function PatchMinimap({ patch, scrollContainer }: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [viewportRatio, setViewportRatio] = useState({ top: 0, height: 1 });
  const linesRef = useRef<PatchMinimapLine[]>([]);
  const rafRef = useRef<number | null>(null);

  // Parse patch lines.
  useEffect(() => {
    linesRef.current = extractPatchLines(patch);
  }, [patch]);

  // Draw the minimap ticks onto the canvas.
  const draw = useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const rect = canvas.getBoundingClientRect();
    const w = rect.width;
    const h = rect.height;

    canvas.width = w * dpr;
    canvas.height = h * dpr;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, w, h);

    const patchLines = linesRef.current;
    if (patchLines.length === 0) return;

    // Draw ticks — only additions and deletions.
    const tickHeight = Math.max(1, h / patchLines.length);
    for (const line of patchLines) {
      if (line.kind === "ctx") continue;
      const y = (line.index / patchLines.length) * h;
      ctx.fillStyle = line.kind === "add" ? TICK_COLOR_ADD : TICK_COLOR_DEL;
      ctx.fillRect(2, y, w - 4, Math.max(1, tickHeight));
    }
  }, []);

  // Track scroll position to update the viewport indicator.
  useEffect(() => {
    if (!scrollContainer) return;

    const update = () => {
      const { scrollTop, scrollHeight, clientHeight } = scrollContainer;
      if (scrollHeight <= 0) {
        setViewportRatio({ top: 0, height: 1 });
        return;
      }
      setViewportRatio({
        top: scrollTop / scrollHeight,
        height: Math.min(1, clientHeight / scrollHeight),
      });
    };

    update();
    scrollContainer.addEventListener("scroll", update, { passive: true });
    const ro = new ResizeObserver(update);
    ro.observe(scrollContainer);

    return () => {
      scrollContainer.removeEventListener("scroll", update);
      ro.disconnect();
    };
  }, [scrollContainer]);

  // Redraw when patch or container size changes.
  useEffect(() => {
    draw();
    const container = containerRef.current;
    if (!container) return;
    const ro = new ResizeObserver(() => {
      if (rafRef.current != null) cancelAnimationFrame(rafRef.current);
      rafRef.current = requestAnimationFrame(draw);
    });
    ro.observe(container);
    return () => {
      ro.disconnect();
      if (rafRef.current != null) cancelAnimationFrame(rafRef.current);
    };
  }, [draw, patch]);

  // Click handler: scroll the container to the clicked position.
  const handleClick = useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      if (!scrollContainer || !containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const clickY = e.clientY - rect.top;
      const ratio = clickY / rect.height;
      const targetScroll =
        ratio * scrollContainer.scrollHeight -
        scrollContainer.clientHeight / 2;
      scrollContainer.scrollTo({
        top: Math.max(0, targetScroll),
        behavior: "smooth",
      });
    },
    [scrollContainer],
  );

  // Don't render if the patch is trivially small.
  if (linesRef.current.length < 5 && patch.split("\n").length < 10) {
    return null;
  }

  return (
    <div
      ref={containerRef}
      className="relative shrink-0 cursor-pointer select-none bg-background"
      style={{ width: MINIMAP_WIDTH }}
      onClick={handleClick}
      role="scrollbar"
      aria-label="Diff minimap"
      aria-orientation="vertical"
      aria-valuenow={Math.round(viewportRatio.top * 100)}
      aria-valuemin={0}
      aria-valuemax={100}
    >
      <canvas
        ref={canvasRef}
        className="absolute inset-0 h-full w-full"
        style={{ width: MINIMAP_WIDTH }}
      />
      {/* Viewport indicator */}
      <div
        className="pointer-events-none absolute left-0 right-0 rounded-sm"
        style={{
          top: `${viewportRatio.top * 100}%`,
          height: `${viewportRatio.height * 100}%`,
          backgroundColor: VIEWPORT_BG,
          borderTop: `1px solid ${VIEWPORT_BORDER}`,
          borderBottom: `1px solid ${VIEWPORT_BORDER}`,
          minHeight: 8,
        }}
      />
    </div>
  );
}
