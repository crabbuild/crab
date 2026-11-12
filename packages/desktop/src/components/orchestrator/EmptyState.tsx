import { Button } from "@/components/ui/button";
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
      <Bot size={40} className="text-muted-foreground/60" />

      <div>
        <h2 className="text-base font-semibold text-foreground">
          Delegate work to AI agents
        </h2>
        <p className="mt-2 max-w-[360px] text-xs text-muted-foreground leading-relaxed">
          Create isolated workspaces where Claude Code or Codex can implement
          features, fix bugs, and refactor code autonomously. Review their
          changes, then apply.
        </p>
      </div>

      {/* How it works */}
      <div className="w-full max-w-[360px] rounded-lg border border-border bg-card p-4 text-left">
        <p className="mb-2 text-[11px] font-medium text-foreground">
          How it works:
        </p>
        <ol className="space-y-1.5 text-[11px] text-muted-foreground">
          <li className="flex gap-2">
            <span className="shrink-0 font-mono text-primary">1.</span>
            Describe a task → agent works in isolation
          </li>
          <li className="flex gap-2">
            <span className="shrink-0 font-mono text-primary">2.</span>
            Review the diff → leave comments, iterate
          </li>
          <li className="flex gap-2">
            <span className="shrink-0 font-mono text-primary">3.</span>
            Apply changes → merge into your branch
          </li>
        </ol>
      </div>

      {/* Detected agents */}
      {agents.length > 0 && (
        <div className="flex items-center gap-3 text-[11px] text-muted-foreground">
          <span>Agents detected:</span>
          {agents.map((a) => (
            <span key={a.name} className="flex items-center gap-1">
              {a.available ? (
                <Check size={10} className="text-success" />
              ) : (
                <span className="h-2 w-2 rounded-full bg-accent" />
              )}
              {a.name}
            </span>
          ))}
        </div>
      )}

      {/* CTA */}
      <Button
        variant="default"
        size="compact"
        type="button"
        onClick={onCreateTask}
        className="flex items-center gap-2 rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/80 transition"
      >
        <Plus size={14} /> Create your first task
      </Button>
    </div>
  );
}
