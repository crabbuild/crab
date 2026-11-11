/**
 * Cost & Token Tracker for the Crab Desktop Orchestrator.
 *
 * Accumulates TokenUsage records per workspace, estimates costs
 * using provider-reported estimates, enforces budget limits, and
 * persists cost data to disk. Emits `budget-warning` when a
 * workspace exceeds its configured budget.
 */

import { EventEmitter } from "node:events";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  timestamp: number;
  turnId: string;
  model?: string;
  estimatedCostUsd?: number;
  /** Present only on provider cumulative snapshots, never on priced deltas. */
  sessionId?: string;
}

export interface WorkspaceCost {
  workspaceId: string;
  agentType: string;
  model: string;
  usage: TokenUsage[];
  estimatedCostUsd?: number;
  budgetLimitUsd?: number;
  cumulativeUsage?: Record<string, TokenUsage>;
}

interface PersistedCostData {
  workspaces: WorkspaceCost[];
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_PERSIST_PATH = path.join(
  os.homedir(),
  ".crab",
  "desktop",
  "agent-costs.json",
);

// ─── CostTracker ─────────────────────────────────────────────────────────────

export class CostTracker extends EventEmitter {
  private costs = new Map<string, WorkspaceCost>();
  private readonly persistPath: string;

  constructor(persistPath?: string) {
    super();
    this.persistPath = persistPath ?? DEFAULT_PERSIST_PATH;
  }

  /**
   * Records a token usage event for a workspace. Creates the workspace
   * entry if it doesn't exist, appends the usage record, and recalculates
   * the estimated cost. Emits `budget-warning` if the budget is exceeded.
   */
  recordUsage(
    workspaceId: string,
    usage: TokenUsage,
    agentType: string,
    model: string,
  ): void {
    let entry = this.costs.get(workspaceId);

    if (!entry) {
      entry = {
        workspaceId,
        agentType,
        model,
        usage: [],
        estimatedCostUsd: 0,
      };
      this.costs.set(workspaceId, entry);
    }

    // Update agent type and model in case they changed mid-session.
    entry.agentType = agentType;
    entry.model = model;

    let record = { ...usage, model };
    if (usage.sessionId) {
      const key = JSON.stringify([usage.sessionId, model]);
      entry.cumulativeUsage ??= {};
      const previous = entry.cumulativeUsage[key];
      // A crashed provider may report zeroed final totals. Keep the prior
      // high-water mark; resets receive a new session ID from the provider.
      if (previous && (usage.estimatedCostUsd! < previous.estimatedCostUsd!
        || usage.inputTokens < previous.inputTokens || usage.outputTokens < previous.outputTokens
        || usage.cacheReadTokens < previous.cacheReadTokens || usage.cacheWriteTokens < previous.cacheWriteTokens)) return;
      entry.cumulativeUsage[key] = record;
      record = {
        ...record, sessionId: undefined,
        inputTokens: usage.inputTokens - (previous?.inputTokens ?? 0),
        outputTokens: usage.outputTokens - (previous?.outputTokens ?? 0),
        cacheReadTokens: usage.cacheReadTokens - (previous?.cacheReadTokens ?? 0),
        cacheWriteTokens: usage.cacheWriteTokens - (previous?.cacheWriteTokens ?? 0),
        estimatedCostUsd: usage.estimatedCostUsd! - (previous?.estimatedCostUsd ?? 0),
      };
      if (!record.inputTokens && !record.outputTokens && !record.cacheReadTokens && !record.cacheWriteTokens && !record.estimatedCostUsd) return;
    }
    entry.usage.push(record);
    entry.estimatedCostUsd = entry.usage.every(record => record.estimatedCostUsd !== undefined)
      ? entry.usage.reduce((sum, record) => sum + record.estimatedCostUsd!, 0)
      : undefined;

    // Check budget and emit warning if exceeded.
    if (entry.budgetLimitUsd != null && entry.estimatedCostUsd !== undefined && entry.estimatedCostUsd > entry.budgetLimitUsd) {
      this.emit("budget-warning", {
        workspaceId,
        cost: entry.estimatedCostUsd,
        limit: entry.budgetLimitUsd,
      });
    }
  }

  /** Returns cost data for a workspace, or undefined if not tracked. */
  getCost(workspaceId: string): WorkspaceCost | undefined {
    return this.costs.get(workspaceId);
  }

  /** Returns all workspace costs as an array. */
  getAllCosts(): WorkspaceCost[] {
    return Array.from(this.costs.values());
  }

  /**
   * Returns per-turn cost breakdown for a workspace. Each usage record
   * is priced independently so the UI can surface "$0.24 this turn"
   * chips and flag unusually expensive turns.
   *
   * Results are sorted oldest-first so the caller can compute rolling
   * averages or diff against the previous turn without re-sorting.
   */
  getPerTurnCosts(workspaceId: string): Array<{
    turnId: string;
    timestamp: number;
    cost?: number;
    inputTokens: number;
    outputTokens: number;
    cacheReadTokens: number;
    cacheWriteTokens: number;
  }> {
    const entry = this.costs.get(workspaceId);
    if (!entry) return [];

    const turns = new Map<string, { turnId: string; timestamp: number; cost?: number; inputTokens: number; outputTokens: number; cacheReadTokens: number; cacheWriteTokens: number }>();
    for (const usage of entry.usage) {
      const turn = turns.get(usage.turnId);
      if (!turn) { turns.set(usage.turnId, { ...usage, cost: usage.estimatedCostUsd }); continue; }
      turn.cost = turn.cost !== undefined && usage.estimatedCostUsd !== undefined ? turn.cost + usage.estimatedCostUsd : undefined;
      turn.inputTokens += usage.inputTokens;
      turn.outputTokens += usage.outputTokens;
      turn.cacheReadTokens += usage.cacheReadTokens;
      turn.cacheWriteTokens += usage.cacheWriteTokens;
      turn.timestamp = Math.max(turn.timestamp, usage.timestamp);
    }
    return [...turns.values()].sort((a, b) => a.timestamp - b.timestamp);
  }

  /** Sets the budget limit (in USD) for a workspace. */
  setBudget(workspaceId: string, limitUsd: number): void {
    let entry = this.costs.get(workspaceId);

    if (!entry) {
      entry = {
        workspaceId,
        agentType: "",
        model: "",
        usage: [],
        estimatedCostUsd: 0,
        budgetLimitUsd: limitUsd,
      };
      this.costs.set(workspaceId, entry);
    } else {
      entry.budgetLimitUsd = limitUsd;
    }
  }

  /**
   * Checks whether a workspace has exceeded its budget.
   * Returns the current cost, limit, and whether the limit is exceeded.
   */
  checkBudget(workspaceId: string): { exceeded: boolean; cost?: number; limit: number } {
    const entry = this.costs.get(workspaceId);

    if (!entry || entry.budgetLimitUsd == null) {
      return { exceeded: false, cost: entry ? entry.estimatedCostUsd : 0, limit: 0 };
    }

    return {
      exceeded: entry.estimatedCostUsd !== undefined && entry.estimatedCostUsd > entry.budgetLimitUsd,
      cost: entry.estimatedCostUsd,
      limit: entry.budgetLimitUsd,
    };
  }

  /**
   * Export recorded tokens and provider estimates as CSV.
   * Columns: workspace_id, agent_type, model, total_input_tokens,
   * total_output_tokens, total_cache_read_tokens, total_cache_write_tokens,
   * estimated_cost_usd, budget_limit_usd, first_usage, last_usage.
   */
  exportCsv(): string {
    const header = [
      "workspace_id",
      "agent_type",
      "model",
      "total_input_tokens",
      "total_output_tokens",
      "total_cache_read_tokens",
      "total_cache_write_tokens",
      "estimated_cost_usd",
      "budget_limit_usd",
      "first_usage",
      "last_usage",
    ].join(",");

    const rows: string[] = [header];

    for (const entry of this.costs.values()) {
      const totals = entry.usage.reduce(
        (acc, u) => ({
          input: acc.input + u.inputTokens,
          output: acc.output + u.outputTokens,
          cacheRead: acc.cacheRead + u.cacheReadTokens,
          cacheWrite: acc.cacheWrite + u.cacheWriteTokens,
        }),
        { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      );

      const firstUsage = entry.usage.length > 0
        ? new Date(entry.usage[0].timestamp).toISOString()
        : "";
      const lastUsage = entry.usage.length > 0
        ? new Date(entry.usage[entry.usage.length - 1].timestamp).toISOString()
        : "";

      rows.push([
        entry.workspaceId,
        entry.agentType,
        entry.model,
        totals.input,
        totals.output,
        totals.cacheRead,
        totals.cacheWrite,
        entry.estimatedCostUsd?.toFixed(6) ?? "",
        entry.budgetLimitUsd?.toFixed(2) ?? "",
        firstUsage,
        lastUsage,
      ].join(","));
    }

    return rows.join("\n");
  }

  /** Persists all cost data to `~/.crab/desktop/agent-costs.json`. */
  persist(): void {
    const data: PersistedCostData = {
      workspaces: Array.from(this.costs.values()),
    };

    const dir = path.dirname(this.persistPath);

    try {
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(this.persistPath, JSON.stringify(data, null, 2), "utf-8");
    } catch {
      // Persistence is best-effort — don't crash the app.
    }
  }

  /** Loads cost data from disk. Call on app startup. */
  load(): void {
    try {
      if (!fs.existsSync(this.persistPath)) return;

      const raw = fs.readFileSync(this.persistPath, "utf-8");
      const data: PersistedCostData = JSON.parse(raw);

      if (!data.workspaces || !Array.isArray(data.workspaces)) return;

      for (const entry of data.workspaces) {
        // Totals come from per-record provider estimates. An older local
        // price-table total cannot price records without provider evidence.
        entry.estimatedCostUsd = entry.usage.every(record => record.estimatedCostUsd !== undefined)
          ? entry.usage.reduce((sum, record) => sum + record.estimatedCostUsd!, 0) : undefined;
        this.costs.set(entry.workspaceId, entry);
      }
    } catch {
      // If the file is corrupted or unreadable, start fresh.
    }
  }

}
