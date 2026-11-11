/**
 * CommandSessionCard — renders Codex `command/exec` session output.
 *
 * Unlike BashCard (which renders agent tool calls), this card renders
 * the output of `command/exec` protocol sessions — background commands
 * launched by the orchestrator or renderer, streaming output through
 * the `command_output` AgentEvent delta by delta. Events are grouped
 * by `commandSessionId` so multiple concurrent sessions render
 * independently.
 *
 * When the session is still running, deltas append in real time. When
 * `command_exited` arrives, the exit code badge is shown and the
 * running spinner stops.
 */

import { memo, useState, useEffect, useRef, useCallback } from "react";
import clsx from "clsx";
import {
  Terminal,
  ChevronDown,
  ChevronRight,
  Copy,
  Check,
  Loader2,
  XCircle,
} from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import type { AgentEvent } from "../../../../electron/orchestrator-adapters";

type Props = Omit<ToolCardRendererProps, "event"> & { event: AgentEvent };
import { CollapsedCardLine } from "./CollapsedCardLine";
import { useCardExpanded } from "./useCardExpanded";
import { AnsiText } from "../../../lib/ansi-text";

// ── Helpers ─────────────────────────────────────────────────────────

const OUTPUT_TRUNCATION_LIMIT = 8000;

function truncateOutput(output: string): { text: string; truncated: boolean } {
  if (output.length <= OUTPUT_TRUNCATION_LIMIT) return { text: output, truncated: false };
  return {
    text: output.slice(0, OUTPUT_TRUNCATION_LIMIT),
    truncated: true,
  };
}

export function CommandSessionCard({
  event,
  collapsed: collapsedProp,
  onToggleCollapse,
  collapsedSummary,
}: Props) {
  // Use the unique id from card rendering context
  const sessionId = String(event.commandSessionId ?? event.itemId ?? "");
  const isRunning = event.type === "command_output" && event.content != null;
  const isExited = event.type === "command_exited";
  const exitCode = isExited ? (event.exitCode ?? null) : null;
  const [copied, setCopied] = useState(false);

  // Accumulate deltas (this card instance may receive progressive renders)
  const [accumulated, setAccumulated] = useState("");
  const prevContent = useRef("");

  // Append new deltas as they arrive
  useEffect(() => {
    if (event.type === "command_output" && event.content) {
      const delta = String(event.content);
      setAccumulated(prev => {
        const next = prev + delta;
        prevContent.current = next;
        return next;
      });
    }
  }, [event.content, event.type]);

  const collapsed = collapsedProp ?? false;
  const hasOutput = accumulated.length > 0;
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, !isRunning && isExited);
  const toggle = () => setExpanded(!expanded);

  // Copy entire output
  const handleCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(accumulated);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch { /* clipboard denied */ }
  }, [accumulated]);

  const { text, truncated } = truncateOutput(accumulated);

  // ── Collapsed ────────────────────────────────────────────────────
  if (collapsed) {
    const defaultSummary = (
      <>
        <Terminal size={11} className="shrink-0 text-muted-foreground" />
        <span className="font-medium text-foreground/80">
          {isRunning ? "Running" : "Command"}
        </span>
        {exitCode !== null && (
          <span className={clsx(
            "rounded px-1 py-0.5 text-[8px] font-mono",
            exitCode === 0
              ? "bg-success/10 text-success"
              : "bg-destructive/10 text-destructive",
          )}>
            {exitCode === 0 ? "✓ ok" : `exit ${exitCode}`}
          </span>
        )}
        {accumulated && (
          <span className="truncate text-foreground/50 text-[10px]">
            {accumulated.slice(0, 80).replace(/\n/g, " ")}
          </span>
        )}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Command session ${sessionId}`}
      />
    );
  }

  // ── Expanded ─────────────────────────────────────────────────────
  return (
    <div className={clsx(
      "rounded-md border overflow-hidden",
      isRunning
        ? "border-info/30 bg-card/20"
        : exitCode === 0
          ? "border-border/40 bg-card/30"
          : "border-destructive/30 bg-destructive/5",
    )}>
      {/* Header */}
      <button
        type="button"
        onClick={hasOutput ? toggle : undefined}
        className={clsx(
          "w-full flex items-center gap-1.5 px-2.5 py-1.5 group",
          hasOutput ? "cursor-pointer hover:bg-accent/50" : "cursor-default",
        )}
        aria-expanded={expanded}
        aria-label={hasOutput ? "Toggle output" : "Command session"}
      >
        {isRunning ? (
          <Loader2 size={11} className="shrink-0 text-info animate-spin" />
        ) : exitCode !== 0 ? (
          <XCircle size={11} className="shrink-0 text-destructive" />
        ) : (
          <Terminal size={11} className="shrink-0 text-muted-foreground" />
        )}

        <span className="text-[10px] font-medium text-foreground/80">
          {isRunning ? "Running" : "Command"}
        </span>

        {/* Exit code badge */}
        {exitCode !== null && (
          <span className={clsx(
            "shrink-0 rounded px-1 py-0.5 text-[8px] font-mono font-medium border",
            exitCode === 0
              ? "bg-success/10 text-success border-success/30"
              : "bg-destructive/10 text-destructive border-destructive/30",
          )}>
            {exitCode === 0 ? "✓" : `exit ${exitCode}`}
          </span>
        )}

        {/* Running indicator */}
        {isRunning && (
          <span className="shrink-0 rounded px-1 py-0.5 text-[8px] font-medium text-info border border-info/30 bg-info/10">
            streaming
          </span>
        )}

        <span className="flex-1" />

        {/* Copy button (visible on hover) */}
        {hasOutput && (
          <button
            type="button"
            onClick={(e) => { e.stopPropagation(); handleCopy(); }}
            className="shrink-0 opacity-0 group-hover:opacity-100 text-muted-foreground hover:text-foreground transition p-0.5"
            aria-label="Copy output"
          >
            {copied ? <Check size={9} className="text-success" /> : <Copy size={9} />}
          </button>
        )}

        {hasOutput && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
            : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        )}
      </button>

      {/* Expanded: terminal output */}
      {expanded && hasOutput && (
        <div className="border-t border-border/30 max-h-[250px] overflow-auto px-2.5 py-2 bg-card/10">
          <pre className="font-mono text-[10px] leading-[1.5] whitespace-pre-wrap break-all text-foreground/70">
            <AnsiText>{text}</AnsiText>
          </pre>
          {truncated && (
            <div className="mt-1 text-[9px] text-muted-foreground italic">
              ... output truncated ({Math.round(accumulated.length / 1024)}KB total)
            </div>
          )}
        </div>
      )}

      {/* No output yet — spinner */}
      {!hasOutput && isRunning && (
        <div className="border-t border-border/30 px-2.5 py-2 flex items-center gap-2 text-[10px] text-muted-foreground">
          <Loader2 size={10} className="animate-spin" />
          Waiting for output...
        </div>
      )}
    </div>
  );
}

/**
 * Memoized wrapper that prevents re-renders for the same session
 * when deltas arrive (they accumulate internally via useEffect).
 */
export const CommandSessionCardMemo = memo(CommandSessionCard);
