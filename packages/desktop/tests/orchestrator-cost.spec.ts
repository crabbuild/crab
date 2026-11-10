import { afterEach, describe, expect, it } from "vitest";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { CostTracker, type TokenUsage } from "../electron/orchestrator-cost";

const directories: string[] = [];
afterEach(() => { for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true }); });
function snapshot(turnId: string, inputTokens: number, estimatedCostUsd: number, sessionId = "session-1"): TokenUsage {
  return { turnId, inputTokens, outputTokens: inputTokens, cacheReadTokens: 0, cacheWriteTokens: 0, estimatedCostUsd, sessionId, timestamp: Number(turnId) };
}

describe("provider usage accounting", () => {
  it("counts cumulative multi-model totals once and groups each turn's model deltas", () => {
    const tracker = new CostTracker();
    tracker.recordUsage("task", snapshot("1", 10, 0.125), "claude-code", "main-model");
    tracker.recordUsage("task", snapshot("1", 5, 0.125), "claude-code", "subagent-model");
    tracker.recordUsage("task", snapshot("2", 20, 0.25), "claude-code", "main-model");
    tracker.recordUsage("task", snapshot("2", 5, 0.125), "claude-code", "subagent-model");
    tracker.recordUsage("task", snapshot("2", 20, 0.25), "claude-code", "main-model");
    expect({ total: tracker.getCost("task")?.estimatedCostUsd, turns: tracker.getPerTurnCosts("task").map(turn => [turn.turnId, turn.inputTokens, turn.cost]) }).toEqual({
      total: 0.375, turns: [["1", 15, 0.25], ["2", 10, 0.125]],
    });
  });

  it("restores the provider baseline so resumed totals do not charge earlier turns again", () => {
    const directory = mkdtempSync(path.join(tmpdir(), "crab-cost-"));
    directories.push(directory);
    const file = path.join(directory, "costs.json");
    const first = new CostTracker(file);
    first.recordUsage("task", snapshot("1", 10, 0.125), "claude-code", "model");
    first.persist();
    const resumed = new CostTracker(file);
    resumed.load();
    resumed.recordUsage("task", snapshot("2", 20, 0.25), "claude-code", "model");
    expect(resumed.getPerTurnCosts("task").map(turn => [turn.inputTokens, turn.cost])).toEqual([[10, 0.125], [10, 0.125]]);
  });

  it("keeps prior spend after a provider reset and ignores a crashed session's zeroed totals", () => {
    const tracker = new CostTracker();
    tracker.recordUsage("task", snapshot("1", 10, 0.125), "claude-code", "model");
    tracker.recordUsage("task", snapshot("2", 0, 0), "claude-code", "model");
    tracker.recordUsage("task", snapshot("3", 5, 0.125, "session-after-clear"), "claude-code", "model");
    expect({ total: tracker.getCost("task")?.estimatedCostUsd, tokens: tracker.getPerTurnCosts("task").map(turn => turn.inputTokens) }).toEqual({ total: 0.25, tokens: [10, 5] });
  });

  it("keeps token counts without inventing a dollar estimate or a budget warning", () => {
    const tracker = new CostTracker();
    tracker.setBudget("task", 0.01);
    tracker.recordUsage("task", { turnId: "1", timestamp: 1, inputTokens: 100, outputTokens: 20, cacheReadTokens: 40, cacheWriteTokens: 0 }, "codex", "provider-model");
    expect({ turn: tracker.getPerTurnCosts("task").map(turn => [turn.inputTokens, turn.cost]), exceeded: tracker.checkBudget("task").exceeded, total: tracker.getCost("task")?.estimatedCostUsd }).toEqual({ turn: [[100, undefined]], exceeded: false, total: undefined });
  });
});
