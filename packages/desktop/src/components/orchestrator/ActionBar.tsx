import {
Check,
MessageSquare,
Play,
RotateCcw,
Trash2,
X,
} from "lucide-react";
import { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  task: OrchestratorTask;
  isStreaming: boolean;
  onStop: () => void;
  onRetry: () => void;
  onRestore: () => void;
  onDiscard: () => void;
  onArchive: () => void;
  onApprovePlan: () => void;
  onRefinePlan: () => void;
  onRejectPlan: () => void;
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Minimal action bar — only shows lifecycle controls.
 * Apply/PR/Review actions live in the Diff tab toolbar.
 */
export function ActionBar(props: Props) {
  const { task } = props;

  // Running / iterating → no action bar (stop button is in the chat input)
  if (task.status === "running" || task.status === "iterating") {
    return null;
  }

  // Queued → Cancel
  if (task.status === "queued") {
    return (
      <Bar>
        <ActionButton label="Cancel" icon={<X size={12} />} onClick={props.onStop} variant="danger" />
      </Bar>
    );
  }

  // Planning → Approve / Refine / Reject (only when agent has finished producing the plan)
  if (task.status === "planning" && !props.isStreaming) {
    return (
      <Bar>
        <ActionButton label="Approve plan" icon={<Check size={12} />} onClick={props.onApprovePlan} variant="primary" />
        <ActionButton label="Refine" icon={<MessageSquare size={12} />} onClick={props.onRefinePlan} />
        <ActionButton label="Reject" icon={<Trash2 size={12} />} onClick={props.onRejectPlan} variant="danger" />
      </Bar>
    );
  }

  // Planning but still streaming → show a "Planning…" indicator
  if (task.status === "planning" && props.isStreaming) {
    return null;
  }

  // Applied → Reopen the conversation
  if (task.status === "applied") {
    return (
      <Bar>
        <ActionButton label="Reopen task" icon={<RotateCcw size={12} />} onClick={props.onRestore} />
      </Bar>
    );
  }

  // Failed / stopped / interrupted → Retry + Discard
  if (task.status === "failed" || task.status === "stopped" || task.status === "interrupted") {
    return (
      <Bar>
        <ActionButton label="Retry" icon={<Play size={12} />} onClick={props.onRetry} variant="primary" />
        <ActionButton label="Discard" icon={<Trash2 size={12} />} onClick={props.onDiscard} variant="danger" />
      </Bar>
    );
  }

  // Archived → Restore
  if (task.status === "archived") {
    return (
      <Bar>
        <ActionButton label="Restore" icon={<RotateCcw size={12} />} onClick={props.onRestore} />
      </Bar>
    );
  }

  // Reviewing → no action bar (actions are in the Diff tab)
  return null;
}

// ── Primitives ──────────────────────────────────────────────────────

function Bar({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex shrink-0 items-center gap-2 border-t border-gx-border px-4 py-2 bg-gx-surface">
      {children}
    </div>
  );
}

function ActionButton({
  label,
  icon,
  onClick,
  variant = "default",
}: {
  label: string;
  icon: React.ReactNode;
  onClick: () => void;
  variant?: "primary" | "danger" | "default";
}) {
  const cls =
    variant === "primary"
      ? "bg-gx-accent text-white hover:bg-gx-accent/80"
      : variant === "danger"
        ? "border border-gx-error/40 text-gx-error hover:bg-gx-error/10"
        : "border border-gx-border text-gx-text hover:bg-gx-surface-hover";

  return (
    <button
      type="button"
      onClick={onClick}
      className={`flex items-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium transition ${cls}`}
    >
      {icon}
      {label}
    </button>
  );
}
