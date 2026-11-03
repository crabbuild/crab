import { Bot, Check, Plus } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

interface DetectedAgent {
  name: string;
  available: boolean;
}

interface Props {
  agents: DetectedAgent[];
  onCreateTask: () => void;
}

// ── Component ───────────────────────────────────────────────────────

export function EmptyState({ agents, onCreateTask }: Props) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-5 p-8 text-center">
      <Bot size={40} className="text-gx-muted/60" />

      <div>
        <h2 className="text-base font-semibold text-gx-text">Delegate work to AI agents</h2>
        <p className="mt-2 max-w-[360px] text-xs text-gx-muted leading-relaxed">
          Create isolated workspaces where Claude Code or Codex can implement features, fix bugs, and
          refactor code autonomously. Review their changes, then apply.
        </p>
      </div>

      {/* How it works */}
      <div className="w-full max-w-[360px] rounded-lg border border-gx-border bg-gx-surface p-4 text-left">
        <p className="mb-2 text-[11px] font-medium text-gx-text">How it works:</p>
        <ol className="space-y-1.5 text-[11px] text-gx-muted">
          <li className="flex gap-2">
            <span className="shrink-0 font-mono text-gx-accent">1.</span>
            Describe a task → agent works in isolation
          </li>
          <li className="flex gap-2">
            <span className="shrink-0 font-mono text-gx-accent">2.</span>
            Review the diff → leave comments, iterate
          </li>
          <li className="flex gap-2">
            <span className="shrink-0 font-mono text-gx-accent">3.</span>
            Apply changes → merge into your branch
          </li>
        </ol>
      </div>

      {/* Detected agents */}
      {agents.length > 0 && (
        <div className="flex items-center gap-3 text-[11px] text-gx-muted">
          <span>Agents detected:</span>
          {agents.map((a) => (
            <span key={a.name} className="flex items-center gap-1">
              {a.available ? (
                <Check size={10} className="text-gx-success" />
              ) : (
                <span className="h-2 w-2 rounded-full bg-gray-400" />
              )}
              {a.name}
            </span>
          ))}
        </div>
      )}

      {/* CTA */}
      <button
        type="button"
        onClick={onCreateTask}
        className="flex items-center gap-2 rounded-md bg-gx-accent px-4 py-2 text-sm font-medium text-white hover:bg-gx-accent/80 transition"
      >
        <Plus size={14} /> Create your first task
      </button>
    </div>
  );
}
