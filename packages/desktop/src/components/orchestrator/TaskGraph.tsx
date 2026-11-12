import { Button } from "@/components/ui/button";
import {
  AlertCircle,
  CheckCircle2,
  Circle,
  Clock,
  Loader2,
} from "lucide-react";
import { useMemo } from "react";

// ── Types ───────────────────────────────────────────────────────────

interface TaskNode {
  id: string;
  title: string;
  description: string;
  suggestedAgent: "claude-code" | "codex" | "custom";
  estimatedFiles: string[];
  dependencies: string[];
  status: "queued" | "running" | "reviewing" | "applied" | "failed";
  workspaceId?: string;
}

interface TaskGraphData {
  id: string;
  rootPrompt: string;
  nodes: TaskNode[];
  createdAt: number;
  status: "draft" | "approved" | "in-progress" | "completed";
}

interface Props {
  graph: TaskGraphData;
  onNodeClick?: (nodeId: string) => void;
}

// ── Helpers ─────────────────────────────────────────────────────────

function statusIcon(status: TaskNode["status"]) {
  switch (status) {
    case "applied":
      return <CheckCircle2 size={14} className="text-success" />;
    case "running":
      return <Loader2 size={14} className="animate-spin text-primary" />;
    case "reviewing":
      return <Circle size={14} className="fill-info text-info" />;
    case "failed":
      return <AlertCircle size={14} className="text-destructive" />;
    case "queued":
    default:
      return <Clock size={14} className="text-muted-foreground/60" />;
  }
}

function statusLabel(status: TaskNode["status"]): string {
  switch (status) {
    case "applied":
      return "Done";
    case "running":
      return "Running";
    case "reviewing":
      return "Reviewing";
    case "failed":
      return "Failed";
    case "queued":
    default:
      return "Queued";
  }
}

interface LayoutNode {
  node: TaskNode;
  column: number;
  row: number;
}

/**
 * Simple left-to-right topological layout. Assigns each node to a
 * column based on its longest dependency chain depth.
 */
function computeLayout(nodes: TaskNode[]): LayoutNode[] {
  const nodeMap = new Map(nodes.map((n) => [n.id, n]));
  const depths = new Map<string, number>();

  function getDepth(id: string): number {
    if (depths.has(id)) return depths.get(id)!;
    const node = nodeMap.get(id);
    if (!node || node.dependencies.length === 0) {
      depths.set(id, 0);
      return 0;
    }
    const maxDep = Math.max(
      ...node.dependencies
        .filter((d) => nodeMap.has(d))
        .map((d) => getDepth(d) + 1),
    );
    depths.set(id, maxDep);
    return maxDep;
  }

  for (const node of nodes) {
    getDepth(node.id);
  }

  // Group by column.
  const columns = new Map<number, TaskNode[]>();
  for (const node of nodes) {
    const col = depths.get(node.id) ?? 0;
    if (!columns.has(col)) columns.set(col, []);
    columns.get(col)!.push(node);
  }

  const layout: LayoutNode[] = [];
  for (const [col, colNodes] of columns) {
    colNodes.forEach((node, row) => {
      layout.push({ node, column: col, row });
    });
  }

  return layout;
}

// ── Component ───────────────────────────────────────────────────────

export function TaskGraph({ graph, onNodeClick }: Props) {
  const layout = useMemo(() => computeLayout(graph.nodes), [graph.nodes]);

  const maxCol = Math.max(0, ...layout.map((l) => l.column));

  // Progress summary.
  const done = graph.nodes.filter((n) => n.status === "applied").length;
  const running = graph.nodes.filter((n) => n.status === "running").length;
  const failed = graph.nodes.filter((n) => n.status === "failed").length;
  const total = graph.nodes.length;

  // Group layout by column for rendering.
  const columnGroups = useMemo(() => {
    const groups: LayoutNode[][] = [];
    for (let c = 0; c <= maxCol; c++) {
      groups.push(layout.filter((l) => l.column === c));
    }
    return groups;
  }, [layout, maxCol]);

  return (
    <div
      className="rounded border border-border bg-background p-4"
      role="figure"
      aria-label={`Task graph: ${graph.rootPrompt}`}
    >
      {/* Progress summary */}
      <div className="mb-3 flex items-center gap-3 text-[11px] text-muted-foreground">
        <span>
          {done}/{total} complete
        </span>
        {running > 0 && <span className="text-primary">{running} running</span>}
        {failed > 0 && (
          <span className="text-destructive">{failed} failed</span>
        )}
      </div>

      {/* DAG layout: left-to-right columns */}
      <div
        className="flex items-start gap-6 overflow-x-auto pb-2"
        role="list"
        aria-label="Task nodes"
      >
        {columnGroups.map((colNodes, colIdx) => (
          <div key={colIdx} className="flex flex-col gap-2">
            {colNodes.map(({ node }) => (
              <div key={node.id} className="flex items-center gap-2">
                {/* Dependency arrow (CSS line) */}
                {colIdx > 0 && (
                  <div className="h-px w-4 bg-border" aria-hidden="true" />
                )}

                {/* Node card */}
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={() => onNodeClick?.(node.id)}
                  className="justify-start flex items-center gap-2 rounded border border-border/60 bg-background px-2.5 py-1.5 text-left transition hover:border-primary/60 hover:bg-accent focus-visible:outline focus-visible:outline-1 focus-visible:outline-primary"
                  aria-label={`${node.title} — ${statusLabel(node.status)}`}
                  role="listitem"
                >
                  {statusIcon(node.status)}
                  <span className="max-w-[140px] truncate text-xs text-foreground">
                    {node.title}
                  </span>
                </Button>

                {/* Outgoing arrow */}
                {colIdx < maxCol && (
                  <div className="h-px w-4 bg-border" aria-hidden="true" />
                )}
              </div>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}
