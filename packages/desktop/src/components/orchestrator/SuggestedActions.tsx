import {
  Archive,
  Ban,
  Eye,
  FileText,
  GitMerge,
  GitPullRequest,
  MessageSquare,
  Play,
  Square,
  Trash2,
  Wrench,
  X,
} from "lucide-react";
import type { OrchestratorTask } from "./TaskCard";

// ── Types ───────────────────────────────────────────────────────────

export interface SuggestedAction {
  label: string;
  action: string;
  icon: React.ReactNode;
  primary?: boolean;
  disabled?: boolean;
}

export interface AutoFixInfo {
  attempts: number;
  maxAttempts: number;
}

export interface SuggestedActionsProps {
  task: OrchestratorTask;
  /** Whether CI checks are passing (relevant for merge action). */
  checksPass?: boolean;
  /** Auto-fix state when task is in auto-fixing mode. */
  autoFix?: AutoFixInfo;
  onReviewDiff?: () => void;
  onCreatePr?: () => void;
  onSendFeedback?: () => void;
  onViewPr?: () => void;
  onMerge?: () => void;
  onArchive?: () => void;
  onViewLogs?: () => void;
  onRetry?: () => void;
  onDiscard?: () => void;
  onStopAutoFix?: () => void;
  onStop?: () => void;
  onCancel?: () => void;
}

// ── Action logic ────────────────────────────────────────────────────

export function getSuggestedActions(
  task: OrchestratorTask,
  checksPass?: boolean,
  autoFix?: AutoFixInfo,
): SuggestedAction[] {
  switch (task.status) {
    case "reviewing":
      if (!task.prNumber) {
        return [
          { label: "Review diff", action: "open-diff", icon: <Eye size={14} />, primary: true },
          { label: "Create PR", action: "create-pr", icon: <GitPullRequest size={14} /> },
          { label: "Send feedback", action: "iterate", icon: <MessageSquare size={14} /> },
        ];
      }
      return [
        { label: "View PR", action: "open-pr", icon: <GitPullRequest size={14} />, primary: true },
        { label: "Merge", action: "merge", icon: <GitMerge size={14} />, disabled: !checksPass },
      ];

    case "applied":
      return [
        { label: "Archive", action: "archive", icon: <Archive size={14} />, primary: true },
      ];

    case "failed":
    case "stopped":
    case "interrupted":
      return [
        { label: "View logs", action: "open-activity", icon: <FileText size={14} />, primary: true },
        { label: "Retry", action: "retry", icon: <Play size={14} /> },
        { label: "Discard", action: "discard", icon: <Trash2 size={14} /> },
      ];

    case "running":
    case "iterating":
      return [
        { label: "Stop", action: "stop", icon: <Square size={14} />, primary: true },
      ];

    case "queued":
      return [
        { label: "Cancel", action: "cancel", icon: <X size={14} />, primary: true },
      ];

    default:
      break;
  }

  // Handle "auto-fixing" — not in the base TaskStatus union but may
  // appear via extended status. Check autoFix prop as a signal.
  if (autoFix) {
    return [
      {
        label: `Auto-fix ${autoFix.attempts}/${autoFix.maxAttempts}`,
        action: "none",
        icon: <Wrench size={14} />,
        disabled: true,
      },
      { label: "Stop auto-fix", action: "cancel-autofix", icon: <Ban size={14} /> },
    ];
  }

  return [];
}

// ── Action dispatcher ───────────────────────────────────────────────

function dispatchAction(action: string, props: SuggestedActionsProps): void {
  switch (action) {
    case "open-diff":
      props.onReviewDiff?.();
      break;
    case "create-pr":
      props.onCreatePr?.();
      break;
    case "iterate":
      props.onSendFeedback?.();
      break;
    case "open-pr":
      props.onViewPr?.();
      break;
    case "merge":
      props.onMerge?.();
      break;
    case "archive":
      props.onArchive?.();
      break;
    case "open-activity":
      props.onViewLogs?.();
      break;
    case "retry":
      props.onRetry?.();
      break;
    case "discard":
      props.onDiscard?.();
      break;
    case "cancel-autofix":
      props.onStopAutoFix?.();
      break;
    case "stop":
      props.onStop?.();
      break;
    case "cancel":
      props.onCancel?.();
      break;
  }
}

// ── Component ───────────────────────────────────────────────────────

export function SuggestedActions(props: SuggestedActionsProps) {
  const { task, checksPass, autoFix } = props;
  const actions = getSuggestedActions(task, checksPass, autoFix);

  if (actions.length === 0) return null;

  return (
    <div
      className="flex shrink-0 flex-wrap items-center gap-2 border-t border-border bg-card px-4 py-2.5"
      role="toolbar"
      aria-label="Suggested actions"
    >
      {actions.map((action) => (
        <button
          key={action.action + action.label}
          type="button"
          disabled={action.disabled}
          onClick={() => dispatchAction(action.action, props)}
          aria-label={action.label}
          className={
            action.primary
              ? "flex items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition hover:bg-primary/80 disabled:cursor-not-allowed disabled:opacity-50"
              : "flex items-center gap-1.5 rounded-md border border-border px-3 py-1.5 text-xs font-medium text-foreground transition hover:bg-accent disabled:cursor-not-allowed disabled:opacity-50"
          }
        >
          {action.icon}
          {action.label}
        </button>
      ))}
    </div>
  );
}
