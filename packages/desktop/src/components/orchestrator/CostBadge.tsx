import { useState } from "react";
import { Coins } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  costUsd: number;
  inputTokens?: number;
  outputTokens?: number;
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatTokenCount(count: number): string {
  if (count >= 1000) {
    return `${Math.round(count / 1000)}K`;
  }
  return String(count);
}

function formatCost(usd: number): string {
  return `$${usd.toFixed(2)}`;
}

// ── Component ───────────────────────────────────────────────────────

export function CostBadge({ costUsd, inputTokens, outputTokens }: Props) {
  const [showTooltip, setShowTooltip] = useState(false);

  if (!costUsd) return null;

  const hasTokenBreakdown =
    inputTokens !== undefined || outputTokens !== undefined;

  const tooltipText = hasTokenBreakdown
    ? `${formatTokenCount(inputTokens ?? 0)} in / ${formatTokenCount(outputTokens ?? 0)} out`
    : formatCost(costUsd);

  return (
    <span
      className="relative inline-flex items-center gap-1 rounded bg-gx-surface px-1.5 py-0.5 text-[10px] text-gx-muted"
      onMouseEnter={() => setShowTooltip(true)}
      onMouseLeave={() => setShowTooltip(false)}
      aria-label={`Cost: ${formatCost(costUsd)}${hasTokenBreakdown ? `, ${tooltipText}` : ""}`}
    >
      <Coins size={10} className="shrink-0" />
      <span>{formatCost(costUsd)}</span>

      {/* Tooltip */}
      {showTooltip && hasTokenBreakdown && (
        <span
          role="tooltip"
          className="pointer-events-none absolute bottom-full left-1/2 z-50 mb-1 -translate-x-1/2 whitespace-nowrap rounded border border-gx-border bg-gx-surface px-2 py-1 text-[10px] text-gx-text shadow-lg"
        >
          {tooltipText}
        </span>
      )}
    </span>
  );
}
