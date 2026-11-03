/**
 * Cost & Token Tracker for the Crab Desktop Orchestrator.
 *
 * Accumulates TokenUsage records per workspace, estimates costs
 * using a provider pricing table, enforces budget limits, and
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
}

export interface WorkspaceCost {
  workspaceId: string;
  agentType: string;
  model: string;
  usage: TokenUsage[];
  estimatedCostUsd: number;
  budgetLimitUsd?: number;
}

interface PersistedCostData {
  workspaces: WorkspaceCost[];
}

// ─── Pricing Table ───────────────────────────────────────────────────────────

/**
 * Per-token pricing in USD. Updated periodically as providers change rates.
 * Cache read tokens are priced at 10% of input; cache write tokens at 125%
 * of input (following Anthropic's published pricing).
 *
 * Latest Claude rate card (April 2026): Opus 4.7 keeps the $5/$25 split
 * from 4.6; Sonnet 4.6 is $3/$15; Haiku 4.5 is $1/$5. GPT-5-family rates
 * match the published Codex pricing.
 */
const PRICING: Record<string, { input: number; output: number }> = {
  "claude-opus-4-7": { input: 5.0 / 1_000_000, output: 25.0 / 1_000_000 },
  "claude-sonnet-4-6": { input: 3.0 / 1_000_000, output: 15.0 / 1_000_000 },
  "claude-haiku-4-5": { input: 1.0 / 1_000_000, output: 5.0 / 1_000_000 },
  "gpt-5.3-codex": { input: 1.25 / 1_000_000, output: 10.0 / 1_000_000 },
  "gpt-5-codex": { input: 1.25 / 1_000_000, output: 10.0 / 1_000_000 },
  "gpt-5.5": { input: 2.5 / 1_000_000, output: 10.0 / 1_000_000 },
};

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

    entry.usage.push(usage);
    entry.estimatedCostUsd = this.calculateCost(entry);

    // Check budget and emit warning if exceeded.
    if (entry.budgetLimitUsd != null && entry.estimatedCostUsd > entry.budgetLimitUsd) {
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
    cost: number;
    inputTokens: number;
    outputTokens: number;
    cacheReadTokens: number;
    cacheWriteTokens: number;
  }> {
    const entry = this.costs.get(workspaceId);
    if (!entry) return [];

    const pricing = PRICING[entry.model];
    const inputRate = pricing?.input ?? PRICING["claude-opus-4-7"].input;
    const outputRate = pricing?.output ?? PRICING["claude-opus-4-7"].output;
    const cacheReadRate = inputRate * 0.1;
    const cacheWriteRate = inputRate * 1.25;

    return [...entry.usage]
      .sort((a, b) => a.timestamp - b.timestamp)
      .map((u) => ({
        turnId: u.turnId,
        timestamp: u.timestamp,
        cost:
          u.inputTokens * inputRate +
          u.outputTokens * outputRate +
          u.cacheReadTokens * cacheReadRate +
          u.cacheWriteTokens * cacheWriteRate,
        inputTokens: u.inputTokens,
        outputTokens: u.outputTokens,
        cacheReadTokens: u.cacheReadTokens,
        cacheWriteTokens: u.cacheWriteTokens,
      }));
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
  checkBudget(workspaceId: string): { exceeded: boolean; cost: number; limit: number } {
    const entry = this.costs.get(workspaceId);

    if (!entry || entry.budgetLimitUsd == null) {
      return { exceeded: false, cost: entry?.estimatedCostUsd ?? 0, limit: 0 };
    }

    return {
      exceeded: entry.estimatedCostUsd > entry.budgetLimitUsd,
      cost: entry.estimatedCostUsd,
      limit: entry.budgetLimitUsd,
    };
  }

  /**
   * Exports all cost data as a CSV string suitable for team billing.
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
        entry.estimatedCostUsd.toFixed(6),
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
        this.costs.set(entry.workspaceId, entry);
      }
    } catch {
      // If the file is corrupted or unreadable, start fresh.
    }
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Calculates the total estimated cost for a workspace based on its
   * usage records and the pricing table. Falls back to the most expensive
   * model pricing if the model is unknown.
   */
  private calculateCost(entry: WorkspaceCost): number {
    const pricing = PRICING[entry.model];

    // Fallback: use claude-opus pricing (most expensive) for unknown models.
    const inputRate = pricing?.input ?? PRICING["claude-opus-4-7"].input;
    const outputRate = pricing?.output ?? PRICING["claude-opus-4-7"].output;

    // Cache read tokens are discounted (10% of input rate).
    const cacheReadRate = inputRate * 0.1;
    // Cache write tokens are slightly more expensive (125% of input rate).
    const cacheWriteRate = inputRate * 1.25;

    let totalCost = 0;

    for (const usage of entry.usage) {
      totalCost +=
        usage.inputTokens * inputRate +
        usage.outputTokens * outputRate +
        usage.cacheReadTokens * cacheReadRate +
        usage.cacheWriteTokens * cacheWriteRate;
    }

    return totalCost;
  }
}
