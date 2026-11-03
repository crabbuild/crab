import { useEffect, useState } from "react";
import clsx from "clsx";
import { ArrowLeft, X, Loader2, ExternalLink } from "lucide-react";
import { useKanbanAnalytics } from "../../hooks/useKanbanAnalytics";
import type { KanbanBoardAnalytics, KanbanCardAnalytics } from "../../ipc";

interface AnalyticsPanelProps {
  boardId: string;
  onClose: () => void;
}

// ── Formatting Helpers ────────────────────────────────────────────────────────

function formatCurrency(amount: number): string {
  return `$${amount.toFixed(2)}`;
}

function formatDuration(ms: number): string {
  if (ms <= 0) return "—";
  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

function formatPercentage(rate: number): string {
  return `${Math.round(rate * 100)}%`;
}

// ── Summary Cards ─────────────────────────────────────────────────────────────

function SummaryCards({ summary }: { summary: KanbanBoardAnalytics["summary"] }) {
  return (
    <div className="grid grid-cols-2 gap-2">
      <div className="rounded border border-gx-border bg-gx-surface p-2">
        <div className="text-[10px] text-gx-muted uppercase tracking-wide">Total Cost</div>
        <div className="mt-0.5 text-sm font-medium text-gx-text">
          {formatCurrency(summary.totalCost)}
        </div>
      </div>
      <div className="rounded border border-gx-border bg-gx-surface p-2">
        <div className="text-[10px] text-gx-muted uppercase tracking-wide">Avg Cycle Time</div>
        <div className="mt-0.5 text-sm font-medium text-gx-text">
          {formatDuration(summary.averageCycleTimeMs)}
        </div>
      </div>
      <div className="rounded border border-gx-border bg-gx-surface p-2">
        <div className="text-[10px] text-gx-muted uppercase tracking-wide">Success Rate</div>
        <div className="mt-0.5 text-sm font-medium text-gx-text">
          {formatPercentage(summary.successRate)}
        </div>
        <div className="text-[10px] text-gx-muted">
          {summary.totalCompleted}/{summary.totalAttempted} completed
        </div>
      </div>
      <div className="rounded border border-gx-border bg-gx-surface p-2">
        <div className="text-[10px] text-gx-muted uppercase tracking-wide">Cards Attempted</div>
        <div className="mt-0.5 text-sm font-medium text-gx-text">
          {summary.totalAttempted}
        </div>
      </div>
    </div>
  );
}

// ── Iteration Distribution ────────────────────────────────────────────────────

function IterationDistribution({ distribution }: { distribution: Record<number, number> }) {
  const entries = Object.entries(distribution)
    .map(([k, v]) => ({ iterations: Number(k), count: v }))
    .sort((a, b) => a.iterations - b.iterations);

  if (entries.length === 0) {
    return <div className="text-xs text-gx-muted">No iteration data yet.</div>;
  }

  const maxCount = Math.max(...entries.map((e) => e.count), 1);

  return (
    <div className="space-y-1">
      <div className="text-[10px] text-gx-muted uppercase tracking-wide mb-1">
        Iteration Distribution
      </div>
      {entries.map(({ iterations, count }) => (
        <div key={iterations} className="flex items-center gap-2">
          <span className="text-[10px] text-gx-muted w-8 text-right shrink-0">
            {iterations === 0 ? "0" : iterations}×
          </span>
          <div className="flex-1 h-3 bg-gx-bg rounded overflow-hidden">
            <div
              className="h-full bg-gx-accent/60 rounded transition-all"
              style={{ width: `${(count / maxCount) * 100}%` }}
            />
          </div>
          <span className="text-[10px] text-gx-muted w-6 shrink-0">{count}</span>
        </div>
      ))}
    </div>
  );
}

// ── Per-Card Analytics View ───────────────────────────────────────────────────

function CardAnalyticsView({
  analytics,
  onBack,
}: {
  analytics: KanbanCardAnalytics;
  onBack: () => void;
}) {
  return (
    <div className="space-y-3">
      <button
        type="button"
        className="flex items-center gap-1 text-xs text-gx-accent hover:underline"
        onClick={onBack}
      >
        <ArrowLeft size={12} />
        Back to summary
      </button>

      <div className="text-xs font-medium text-gx-text">
        {analytics.externalId}
      </div>

      {/* Outcome badge */}
      <div className="flex items-center gap-2">
        <span
          className={clsx(
            "chip text-[10px] px-1.5 py-0",
            analytics.outcome === "success" && "border-gx-success/50 text-gx-success",
            analytics.outcome === "blocked" && "border-gx-error/50 text-gx-error",
            analytics.outcome === "cancelled" && "border-gx-muted/50 text-gx-muted",
            analytics.outcome === "in_progress" && "border-gx-accent/50 text-gx-accent",
          )}
        >
          {analytics.outcome}
        </span>
        {analytics.cycleTimeMs != null && (
          <span className="text-[10px] text-gx-muted">
            Cycle: {formatDuration(analytics.cycleTimeMs)}
          </span>
        )}
        <span className="text-[10px] text-gx-muted">
          Iterations: {analytics.iterationCount}
        </span>
      </div>

      {/* PR link */}
      {analytics.prUrl && (
        <div className="flex items-center gap-1">
          <ExternalLink size={10} className="text-gx-muted" />
          <a
            href={analytics.prUrl}
            target="_blank"
            rel="noopener noreferrer"
            className="text-xs text-gx-accent hover:underline"
          >
            PR #{analytics.prNumber}
          </a>
        </div>
      )}

      {/* Token usage per turn */}
      {analytics.tokenUsage.length > 0 && (
        <div>
          <div className="text-[10px] text-gx-muted uppercase tracking-wide mb-1">
            Token Usage per Turn
          </div>
          <div className="space-y-0.5 max-h-40 overflow-y-auto">
            {analytics.tokenUsage.map((turn) => (
              <div
                key={turn.turnNumber}
                className="flex items-center gap-2 text-[10px] text-gx-text"
              >
                <span className="text-gx-muted w-6 shrink-0">#{turn.turnNumber}</span>
                <span className="flex-1">
                  {turn.inputTokens != null ? `${turn.inputTokens.toLocaleString()} in` : "—"}
                  {" / "}
                  {turn.outputTokens != null ? `${turn.outputTokens.toLocaleString()} out` : "—"}
                </span>
                <span className="text-gx-muted shrink-0">
                  {turn.costEstimate != null ? formatCurrency(turn.costEstimate) : "—"}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* State transition timeline */}
      {analytics.stateTransitions.length > 0 && (
        <div>
          <div className="text-[10px] text-gx-muted uppercase tracking-wide mb-1">
            State Transitions
          </div>
          <div className="space-y-0.5 max-h-40 overflow-y-auto">
            {analytics.stateTransitions.map((t, i) => (
              <div
                key={i}
                className="flex items-center gap-2 text-[10px] text-gx-text"
              >
                <span className="text-gx-muted shrink-0 w-16">
                  {new Date(t.timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
                </span>
                <span className="text-gx-muted">{t.from}</span>
                <span className="text-gx-muted">→</span>
                <span>{t.to}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

// ── Card List (clickable to drill into per-card analytics) ────────────────────

function CardList({
  cards,
  onSelectCard,
}: {
  cards: KanbanCardAnalytics[];
  onSelectCard: (cardId: string) => void;
}) {
  if (cards.length === 0) {
    return <div className="text-xs text-gx-muted">No card analytics available.</div>;
  }

  return (
    <div>
      <div className="text-[10px] text-gx-muted uppercase tracking-wide mb-1">
        Cards
      </div>
      <div className="space-y-0.5 max-h-48 overflow-y-auto">
        {cards.map((card) => (
          <button
            key={card.cardId}
            type="button"
            className="flex w-full items-center gap-2 rounded px-1.5 py-1 text-xs text-gx-text hover:bg-gx-surface-hover transition"
            onClick={() => onSelectCard(card.cardId)}
          >
            <span
              className={clsx(
                "h-1.5 w-1.5 rounded-full shrink-0",
                card.outcome === "success" && "bg-gx-success",
                card.outcome === "blocked" && "bg-gx-error",
                card.outcome === "cancelled" && "bg-gx-muted",
                card.outcome === "in_progress" && "bg-gx-accent",
              )}
            />
            <span className="truncate flex-1 text-left">{card.externalId}</span>
            <span className="text-[10px] text-gx-muted shrink-0">
              {card.tokenUsage.reduce((sum, t) => sum + (t.costEstimate ?? 0), 0) > 0
                ? formatCurrency(card.tokenUsage.reduce((sum, t) => sum + (t.costEstimate ?? 0), 0))
                : "—"}
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}

// ── Main Panel ────────────────────────────────────────────────────────────────

/**
 * Analytics slide-over panel. Fetches board analytics on open and
 * displays summary cards, iteration distribution, and per-card drill-down.
 */
export function AnalyticsPanel({ boardId, onClose }: AnalyticsPanelProps) {
  const {
    boardAnalytics,
    cardAnalytics,
    loading,
    error,
    fetchBoardAnalytics,
    fetchCardAnalytics,
    clearCardAnalytics,
  } = useKanbanAnalytics();

  const [selectedCardId, setSelectedCardId] = useState<string | null>(null);

  // Fetch board analytics on mount (on-demand)
  useEffect(() => {
    fetchBoardAnalytics(boardId);
  }, [boardId, fetchBoardAnalytics]);

  const handleSelectCard = (cardId: string) => {
    setSelectedCardId(cardId);
    fetchCardAnalytics(cardId);
  };

  const handleBackToSummary = () => {
    setSelectedCardId(null);
    clearCardAnalytics();
  };

  return (
    <div className="flex h-full w-72 flex-col border-l border-gx-border bg-gx-bg">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-gx-border px-3 py-2">
        <span className="text-xs font-medium text-gx-text">Analytics</span>
        <button
          type="button"
          className="btn-icon"
          onClick={onClose}
          title="Close analytics"
        >
          <X size={14} />
        </button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-3 space-y-3">
        {loading && (
          <div className="flex items-center justify-center py-8">
            <Loader2 size={16} className="animate-spin text-gx-muted" />
          </div>
        )}

        {error && (
          <div className="text-xs text-gx-error">{error}</div>
        )}

        {!loading && !error && boardAnalytics && !selectedCardId && (
          <>
            <SummaryCards summary={boardAnalytics.summary} />
            <IterationDistribution distribution={boardAnalytics.summary.iterationDistribution} />
            <CardList
              cards={boardAnalytics.cards}
              onSelectCard={handleSelectCard}
            />
          </>
        )}

        {!loading && !error && selectedCardId && cardAnalytics && (
          <CardAnalyticsView
            analytics={cardAnalytics}
            onBack={handleBackToSummary}
          />
        )}

        {!loading && !error && !boardAnalytics && (
          <div className="text-xs text-gx-muted py-4 text-center">
            No analytics data available yet.
          </div>
        )}
      </div>
    </div>
  );
}
