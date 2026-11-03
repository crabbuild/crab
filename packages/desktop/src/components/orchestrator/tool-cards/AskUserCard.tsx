/**
 * Specialized renderer for AskUserQuestion tool calls.
 *
 * Shows the question with multiple-choice options in a clean
 * card format. When the question hasn't been answered yet (no
 * tool result), shows interactive buttons so the user can respond
 * directly from the activity feed. The response is sent as
 * iteration feedback to the orchestrator.
 *
 * Highlights the selected answer if available in the tool result.
 */

import { useCallback, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  HelpCircle,
  CircleDot,
  CheckCircle2,
  Send,
} from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Component ───────────────────────────────────────────────────────

export function AskUserCard({ event, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useState(true); // Default expanded for questions
  const [freeformText, setFreeformText] = useState("");

  const input = event.input as Record<string, unknown> | undefined;
  const question = String(input?.question ?? input?.text ?? input?.prompt ?? "");
  const choices = parseChoices(input?.choices ?? input?.options);
  const selectedAnswer = event.detail ?? "";
  const isAnswered = selectedAnswer.length > 0;

  // Collapsed one-liner branch. Only collapse if answered — unanswered
  // questions should stay visible like ApprovalCard.
  const collapsed = isAnswered ? (collapsedProp ?? false) : false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <CheckCircle2 size={11} className="shrink-0 text-gx-success" />
        <span className="font-medium text-gx-text/80">Question (answered)</span>
        <span className="truncate text-gx-text/70">{question.slice(0, 50)}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Question: ${question.slice(0, 50)}`}
      />
    );
  }

  // Callback to handle choice selection — dispatches a custom event
  // that the parent ChatTab/AgentTasksView can listen for and route
  // to the orchestrator's iterate() method.
  const handleChoiceSelect = useCallback((choice: string) => {
    window.dispatchEvent(
      new CustomEvent("orchestrator:answer-question", {
        detail: { answer: choice },
      }),
    );
  }, []);

  const handleFreeformSubmit = useCallback(() => {
    if (!freeformText.trim()) return;
    window.dispatchEvent(
      new CustomEvent("orchestrator:answer-question", {
        detail: { answer: freeformText.trim() },
      }),
    );
    setFreeformText("");
  }, [freeformText]);

  return (
    <div className={clsx(
      "rounded-md border overflow-hidden",
      isAnswered
        ? "border-gx-border/40"
        : "border-gx-warning/40 bg-gx-warning/5",
    )}>
      {/* Header */}
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left hover:bg-gx-surface-hover transition cursor-pointer"
      >
        <HelpCircle size={11} className={clsx(
          "shrink-0",
          isAnswered ? "text-gx-muted" : "text-gx-warning",
        )} />
        <span className={clsx(
          "shrink-0 font-medium",
          isAnswered ? "text-gx-muted" : "text-gx-warning",
        )}>
          {isAnswered ? "Question (answered)" : "Question (awaiting response)"}
        </span>
        <span className="min-w-0 flex-1 truncate text-gx-text/80">
          {question.slice(0, 80)}{question.length > 80 ? "…" : ""}
        </span>

        {isAnswered && (
          <CheckCircle2 size={10} className="shrink-0 text-gx-success" />
        )}

        {expanded
          ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
          : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        }
      </button>

      {/* Expanded: question + choices */}
      {expanded && (
        <div className="border-t border-gx-border/30 px-2.5 py-2 space-y-2">
          {/* Full question */}
          {question.length > 80 && (
            <p className="text-[10px] text-gx-text/80 leading-relaxed">{question}</p>
          )}

          {/* Choices */}
          {choices.length > 0 && (
            <div className="space-y-1">
              {choices.map((choice, idx) => {
                const isSelected = selectedAnswer.includes(choice) ||
                  selectedAnswer.includes(String(idx + 1));
                return (
                  <button
                    key={idx}
                    type="button"
                    onClick={() => !isAnswered && handleChoiceSelect(choice)}
                    disabled={isAnswered}
                    className={clsx(
                      "flex w-full items-center gap-2 rounded px-2 py-1.5 text-[10px] border transition text-left",
                      isSelected
                        ? "border-gx-accent/40 bg-gx-accent/10 text-gx-text"
                        : isAnswered
                          ? "border-gx-border/30 text-gx-text/70 cursor-default"
                          : "border-gx-border/30 text-gx-text/70 hover:border-gx-accent/30 hover:bg-gx-accent/5 cursor-pointer",
                    )}
                  >
                    <CircleDot size={10} className={clsx(
                      isSelected ? "text-gx-accent" : "text-gx-muted/50",
                    )} />
                    <span className="flex-1">{choice}</span>
                    {isSelected && (
                      <span className="text-[8px] font-medium text-gx-accent">selected</span>
                    )}
                  </button>
                );
              })}
            </div>
          )}

          {/* Free-form answer input (when no choices or not yet answered) */}
          {!isAnswered && choices.length === 0 && (
            <div className="flex items-center gap-1.5">
              <input
                type="text"
                value={freeformText}
                onChange={(e) => setFreeformText(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleFreeformSubmit();
                }}
                placeholder="Type your answer…"
                className="flex-1 rounded border border-gx-border bg-gx-bg px-2 py-1 text-[10px] text-gx-text placeholder:text-gx-muted/50 focus:border-gx-accent focus:outline-none"
                aria-label="Answer to agent question"
              />
              <button
                type="button"
                onClick={handleFreeformSubmit}
                disabled={!freeformText.trim()}
                className={clsx(
                  "rounded p-1 transition",
                  freeformText.trim()
                    ? "text-gx-accent hover:bg-gx-accent/10 cursor-pointer"
                    : "text-gx-muted/40 cursor-default",
                )}
                aria-label="Send answer"
              >
                <Send size={12} />
              </button>
            </div>
          )}

          {/* Answered: show the response */}
          {isAnswered && choices.length === 0 && (
            <div className="flex items-center gap-2 rounded border border-gx-success/30 bg-gx-success/5 px-2 py-1.5 text-[10px] text-gx-text/80">
              <CheckCircle2 size={10} className="text-gx-success shrink-0" />
              <span>{selectedAnswer}</span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function parseChoices(raw: unknown): string[] {
  if (Array.isArray(raw)) {
    return raw.map((item) => {
      if (typeof item === "string") return item;
      if (typeof item === "object" && item !== null) {
        const obj = item as Record<string, unknown>;
        return String(obj.label ?? obj.text ?? obj.value ?? JSON.stringify(item));
      }
      return String(item);
    });
  }
  return [];
}
