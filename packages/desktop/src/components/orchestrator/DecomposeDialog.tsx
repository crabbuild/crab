import { Input } from "@/components/ui/input";
import { NativeSelect } from "@/components/ui/native-select";
import { useCallback, useState } from "react";
import {
  ArrowRight,
  Check,
  GripVertical,
  Pencil,
  Plus,
  Trash2,
  X,
} from "lucide-react";

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

interface TaskGraph {
  id: string;
  rootPrompt: string;
  nodes: TaskNode[];
  createdAt: number;
  status: "draft" | "approved" | "in-progress" | "completed";
}

interface Props {
  graph: TaskGraph;
  onApprove: (graph: TaskGraph) => void;
  onEdit: (graph: TaskGraph) => void;
  onCancel: () => void;
}

type AgentType = TaskNode["suggestedAgent"];

const AGENT_LABELS: Record<AgentType, string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
  custom: "Custom",
};

// ── Component ───────────────────────────────────────────────────────

export function DecomposeDialog({ graph, onApprove, onEdit, onCancel }: Props) {
  const [nodes, setNodes] = useState<TaskNode[]>(() => [...graph.nodes]);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editTitle, setEditTitle] = useState("");

  const handleRename = useCallback((node: TaskNode) => {
    setEditingId(node.id);
    setEditTitle(node.title);
  }, []);

  const commitRename = useCallback(() => {
    if (!editingId) return;
    setNodes((prev) =>
      prev.map((n) => (n.id === editingId ? { ...n, title: editTitle } : n)),
    );
    setEditingId(null);
  }, [editingId, editTitle]);

  const handleRemove = useCallback((nodeId: string) => {
    setNodes((prev) => {
      const filtered = prev.filter((n) => n.id !== nodeId);
      // Remove dangling dependency references.
      return filtered.map((n) => ({
        ...n,
        dependencies: n.dependencies.filter((d) => d !== nodeId),
      }));
    });
  }, []);

  const handleAgentChange = useCallback((nodeId: string, agent: AgentType) => {
    setNodes((prev) =>
      prev.map((n) => (n.id === nodeId ? { ...n, suggestedAgent: agent } : n)),
    );
  }, []);

  const handleMoveUp = useCallback((index: number) => {
    if (index === 0) return;
    setNodes((prev) => {
      const next = [...prev];
      [next[index - 1], next[index]] = [next[index], next[index - 1]];
      return next;
    });
  }, []);

  const handleAddNode = useCallback(() => {
    const newNode: TaskNode = {
      id: crypto.randomUUID(),
      title: "New task",
      description: "",
      suggestedAgent: "claude-code",
      estimatedFiles: [],
      dependencies: [],
      status: "queued",
    };
    setNodes((prev) => [...prev, newNode]);
    setEditingId(newNode.id);
    setEditTitle(newNode.title);
  }, []);

  const handleApprove = useCallback(() => {
    const updated: TaskGraph = { ...graph, nodes };
    onApprove(updated);
  }, [graph, nodes, onApprove]);

  const handleEdit = useCallback(() => {
    const updated: TaskGraph = { ...graph, nodes };
    onEdit(updated);
  }, [graph, nodes, onEdit]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      role="dialog"
      aria-modal="true"
      aria-labelledby="decompose-dialog-title"
    >
      <div className="mx-4 flex max-h-[80vh] w-full max-w-2xl flex-col rounded-lg border border-border bg-background shadow-xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-5 py-4">
          <div>
            <h2
              id="decompose-dialog-title"
              className="text-sm font-semibold text-foreground"
            >
              Task Decomposition
            </h2>
            <p className="mt-0.5 text-xs text-muted-foreground line-clamp-1">
              {graph.rootPrompt}
            </p>
          </div>
          <button
            type="button"
            onClick={onCancel}
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            aria-label="Close dialog"
          >
            <X size={16} />
          </button>
        </div>

        {/* Node list */}
        <div className="flex-1 overflow-y-auto px-5 py-3">
          <ul className="space-y-2" role="list" aria-label="Task nodes">
            {nodes.map((node, idx) => (
              <li
                key={node.id}
                className="group flex items-start gap-2 rounded border border-border/60 bg-background px-3 py-2"
              >
                {/* Drag handle / reorder */}
                <button
                  type="button"
                  onClick={() => handleMoveUp(idx)}
                  className="mt-0.5 shrink-0 text-muted-foreground/50 hover:text-foreground"
                  aria-label={`Move task ${idx + 1} up`}
                  disabled={idx === 0}
                >
                  <GripVertical size={14} />
                </button>

                {/* Content */}
                <div className="min-w-0 flex-1">
                  {editingId === node.id ? (
                    <div className="flex items-center gap-1.5">
                      <Input
                        className="flex-1 rounded border border-primary bg-background px-2 py-0.5 text-xs text-foreground focus-visible:outline-none"
                        value={editTitle}
                        onChange={(e) => setEditTitle(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") commitRename();
                          if (e.key === "Escape") setEditingId(null);
                        }}
                        autoFocus
                        aria-label="Task title"
                      />
                      <button
                        type="button"
                        onClick={commitRename}
                        className="rounded p-0.5 text-success hover:bg-accent"
                        aria-label="Confirm rename"
                      >
                        <Check size={12} />
                      </button>
                    </div>
                  ) : (
                    <div className="flex items-center gap-1.5">
                      <span className="text-xs font-medium text-foreground">
                        {idx + 1}. {node.title}
                      </span>
                      <button
                        type="button"
                        onClick={() => handleRename(node)}
                        className="rounded p-0.5 text-muted-foreground/50 opacity-0 transition hover:text-foreground group-hover:opacity-100"
                        aria-label={`Rename "${node.title}"`}
                      >
                        <Pencil size={10} />
                      </button>
                    </div>
                  )}

                  {/* Dependencies */}
                  {node.dependencies.length > 0 && (
                    <div className="mt-1 flex items-center gap-1 text-[10px] text-muted-foreground">
                      <ArrowRight size={9} />
                      depends on:{" "}
                      {node.dependencies
                        .map((depId) => {
                          const dep = nodes.find((n) => n.id === depId);
                          return dep?.title ?? depId.slice(0, 8);
                        })
                        .join(", ")}
                    </div>
                  )}

                  {/* Estimated files */}
                  {node.estimatedFiles.length > 0 && (
                    <div className="mt-0.5 text-[10px] text-muted-foreground/70 truncate">
                      Files: {node.estimatedFiles.slice(0, 3).join(", ")}
                      {node.estimatedFiles.length > 3 && ` +${node.estimatedFiles.length - 3}`}
                    </div>
                  )}
                </div>

                {/* Agent selector */}
                <NativeSelect
                  className="shrink-0 rounded border border-border bg-background px-1.5 py-0.5 text-[10px] text-foreground focus-visible:outline-none focus:border-primary"
                  value={node.suggestedAgent}
                  onChange={(e) => handleAgentChange(node.id, e.target.value as AgentType)}
                  aria-label={`Agent for "${node.title}"`}
                >
                  {Object.entries(AGENT_LABELS).map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </NativeSelect>

                {/* Remove */}
                <button
                  type="button"
                  onClick={() => handleRemove(node.id)}
                  className="shrink-0 rounded p-0.5 text-muted-foreground/50 opacity-0 transition hover:text-destructive group-hover:opacity-100"
                  aria-label={`Remove "${node.title}"`}
                >
                  <Trash2 size={12} />
                </button>
              </li>
            ))}
          </ul>

          {/* Add node */}
          <button
            type="button"
            onClick={handleAddNode}
            className="mt-2 flex items-center gap-1.5 rounded border border-dashed border-border/60 px-3 py-1.5 text-xs text-muted-foreground transition hover:border-primary hover:text-foreground"
            aria-label="Add task node"
          >
            <Plus size={12} />
            Add task
          </button>
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between border-t border-border px-5 py-3">
          <span className="text-[11px] text-muted-foreground">
            {nodes.length} task{nodes.length !== 1 ? "s" : ""}
          </span>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={onCancel}
              className="rounded border border-border px-3 py-1.5 text-xs text-foreground transition hover:bg-accent"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={handleEdit}
              className="rounded border border-border px-3 py-1.5 text-xs text-foreground transition hover:bg-accent"
            >
              Re-decompose
            </button>
            <button
              type="button"
              onClick={handleApprove}
              className="rounded bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition hover:bg-primary-hover"
              disabled={nodes.length === 0}
            >
              Approve & Start
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
