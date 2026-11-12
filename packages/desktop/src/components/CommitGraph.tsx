import { memo, useMemo } from "react";
import { colorForIndex, GraphData, GraphVertex, layoutGraph } from "../lib/commit-graph";
import type { CommitRow } from "../ipc";

// Geometry.
export const ROW_HEIGHT = 44;
const GRID_X = 16;
const GRID_Y = ROW_HEIGHT;
const OFFSET_X = 12;
const OFFSET_Y = ROW_HEIGHT / 2;
const DOT_RADIUS = 4;
const CURRENT_DOT_RADIUS = 5;
const STROKE = 1.6;
// Angular style: the transition happens at 38% of the row height.
const ANGULAR_D = GRID_Y * 0.38;

/** Lay out the commit list (memoised). */
export function useCommitGraphLayout(commits: CommitRow[]): GraphData {
  return useMemo(() => layoutGraph(commits), [commits]);
}

/** Pixel width of the graph gutter. */
export function graphWidth(data: GraphData): number {
  return 2 * OFFSET_X + data.maxX * GRID_X;
}

function px(point: { x: number; y: number }): { x: number; y: number } {
  return {
    x: point.x * GRID_X + OFFSET_X,
    y: point.y * GRID_Y + OFFSET_Y,
  };
}

interface Props {
  data: GraphData;
  selectedIndex: number | null;
  tipShas: Set<string>;
  tagShas: Set<string>;
  /** Scroll offset of the containing list, used to cull off-screen dots. */
  scrollTop?: number;
  /** Visible height of the containing list, used to cull off-screen dots. */
  viewportHeight?: number;
}

/**
 * Windowed SVG rendering of the commit graph. Lines that fall outside
 * the visible viewport (plus a generous buffer) are skipped at memo
 * time, so the path computation cost scales with the visible portion
 * of the graph rather than the total commit count.
 *
 * Uses the vscode-git-graph "angular" style: vertical lines with
 * angular transitions (straight diagonal segments at 38% of row
 * height).
 *
 * Performance: the buffer extends 5 viewports above/below so the
 * memo only invalidates when the user scrolls more than 5 screens,
 * preventing per-scroll recomputation while still keeping memory
 * bounded for huge graphs.
 */
export function CommitGraphSvg({ data, selectedIndex, tipShas, tagShas, scrollTop = 0, viewportHeight = 800 }: Props) {
  const width = useMemo(() => graphWidth(data), [data]);
  const height = data.vertices.length * ROW_HEIGHT;

  // Quantise the scroll window to multiples of `viewportHeight * 5`
  // so the path memo doesn't invalidate on every scroll event.
  // Combined with `BUFFER = viewportHeight * 5`, this means the
  // window only shifts when the user scrolls by more than half a
  // buffer height — typically once per dozen scroll wheels.
  const WINDOW_STEP = Math.max(viewportHeight * 5, 1);
  const windowIdx = Math.floor(scrollTop / WINDOW_STEP);
  const minY = windowIdx * WINDOW_STEP - viewportHeight * 5;
  const maxY = (windowIdx + 1) * WINDOW_STEP + viewportHeight * 5;

  const paths = useMemo(() => {
    const result: React.ReactNode[] = [];

    for (let bi = 0; bi < data.branches.length; bi++) {
      const branch = data.branches[bi];
      const colour = colorForIndex(branch.color);
      let d = "";

      for (let li = 0; li < branch.lines.length; li++) {
        const line = branch.lines[li];
        const p1 = px(line.p1);
        const p2 = px(line.p2);

        // Cull line segments entirely outside the buffered window.
        const segMinY = Math.min(p1.y, p2.y);
        const segMaxY = Math.max(p1.y, p2.y);
        if (segMaxY < minY || segMinY > maxY) continue;

        // Always emit a move command for the segment start so culled
        // gaps don't draw spurious lines back to the previous point.
        if (!d) {
          d = `M${p1.x.toFixed(1)},${p1.y.toFixed(1)}`;
        } else {
          d += `M${p1.x.toFixed(1)},${p1.y.toFixed(1)}`;
        }

        if (p1.x === p2.x) {
          // Vertical — straight line.
          d += `L${p2.x.toFixed(1)},${p2.y.toFixed(1)}`;
        } else {
          // Angular transition: a diagonal segment.
          if (line.lockedFirst) {
            // Transition near p2: go vertical from p1, then diagonal to p2.
            const midY = p2.y - ANGULAR_D;
            d += `L${p1.x.toFixed(1)},${midY.toFixed(1)}L${p2.x.toFixed(1)},${p2.y.toFixed(1)}`;
          } else {
            // Transition near p1: diagonal from p1, then vertical to p2.
            const midY = p1.y + ANGULAR_D;
            d += `L${p1.x.toFixed(1)},${midY.toFixed(1)}L${p2.x.toFixed(1)},${p2.y.toFixed(1)}`;
          }
        }
      }

      if (d) {
        result.push(
          <path
            key={`b${bi}`}
            d={d}
            fill="none"
            stroke={colour}
            strokeWidth={STROKE}
            opacity={0.9}
          />,
        );
      }
    }

    return result;
  }, [data, windowIdx, WINDOW_STEP]);

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      aria-hidden="true"
      className="absolute left-0 top-0 block"
      style={{
        shapeRendering: "geometricPrecision",
        contain: "strict" as React.CSSProperties["contain"],
        willChange: "transform",
      }}
    >
      {paths}
      <GraphDots
        vertices={data.vertices}
        selectedIndex={selectedIndex}
        tipShas={tipShas}
        tagShas={tagShas}
        scrollTop={scrollTop}
        viewportHeight={viewportHeight}
      />
    </svg>
  );
}

/**
 * Memoised dot layer. Isolated from `paths` so that selecting a commit
 * (which changes `selectedIndex`) only re-renders this component, not
 * the branch-path layer which is stable between selections.
 *
 * Only renders dots for vertices in the visible scroll window plus a
 * generous buffer. This keeps the DOM node count proportional to the
 * viewport height rather than the total commit count.
 */
const GraphDots = memo(function GraphDots({
  vertices,
  selectedIndex,
  tipShas,
  tagShas,
  scrollTop,
  viewportHeight,
}: {
  vertices: GraphVertex[];
  selectedIndex: number | null;
  tipShas: Set<string>;
  tagShas: Set<string>;
  scrollTop: number;
  viewportHeight: number;
}) {
  // Buffer: render dots 3 screens above and below the viewport so
  // fast scrolling doesn't show a flash of missing dots.
  const BUFFER = viewportHeight * 3;
  const minY = scrollTop - BUFFER;
  const maxY = scrollTop + viewportHeight + BUFFER;

  return (
    <>
      {vertices.map((v) => {
        // Cull dots that are far outside the visible window.
        const dotY = v.id * ROW_HEIGHT + ROW_HEIGHT / 2;
        if (dotY < minY || dotY > maxY) return null;

        const { x, y } = px({ x: v.x, y: v.id });
        const colour = colorForIndex(v.color);
        const dim = selectedIndex != null && selectedIndex !== v.id;
        const opacity = dim ? 0.55 : 1;
        const isTip = tipShas.has(v.id.toString());
        const isTag = tagShas.has(v.id.toString());

        if (v.isCurrent) {
          return (
            <circle
              key={`v${v.id}`}
              cx={x}
              cy={y}
              r={CURRENT_DOT_RADIUS}
              fill="none"
              stroke={colour}
              strokeWidth={2}
              opacity={opacity}
            />
          );
        } else if (isTip) {
          const ringColor = isTag ? "rgb(var(--warning))" : colour;
          return (
            <g key={`v${v.id}`}>
              <circle
                cx={x}
                cy={y}
                r={CURRENT_DOT_RADIUS + 2}
                fill="none"
                stroke={ringColor}
                strokeWidth={1.5}
                opacity={dim ? 0.4 : 0.85}
              />
              <circle
                cx={x}
                cy={y}
                r={CURRENT_DOT_RADIUS}
                fill={colour}
                stroke="rgb(var(--background))"
                strokeWidth={1.5}
                opacity={opacity}
              />
            </g>
          );
        } else {
          return (
            <circle
              key={`v${v.id}`}
              cx={x}
              cy={y}
              r={DOT_RADIUS}
              fill={colour}
              stroke="rgb(var(--background))"
              strokeWidth={1.5}
              opacity={opacity}
            />
          );
        }
      })}
    </>
  );
});
