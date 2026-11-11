import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { Plus } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────────

export interface InteractiveDiffGutterProps {
  /** The file path this diff belongs to */
  path: string;
  /** Reference to the diff container element for positioning */
  diffContainer: HTMLDivElement | null;
  /** Called when user clicks "+" on a line */
  onCommentLine(line: number, side: "LEFT" | "RIGHT"): void;
  /** Called when user selects a range */
  onCommentRange(startLine: number, endLine: number, side: "LEFT" | "RIGHT"): void;
  /** Current focused line (for keyboard nav highlight) */
  focusedLine?: number;
}

/** Internal representation of a diff row's position. */
interface RowMeta {
  line: number;
  side: "LEFT" | "RIGHT";
  top: number;
  height: number;
}

/** Maximum lines allowed in a range selection. */
const MAX_RANGE_LINES = 100;

// ── Component ────────────────────────────────────────────────────────

/**
 * Overlay that sits absolutely over the diff's line-number gutter column.
 * Uses ResizeObserver + MutationObserver to stay aligned with the
 * underlying diff rows rendered by @pierre/diffs (Shadow DOM).
 *
 * Key behaviors:
 * - Hover: show "+" icon on the hovered line
 * - Click: call onCommentLine(line, side)
 * - Shift-click: select range from last clicked line (max 100 lines)
 * - Click-and-drag: highlight range, on mouseup call onCommentRange
 * - focusedLine prop: render visible focus indicator for keyboard nav
 */
export function InteractiveDiffGutter({
  path,
  diffContainer,
  onCommentLine,
  onCommentRange,
  focusedLine,
}: InteractiveDiffGutterProps) {
  const overlayRef = useRef<HTMLDivElement>(null);
  const [rows, setRows] = useState<RowMeta[]>([]);
  const [hoveredLine, setHoveredLine] = useState<number | null>(null);
  const [dragStart, setDragStart] = useState<number | null>(null);
  const [dragEnd, setDragEnd] = useState<number | null>(null);
  const [isDragging, setIsDragging] = useState(false);
  const lastClickedLine = useRef<number | null>(null);

  // Scan the diff container for line-number elements and compute positions.
  const scanRows = useCallback(() => {
    if (!diffContainer || !overlayRef.current) return;

    const containerRect = diffContainer.getBoundingClientRect();
    const overlayRect = overlayRef.current.getBoundingClientRect();
    const offsetTop = containerRect.top - overlayRect.top;

    const foundRows: RowMeta[] = [];

    // Query line-number cells from the diff container.
    // @pierre/diffs renders line numbers in elements with data-line-number
    // or within .line-number / td elements. We look for common patterns.
    const lineElements = findLineElements(diffContainer);

    for (const el of lineElements) {
      const lineNum = extractLineNumber(el);
      if (lineNum == null) continue;

      const rect = el.getBoundingClientRect();
      const side = detectSide(el);

      foundRows.push({
        line: lineNum,
        side,
        top: rect.top - containerRect.top + offsetTop + diffContainer.scrollTop,
        height: rect.height,
      });
    }

    // Deduplicate by line+side, keeping the first occurrence.
    const seen = new Set<string>();
    const deduped = foundRows.filter((r) => {
      const key = `${r.line}:${r.side}`;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });

    setRows(deduped);
  }, [diffContainer]);

  // Observe container size and DOM mutations to keep rows aligned.
  useEffect(() => {
    if (!diffContainer) return;

    scanRows();

    const resizeObserver = new ResizeObserver(() => {
      scanRows();
    });
    resizeObserver.observe(diffContainer);

    const mutationObserver = new MutationObserver(() => {
      // Debounce slightly to batch rapid DOM changes.
      requestAnimationFrame(scanRows);
    });
    mutationObserver.observe(diffContainer, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["class", "style"],
    });

    // Also re-scan on scroll since positions are relative.
    const handleScroll = () => requestAnimationFrame(scanRows);
    diffContainer.addEventListener("scroll", handleScroll, { passive: true });

    return () => {
      resizeObserver.disconnect();
      mutationObserver.disconnect();
      diffContainer.removeEventListener("scroll", handleScroll);
    };
  }, [diffContainer, scanRows]);

  // Handle mouse move during drag to update the drag range.
  useEffect(() => {
    if (!isDragging) return;

    const handleMouseMove = (e: globalThis.MouseEvent) => {
      if (!overlayRef.current) return;
      const rect = overlayRef.current.getBoundingClientRect();
      const y = e.clientY - rect.top;
      const row = findRowAtY(rows, y);
      if (row != null) {
        setDragEnd(row.line);
      }
    };

    const handleMouseUp = () => {
      setIsDragging(false);
      if (dragStart != null && dragEnd != null && dragStart !== dragEnd) {
        const [start, end] = normalizeRange(dragStart, dragEnd);
        const startRow = rows.find((r) => r.line === start);
        const side = startRow?.side ?? "RIGHT";
        onCommentRange(start, end, side);
      }
      setDragStart(null);
      setDragEnd(null);
    };

    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);

    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
    };
  }, [isDragging, dragStart, dragEnd, rows, onCommentRange]);

  const handleRowMouseDown = useCallback(
    (e: ReactMouseEvent, row: RowMeta) => {
      // Shift-click: range selection from last clicked line.
      if (e.shiftKey && lastClickedLine.current != null) {
        e.preventDefault();
        const [start, end] = normalizeRange(lastClickedLine.current, row.line);
        onCommentRange(start, end, row.side);
        lastClickedLine.current = row.line;
        return;
      }

      // Start potential drag.
      setDragStart(row.line);
      setDragEnd(row.line);
      setIsDragging(true);
    },
    [onCommentRange],
  );

  const handleRowClick = useCallback(
    (e: ReactMouseEvent, row: RowMeta) => {
      // If this was a drag that moved, don't fire click.
      if (dragStart != null && dragEnd != null && dragStart !== dragEnd) return;
      // Shift-click is handled in mousedown.
      if (e.shiftKey) return;

      lastClickedLine.current = row.line;
      onCommentLine(row.line, row.side);
    },
    [dragStart, dragEnd, onCommentLine],
  );

  const handleRowMouseEnter = useCallback((row: RowMeta) => {
    setHoveredLine(row.line);
  }, []);

  const handleMouseLeave = useCallback(() => {
    setHoveredLine(null);
  }, []);

  // Compute the highlighted range during drag.
  const highlightRange = isDragging && dragStart != null && dragEnd != null
    ? normalizeRange(dragStart, dragEnd)
    : null;

  if (!diffContainer) return null;

  return (
    <div
      ref={overlayRef}
      className="pointer-events-none absolute inset-y-0 left-0 z-10 w-8"
      onMouseLeave={handleMouseLeave}
      aria-label={`Comment gutter for ${path}`}
    >
      {rows.map((row) => {
        const isHovered = hoveredLine === row.line;
        const isFocused = focusedLine === row.line;
        const isInHighlight =
          highlightRange != null &&
          row.line >= highlightRange[0] &&
          row.line <= highlightRange[1];

        return (
          <div
            key={`${row.line}:${row.side}`}
            className="pointer-events-auto absolute left-0 flex w-8 items-center justify-center"
            style={{ top: row.top, height: row.height }}
            onMouseEnter={() => handleRowMouseEnter(row)}
            onMouseDown={(e) => handleRowMouseDown(e, row)}
            onClick={(e) => handleRowClick(e, row)}
            role="button"
            tabIndex={-1}
            aria-label={`Add comment on line ${row.line}`}
          >
            {/* Focus indicator for keyboard navigation */}
            {isFocused && (
              <div className="absolute inset-0 rounded-sm border border-primary bg-primary/10" />
            )}

            {/* Drag highlight */}
            {isInHighlight && (
              <div className="absolute inset-0 bg-primary/15" />
            )}

            {/* "+" icon on hover */}
            {isHovered && !isInHighlight && (
              <div className="relative flex h-4 w-4 items-center justify-center rounded bg-primary/20 text-primary transition-opacity">
                <Plus size={12} strokeWidth={2.5} />
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────

/**
 * Find line-number elements within the diff container.
 * Handles both Shadow DOM and regular DOM patterns used by @pierre/diffs.
 */
function findLineElements(container: HTMLElement): Element[] {
  const elements: Element[] = [];

  // Try querying directly first (non-shadow DOM case).
  const directCells = container.querySelectorAll(
    "[data-line-number], .line-number, td.diff-line-num, .diff-gutter-cell",
  );
  if (directCells.length > 0) {
    elements.push(...Array.from(directCells));
    return elements;
  }

  // For Shadow DOM: look for shadow roots in child elements.
  const shadowHosts = container.querySelectorAll("*");
  for (const host of Array.from(shadowHosts)) {
    const shadow = host.shadowRoot;
    if (!shadow) continue;
    const cells = shadow.querySelectorAll(
      "[data-line-number], .line-number, td.diff-line-num, .diff-gutter-cell",
    );
    elements.push(...Array.from(cells));
  }

  // Fallback: look for elements with numeric text content in the gutter area.
  if (elements.length === 0) {
    const allCells = container.querySelectorAll("td, span, div");
    for (const cell of Array.from(allCells)) {
      const text = cell.textContent?.trim();
      if (text && /^\d+$/.test(text) && cell.clientWidth < 60) {
        elements.push(cell);
      }
    }
  }

  return elements;
}

/** Extract a line number from a DOM element. */
function extractLineNumber(el: Element): number | null {
  // data-line-number attribute.
  const attr = el.getAttribute("data-line-number");
  if (attr) {
    const n = parseInt(attr, 10);
    if (!isNaN(n)) return n;
  }

  // Text content as a number.
  const text = el.textContent?.trim();
  if (text && /^\d+$/.test(text)) {
    const n = parseInt(text, 10);
    if (!isNaN(n)) return n;
  }

  return null;
}

/** Detect which side of the diff a line element belongs to. */
function detectSide(el: Element): "LEFT" | "RIGHT" {
  // Check for data attributes or class names indicating side.
  const side = el.getAttribute("data-side");
  if (side === "left" || side === "old") return "LEFT";
  if (side === "right" || side === "new") return "RIGHT";

  // Check parent row for deletion/addition classes.
  const row = el.closest("tr, [data-diff-row]");
  if (row) {
    const cls = row.className;
    if (cls.includes("deletion") || cls.includes("removed") || cls.includes("del")) {
      return "LEFT";
    }
  }

  // Default to RIGHT (new/addition side).
  return "RIGHT";
}

/** Find the row closest to a given Y coordinate. */
function findRowAtY(rows: RowMeta[], y: number): RowMeta | null {
  for (const row of rows) {
    if (y >= row.top && y < row.top + row.height) {
      return row;
    }
  }
  return null;
}

/** Normalize a range to [min, max], capped at MAX_RANGE_LINES. */
function normalizeRange(a: number, b: number): [number, number] {
  const start = Math.min(a, b);
  const end = Math.min(Math.max(a, b), start + MAX_RANGE_LINES - 1);
  return [start, end];
}
