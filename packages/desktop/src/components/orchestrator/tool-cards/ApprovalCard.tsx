/**
 * Specialized renderer for sandbox approval requests.
 *
 * Codex emits an `approval_request` event when an action escalates
 * beyond the active sandbox (e.g. a shell command tries to leave the
 * worktree, or a file write targets a path outside `workspace-write`).
 * This card lets the user pick Allow, Allow once, or Deny inline in
 * the chat, with an overflow menu for persistent-rule options.
 *
 * In app-server mode the decision is delivered to the running process
 * immediately over the live JSON-RPC channel.
 *
 * The card stays visible after a decision so the user can see what
 * they previously chose, with a "Change my answer" affordance.
 */

import { useEffect, useRef, useState } from "react";
import clsx from "clsx";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  MoreHorizontal,
  ShieldCheck,
  ShieldOff,
  X,
} from "lucide-react";
import { useHunkActions } from "./HunkActionsContext";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

// ── Types ───────────────────────────────────────────────────────────

interface ApprovalPayload {
  requestId: string;
  kind: "command" | "file" | "patch" | "other";
  summary: string;
  detail?: string;
  blocking: boolean;
}

type Decision = "allow" | "allow_once" | "deny";

// ── Component ───────────────────────────────────────────────────────

export function ApprovalCard({ event, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const hunkCtx = useHunkActions();
  const taskId = hunkCtx?.taskId ?? "";
  const [expanded, setExpanded] = useState(true);
  const [decision, setDecision] = useState<Decision | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [overflowOpen, setOverflowOpen] = useState(false);
  const overflowRef = useRef<HTMLDivElement>(null);

  // Approval payload is on the event's `input` field — that's how we
  // squeezed it through the existing tool-card pipeline. Same data the
  // adapter put on `event.approval` in the AgentEvent.
  const input = (event.input ?? {}) as Partial<ApprovalPayload>;
  const payload: ApprovalPayload = {
    requestId: String(input.requestId ?? event.summary ?? ""),
    kind: (input.kind as ApprovalPayload["kind"]) ?? "other",
    summary: String(input.summary ?? event.summary ?? "Approval requested"),
    detail: typeof input.detail === "string" ? input.detail : undefined,
    blocking: Boolean(input.blocking),
  };

  // Collapse logic: while no decision has been made AND the approval is
  // blocking, force the card expanded regardless of the store. Once a
  // decision is recorded, fall back to the shared collapse rule.
  const collapsed = (decision === null && payload.blocking)
    ? false
    : (collapsedProp ?? false);

  // Close overflow menu on outside click or Escape
  useEffect(() => {
    if (!overflowOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (overflowRef.current && !overflowRef.current.contains(e.target as Node)) {
        setOverflowOpen(false);
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOverflowOpen(false);
    };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [overflowOpen]);

  const handleChoose = async (choice: Decision) => {
    if (submitting || !taskId) return;
    setSubmitting(true);
    setError(null);
    try {
      const respond = (window as any).crab?.orchestrator?.respondApproval;
      if (typeof respond !== "function") {
        throw new Error("Approval IPC unavailable");
      }
      const result = await respond(taskId, {
        requestId: payload.requestId,
        decision: choice,
        summary: payload.summary,
      });
      if (result?.success === false) {
        throw new Error(result.error ?? "Failed to record decision");
      }
      setDecision(choice);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
      setOverflowOpen(false);
    }
  };

  const decided = decision !== null;
  const tone = decided
    ? decision === "deny"
      ? "border-gx-error/30 bg-gx-error/5"
      : "border-gx-success/30 bg-gx-success/5"
    : "border-gx-warn/40 bg-gx-warn/5";

  // Collapsed one-liner: show a summary with the decision status.
  if (collapsed) {
    const summaryIcon = decided
      ? decision === "deny"
        ? <ShieldOff size={11} className="shrink-0 text-gx-error" />
        : <ShieldCheck size={11} className="shrink-0 text-gx-success" />
      : <AlertTriangle size={11} className="shrink-0 text-gx-warn" />;
    const summaryLabel = decided
      ? decision === "deny" ? "Denied" : decision === "allow_once" ? "Allowed once" : "Allowed"
      : "Approval requested";
    const defaultSummary = (
      <>
        {summaryIcon}
        <span className="font-medium text-gx-text/80">{summaryLabel}</span>
        <span className="truncate text-gx-muted">{payload.summary}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Approval: ${payload.summary}`}
      />
    );
  }

  return (
    <div className={clsx("rounded-md border overflow-hidden transition-colors", tone)}>
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left hover:bg-gx-surface-hover transition cursor-pointer"
      >
        {decided
          ? decision === "deny"
            ? <ShieldOff size={11} className="shrink-0 text-gx-error" />
            : <ShieldCheck size={11} className="shrink-0 text-gx-success" />
          : <AlertTriangle size={11} className="shrink-0 text-gx-warn" />}
        <span className={clsx(
          "shrink-0 font-medium",
          decided
            ? decision === "deny" ? "text-gx-error" : "text-gx-success"
            : "text-gx-warn",
        )}>
          {decided
            ? decision === "deny"
              ? "Denied"
              : decision === "allow_once"
                ? "Allowed once"
                : "Allowed"
            : payload.blocking ? "Approval needed" : "Approval requested"}
        </span>
        <span className="min-w-0 flex-1 truncate text-gx-text/80">{payload.summary}</span>
        {expanded
          ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
          : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />}
      </button>

      {expanded && (
        <div className="border-t border-gx-border/30 px-2.5 py-2 space-y-2">
          {payload.detail && (
            <pre className="font-mono text-[10px] leading-[1.5] text-gx-text/70 whitespace-pre-wrap break-all max-h-[160px] overflow-auto rounded border border-gx-border/30 bg-gx-bg/40 px-2 py-1.5">
              {payload.detail}
            </pre>
          )}

          <p className="text-[10px] text-gx-muted leading-relaxed">
            Your choice is delivered to the agent immediately.
          </p>

          {error && (
            <p className="text-[10px] text-gx-error">⚠ {error}</p>
          )}

          {!decided && (
            <div className="flex flex-wrap items-center gap-1.5">
              <button
                type="button"
                disabled={submitting}
                onClick={() => handleChoose("allow")}
                className="inline-flex items-center gap-1 rounded border border-gx-success/30 bg-gx-success/10 px-2 py-1 text-[10px] font-medium text-gx-success hover:bg-gx-success/20 transition disabled:opacity-50 cursor-pointer"
              >
                <Check size={10} />
                Allow
              </button>
              <button
                type="button"
                disabled={submitting}
                onClick={() => handleChoose("allow_once")}
                className="inline-flex items-center gap-1 rounded border border-gx-border/40 bg-gx-surface px-2 py-1 text-[10px] font-medium text-gx-text hover:bg-gx-surface-hover transition disabled:opacity-50 cursor-pointer"
              >
                Allow once
              </button>
              <button
                type="button"
                disabled={submitting}
                onClick={() => handleChoose("deny")}
                className="inline-flex items-center gap-1 rounded border border-gx-error/30 bg-gx-error/5 px-2 py-1 text-[10px] font-medium text-gx-error hover:bg-gx-error/15 transition disabled:opacity-50 cursor-pointer"
              >
                <X size={10} />
                Deny
              </button>

              {/* Overflow menu for persistent-rule options */}
              <div className="relative" ref={overflowRef}>
                <button
                  type="button"
                  disabled={submitting}
                  onClick={() => setOverflowOpen((v) => !v)}
                  className="inline-flex items-center justify-center rounded border border-gx-border/40 bg-gx-surface px-1.5 py-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition disabled:opacity-50 cursor-pointer"
                  aria-label="More approval options"
                >
                  <MoreHorizontal size={12} />
                </button>
                {overflowOpen && (
                  <div className="absolute right-0 bottom-full z-50 mb-1 w-56 rounded border border-gx-border bg-gx-surface shadow-xl py-1">
                    <button
                      type="button"
                      className="flex w-full items-center gap-2 px-3 py-1.5 text-[10px] text-gx-text hover:bg-gx-accent/10 transition text-left"
                      title="Stores the rule in Codex's session policy. Editor for the rule itself is coming in v2; for now this behaves like Allow."
                      onClick={() => handleChoose("allow")}
                    >
                      Allow with persistent rule
                    </button>
                    {payload.kind === "command" && (
                      <button
                        type="button"
                        className="flex w-full items-center gap-2 px-3 py-1.5 text-[10px] text-gx-text hover:bg-gx-accent/10 transition text-left"
                        title="Stores the rule in Codex's session policy. Editor for the rule itself is coming in v2; for now this behaves like Allow."
                        onClick={() => handleChoose("allow")}
                      >
                        Apply network policy rule
                      </button>
                    )}
                  </div>
                )}
              </div>
            </div>
          )}

          {decided && (
            <button
              type="button"
              onClick={() => setDecision(null)}
              className="text-[10px] text-gx-muted hover:text-gx-text underline-offset-2 hover:underline"
            >
              Change my answer
            </button>
          )}
        </div>
      )}
    </div>
  );
}
