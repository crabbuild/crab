/**
 * Cost Detail Panel — workspace detail view showing per-turn cost
 * breakdown and a cumulative cost visualization.
 */

import { useCallback, useEffect, useState } from "react";
import { Coins, Loader2, TrendingUp } from "lucide-react";

import type { CostTracker, WorkspaceCost } from "../../../electron/orchestrator-cost";

interface Props {
  workspaceId: string;
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatTokenCount(count: number): string {
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(1)}M`;
  if (count >= 1_000) return `${(count / 1_000).toFixed(1)}K`;
  return String(count);
}

function formatCost(usd: number | undefined): string {
  if (usd === undefined) return "Unavailable";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  return `$${usd.toFixed(2)}`;
}

function formatTimestamp(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

// ── Component ───────────────────────────────────────────────────────

export function CostDetailPanel({ workspaceId }: Props) {
  const [costData, setCostData] = useState<WorkspaceCost | null>(null);
  const [turnCosts, setTurnCosts] = useState<ReturnType<CostTracker["getPerTurnCosts"]>>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchCost = useCallback(async () => {
    try {
      setLoading(true);
      setError(null);
      const result = await (window as any).crab.orchestrator?.getCost(workspaceId);
      if (result?.success) {
        setCostData(result.cost ?? null);
        setTurnCosts(result.turns ?? []);
      } else if (result?.error) {
        setError(result.error);
      }
    } catch {
      setError("Failed to fetch cost data");
    } finally {
      setLoading(false);
    }
  }, [workspaceId]);

  useEffect(() => {
    fetchCost();
  }, [fetchCost]);

  // Loading state
  if (loading) {
    return (
      <div
        className="flex items-center justify-center gap-2 p-6 text-xs text-gx-muted"
        aria-label="Loading cost data"
        role="status"
      >
        <Loader2 size={14} className="animate-spin" />
        <span>Loading cost data…</span>
      </div>
    );
  }

  // Error state
  if (error) {
    return (
      <div className="p-4 text-xs text-gx-error" role="alert">
        {error}
      </div>
    );
  }

  // No data state
  if (!costData || costData.usage.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center gap-2 p-6 text-xs text-gx-muted">
        <Coins size={20} className="opacity-50" />
        <span>No cost data</span>
      </div>
    );
  }

  // Compute totals
  const totalInput = costData.usage.reduce((s, u) => s + u.inputTokens + u.cacheReadTokens + u.cacheWriteTokens, 0);
  const totalOutput = costData.usage.reduce((s, u) => s + u.outputTokens, 0);

  // Compute cumulative costs for the chart
  const cumulativeCosts: number[] = [];
  let runningTotal = 0;
  for (const usage of turnCosts) {
    runningTotal += usage.cost ?? 0;
    cumulativeCosts.push(runningTotal);
  }
  const maxCost = cumulativeCosts[cumulativeCosts.length - 1] || 1;

  return (
    <div className="flex h-full flex-col overflow-auto" aria-label="Cost detail panel">
      {/* Summary */}
      <div className="border-b border-gx-border bg-gx-surface p-4">
        <div className="flex items-center gap-2 text-xs font-medium text-gx-text">
          <Coins size={13} />
          <span>Cost Summary</span>
        </div>
        <div className="mt-3 grid grid-cols-3 gap-3">
          <SummaryCard label="Estimated Cost" value={formatCost(costData.estimatedCostUsd)} />
          <SummaryCard label="Tokens In" value={formatTokenCount(totalInput)} />
          <SummaryCard label="Tokens Out" value={formatTokenCount(totalOutput)} />
        </div>
        {costData.budgetLimitUsd != null && costData.estimatedCostUsd !== undefined && (
          <div className="mt-2 text-[10px] text-gx-muted">
            Budget: {formatCost(costData.budgetLimitUsd)} · Used{" "}
            {Math.round((costData.estimatedCostUsd / costData.budgetLimitUsd) * 100)}%
          </div>
        )}
      </div>

      {/* Cumulative Chart */}
      {costData.estimatedCostUsd !== undefined && <div className="border-b border-gx-border p-4">
        <div className="mb-2 flex items-center gap-2 text-xs font-medium text-gx-text">
          <TrendingUp size={13} />
          <span>Cumulative Cost</span>
        </div>
        <CumulativeChart costs={cumulativeCosts} maxCost={maxCost} />
      </div>}

      {/* Per-Turn Breakdown Table */}
      <div className="flex-1 p-4">
        <h3 className="mb-2 text-xs font-medium text-gx-text">Per-Turn Breakdown</h3>
        <div className="overflow-x-auto rounded border border-gx-border">
          <table className="w-full text-[11px]" aria-label="Cost breakdown by turn">
            <thead>
              <tr className="border-b border-gx-border bg-gx-surface text-left text-gx-muted">
                <th className="px-2 py-1.5 font-medium">Turn</th>
                <th className="px-2 py-1.5 font-medium">Time</th>
                <th className="px-2 py-1.5 font-medium text-right">In</th>
                <th className="px-2 py-1.5 font-medium text-right">Out</th>
                <th className="px-2 py-1.5 font-medium text-right">Cost</th>
              </tr>
            </thead>
            <tbody>
              {turnCosts.map((usage, idx) => (
                <tr
                  key={usage.turnId}
                  className="border-b border-gx-border/50 last:border-0 hover:bg-gx-surface/50 transition-colors"
                >
                  <td className="px-2 py-1.5 font-mono text-gx-muted">{idx + 1}</td>
                  <td className="px-2 py-1.5 text-gx-text">{formatTimestamp(usage.timestamp)}</td>
                  <td className="px-2 py-1.5 text-right text-gx-text">
                    {formatTokenCount(usage.inputTokens + usage.cacheReadTokens + usage.cacheWriteTokens)}
                  </td>
                  <td className="px-2 py-1.5 text-right text-gx-text">
                    {formatTokenCount(usage.outputTokens)}
                  </td>
                  <td className="px-2 py-1.5 text-right font-mono text-gx-accent">
                    {formatCost(usage.cost)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}

// ── Sub-components ──────────────────────────────────────────────────

function SummaryCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded border border-gx-border bg-gx-bg px-2.5 py-2 text-center">
      <div className="text-[10px] text-gx-muted">{label}</div>
      <div className="mt-0.5 text-sm font-medium text-gx-text">{value}</div>
    </div>
  );
}

/**
 * Lightweight cumulative cost chart using Tailwind widths.
 * Each bar represents a turn's cumulative cost as a percentage of the max.
 */
function CumulativeChart({ costs, maxCost }: { costs: number[]; maxCost: number }) {
  // Show at most 20 bars; if more turns, sample evenly
  const maxBars = 20;
  const sampled =
    costs.length <= maxBars
      ? costs
      : Array.from({ length: maxBars }, (_, i) =>
          costs[Math.round((i / (maxBars - 1)) * (costs.length - 1))],
        );

  return (
    <div
      className="flex items-end gap-px"
      style={{ height: "48px" }}
      role="img"
      aria-label={`Cumulative cost chart showing ${costs.length} turns, total ${formatCost(maxCost)}`}
    >
      {sampled.map((cost, idx) => {
        const heightPct = Math.max((cost / maxCost) * 100, 2);
        return (
          <div
            key={idx}
            className="flex-1 rounded-t bg-gx-accent/70 transition-all"
            style={{ height: `${heightPct}%` }}
            title={`Turn ${idx + 1}: ${formatCost(cost)}`}
          />
        );
      })}
    </div>
  );
}
