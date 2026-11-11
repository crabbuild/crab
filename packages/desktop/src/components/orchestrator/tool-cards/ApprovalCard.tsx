import { useState } from "react";
import { ShieldCheck } from "lucide-react";
import type { AgentEvent } from "../../../../electron/orchestrator-adapters";
import { useHunkActions } from "./HunkActionsContext";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

export function ApprovalCard({ event, collapsed, onToggleCollapse }: ToolCardRendererProps) {
  const taskId = useHunkActions()?.taskId;
  const payload = (event.input ?? {}) as NonNullable<AgentEvent["approval"]>;
  const [decision, setDecision] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const resolved = Boolean(payload.resolved || decision);
  const resolvedLabel = decision ?? payload.resolutionReason ?? "Approval resolved";
  const choose = async (id: string, label: string) => {
    if (!taskId || submitting || resolved) return;
    setSubmitting(true);
    setError(null);
    try {
      const result = await window.crab.orchestrator.respondApproval(taskId, { requestId: payload.requestId, decision: id, summary: payload.summary });
      if (!result.success) throw new Error(result.error ?? "Could not send the decision");
      setDecision(label);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Could not send the decision");
    } finally {
      setSubmitting(false);
    }
  };
  if (resolved && collapsed) {
    return <CollapsedCardLine summary={resolvedLabel} onExpand={() => onToggleCollapse?.(false)} ariaLabel={resolvedLabel} />;
  }
  return (
    <div className="rounded-md border border-gx-border bg-gx-surface px-3 py-2 text-xs space-y-2">
      <div className="flex items-center gap-2 font-medium text-gx-text"><ShieldCheck size={14} />{resolved ? resolvedLabel : payload.summary}</div>
      {payload.detail && <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all text-gx-muted">{payload.detail}</pre>}
      {error && <p role="alert" className="text-gx-error">{error}</p>}
      {!resolved && (
        <div className="flex flex-wrap gap-2">
          {payload.options?.map(option => (
            <button key={option.id} type="button" disabled={submitting || !taskId} onClick={() => void choose(option.id, option.label)}
              className="rounded border border-gx-border px-2 py-1 text-gx-text hover:bg-gx-surface-hover disabled:opacity-50">{option.label}</button>
          ))}
        </div>
      )}
    </div>
  );
}
