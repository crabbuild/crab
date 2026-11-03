/**
 * Compact cost chip rendered beside message timestamps.
 *
 * Shows the dollar cost of a single agent turn. Turns that are
 * noticeably more expensive than the rolling average (default: >50%
 * over baseline *and* >10¢ absolute) render in the warn accent so the
 * user spots the jump without opening the usage page.
 */

import clsx from "clsx";
import { DollarSign, AlertTriangle } from "lucide-react";

interface Props {
  /** Cost of this turn in USD. */
  cost: number;
  /** Average cost of prior turns, used to detect jumps. */
  average: number | null;
  /** Optional token totals for the tooltip. When provided we surface the
   *  cache hit ratio so the user can see when a turn is hot vs cold. */
  inputTokens?: number;
  cacheReadTokens?: number;
}

const MIN_DISPLAY = 0.0005; // Below half a tenth of a cent, don't bother showing.
const NUDGE_ABSOLUTE = 0.10; // At least 10¢ higher than average
const NUDGE_MULTIPLE = 1.5; // AND 1.5x the average

export function TurnCostChip({ cost, average, inputTokens, cacheReadTokens }: Props) {
  if (!Number.isFinite(cost) || cost < MIN_DISPLAY) return null;

  const isNudge =
    average != null &&
    average > 0 &&
    cost - average > NUDGE_ABSOLUTE &&
    cost / average > NUDGE_MULTIPLE;

  // Build the tooltip lazily so callers without token data still get the
  // simple "$0.04 (avg $0.03)" form they're used to.
  const tooltip = (() => {
    const lines: string[] = [];
    lines.push(
      average != null
        ? `This turn cost ${formatUsd(cost)} (avg ${formatUsd(average)})`
        : `This turn cost ${formatUsd(cost)}`,
    );
    if (
      typeof inputTokens === "number"
      && typeof cacheReadTokens === "number"
      && inputTokens > 0
    ) {
      const ratio = Math.min(100, Math.round((cacheReadTokens / inputTokens) * 100));
      lines.push(`Prompt cache: ${ratio}% hit (${cacheReadTokens.toLocaleString()} / ${inputTokens.toLocaleString()} tokens)`);
    }
    return lines.join("\n");
  })();

  return (
    <span
      title={tooltip}
      className={clsx(
        "inline-flex items-center gap-0.5 rounded-full px-1 py-0.5 text-[9px] font-medium",
        isNudge
          ? "bg-gx-warn/15 text-gx-warn"
          : "bg-gx-surface text-gx-muted",
      )}
    >
      {isNudge ? <AlertTriangle size={8} /> : <DollarSign size={8} />}
      {formatUsd(cost)}
    </span>
  );
}

export function formatUsd(value: number): string {
  if (!Number.isFinite(value)) return "$—";
  if (value < 0.01) return `$${value.toFixed(4)}`;
  if (value < 1) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(2)}`;
}
