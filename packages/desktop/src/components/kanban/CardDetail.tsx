import clsx from "clsx";
import type {
  KanbanCardInfo,
  KanbanPipelineState,
} from "../../ipc";

interface CardDetailProps {
  card: KanbanCardInfo;
  onClose: () => void;
}

const PIPELINE_STATE_LABELS: Record<KanbanPipelineState, string> = {
  idle: "Idle",
  agent_running: "Agent Running",
  pr_created: "PR Created",
  ci_pending: "CI Pending",
  ci_passed: "CI Passed",
  ci_failed: "CI Failed",
  review_pending: "Review Pending",
  review_approved: "Review Approved",
  review_changes_requested: "Changes Requested",
  merging: "Merging",
  done: "Done",
  blocked: "Blocked",
};

const PIPELINE_STATE_ORDER: KanbanPipelineState[] = [
  "idle",
  "agent_running",
  "pr_created",
  "ci_pending",
  "ci_passed",
  "review_pending",
  "review_approved",
  "merging",
  "done",
];

function getStateIndex(state: KanbanPipelineState): number {
  const idx = PIPELINE_STATE_ORDER.indexOf(state);
  return idx >= 0 ? idx : -1;
}

/**
 * Expanded card detail panel showing pipeline state, state transitions,
 * and approve/reject/cancel action buttons. Rendered as a slide-over
 * panel on the right side of the board.
 */
export function CardDetail({ card, onClose }: CardDetailProps) {
  const currentStateIdx = getStateIndex(card.pipelineState);
  const isBlocked = card.pipelineState === "blocked";
  const isCIFailed = card.pipelineState === "ci_failed";
  const isChangesRequested = card.pipelineState === "review_changes_requested";
  const isAwaitingMerge =
    card.pipelineState === "merging" ||
    card.pipelineState === "review_approved";
  const hasPipeline = card.pipelineState !== "idle" && card.pipelineState !== "done";

  async function handleApproveMerge() {
    try {
      await window.crab.kanban.approveMerge(card.id);
    } catch {
      // Error handled by IPC layer
    }
  }

  async function handleCancelPipeline() {
    try {
      await window.crab.kanban.cancelPipeline(card.id);
    } catch {
      // Error handled by IPC layer
    }
  }

  return (
    <div className="flex h-full w-80 flex-col border-l border-gx-border bg-gx-surface">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-gx-border p-3">
        <h2 className="text-sm font-medium text-gx-text truncate">
          {card.externalId || "Card Detail"}
        </h2>
        <button
          onClick={onClose}
          className="btn-icon text-gx-muted hover:text-gx-text"
          aria-label="Close detail panel"
        >
          ✕
        </button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-3 space-y-4">
        {/* Title & Description */}
        <section>
          <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-1">
            Title
          </h3>
          <p className="text-sm text-gx-text leading-snug">{card.title}</p>
          {card.description && (
            <p className="mt-1.5 text-xs text-gx-muted leading-relaxed line-clamp-6">
              {card.description}
            </p>
          )}
        </section>

        {/* Pipeline State */}
        <section>
          <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-2">
            Pipeline State
          </h3>
          <div className="flex items-center gap-2">
            <span
              className={clsx(
                "chip px-2 py-0.5 text-xs",
                isBlocked && "border-gx-error/50 text-gx-error",
                isCIFailed && "border-gx-error/50 text-gx-error",
                isChangesRequested && "border-gx-warn/50 text-gx-warn",
                card.pipelineState === "done" && "border-gx-success/50 text-gx-success",
                card.pipelineState === "agent_running" && "border-gx-thinking/50 text-gx-thinking",
              )}
            >
              {PIPELINE_STATE_LABELS[card.pipelineState]}
            </span>
            {card.iterationCount > 0 && (
              <span className="text-xs text-gx-muted">
                Iteration {card.iterationCount}
              </span>
            )}
          </div>
        </section>

        {/* State Transitions Timeline */}
        <section>
          <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-2">
            State Transitions
          </h3>
          <div className="space-y-1">
            {PIPELINE_STATE_ORDER.map((state, idx) => {
              const isCompleted = currentStateIdx > idx;
              const isCurrent = card.pipelineState === state;
              return (
                <div key={state} className="flex items-center gap-2">
                  <span
                    className={clsx(
                      "h-2 w-2 rounded-full shrink-0",
                      isCompleted && "bg-gx-success",
                      isCurrent && "bg-gx-accent",
                      !isCompleted && !isCurrent && "bg-gx-border",
                    )}
                  />
                  <span
                    className={clsx(
                      "text-xs",
                      isCurrent && "text-gx-text font-medium",
                      isCompleted && "text-gx-muted",
                      !isCompleted && !isCurrent && "text-gx-muted/50",
                    )}
                  >
                    {PIPELINE_STATE_LABELS[state]}
                  </span>
                </div>
              );
            })}
            {(isBlocked || isCIFailed || isChangesRequested) && (
              <div className="flex items-center gap-2">
                <span className="h-2 w-2 rounded-full shrink-0 bg-gx-error" />
                <span className="text-xs text-gx-error font-medium">
                  {PIPELINE_STATE_LABELS[card.pipelineState]}
                </span>
              </div>
            )}
          </div>
        </section>

        {/* PR Link */}
        {card.linkedPrUrl && (
          <section>
            <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-1">
              Pull Request
            </h3>
            <a
              href={card.linkedPrUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="text-xs text-gx-accent hover:underline"
            >
              PR #{card.linkedPrNumber}
            </a>
          </section>
        )}

        {/* Labels */}
        {card.labels.length > 0 && (
          <section>
            <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-1">
              Labels
            </h3>
            <div className="flex flex-wrap gap-1">
              {card.labels.map((label) => (
                <span key={label} className="chip text-[10px] px-1.5 py-0">
                  {label}
                </span>
              ))}
            </div>
          </section>
        )}

        {/* Assignee */}
        {card.assignee && (
          <section>
            <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-1">
              Assignee
            </h3>
            <div className="flex items-center gap-2">
              {card.assigneeAvatarUrl ? (
                <img
                  src={card.assigneeAvatarUrl}
                  alt={card.assignee}
                  className="h-5 w-5 rounded-full"
                />
              ) : (
                <span className="flex h-5 w-5 items-center justify-center rounded-full bg-gx-accent/20 text-[10px] text-gx-accent">
                  {card.assignee.charAt(0).toUpperCase()}
                </span>
              )}
              <span className="text-xs text-gx-text">{card.assignee}</span>
            </div>
          </section>
        )}

        {/* External Link */}
        {card.externalUrl && (
          <section>
            <h3 className="text-xs font-medium text-gx-muted uppercase tracking-wide mb-1">
              External Issue
            </h3>
            <a
              href={card.externalUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="text-xs text-gx-accent hover:underline"
            >
              {card.externalId || "View Issue"}
            </a>
          </section>
        )}
      </div>

      {/* Action Buttons */}
      {hasPipeline && (
        <div className="border-t border-gx-border p-3 space-y-2">
          {isAwaitingMerge && (
            <button
              onClick={handleApproveMerge}
              className="btn-primary w-full text-xs py-1.5"
            >
              Approve Merge
            </button>
          )}
          <button
            onClick={handleCancelPipeline}
            className="btn-danger w-full text-xs py-1.5"
          >
            Cancel Pipeline
          </button>
        </div>
      )}
    </div>
  );
}
