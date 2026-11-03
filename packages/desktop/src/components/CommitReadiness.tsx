/**
 * Commit readiness indicator — shows a visual checklist of what's
 * needed before a commit can be made. Helps users unfamiliar with
 * git understand the commit prerequisites.
 */
import { Check, Circle } from "lucide-react";
import clsx from "clsx";

interface CommitReadinessProps {
  stagedCount: number;
  hasMessage: boolean;
  /** When true, all checks pass and the commit button is enabled. */
  ready: boolean;
}

export function CommitReadiness({ stagedCount, hasMessage, ready }: CommitReadinessProps) {
  return (
    <div className="flex items-center gap-3 px-3 py-1 text-[10px]">
      <ReadinessItem
        done={stagedCount > 0}
        label={stagedCount > 0 ? `${stagedCount} file${stagedCount === 1 ? "" : "s"} staged` : "Stage files"}
      />
      <ReadinessItem
        done={hasMessage}
        label={hasMessage ? "Message added" : "Add message"}
      />
      {ready && (
        <span className="ml-auto text-gx-success font-medium">Ready to commit</span>
      )}
    </div>
  );
}

function ReadinessItem({ done, label }: { done: boolean; label: string }) {
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1",
        done ? "text-gx-success" : "text-gx-muted",
      )}
    >
      {done ? <Check size={10} /> : <Circle size={10} />}
      {label}
    </span>
  );
}
