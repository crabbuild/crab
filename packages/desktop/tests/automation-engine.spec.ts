import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

import {
  AutomationEngine,
  parseCronExpression,
  cronMatchesNow,
  parseCronField,
} from "../electron/kanban/automation-engine";
import type {
  TriggerContext,
  OrchestratorLike,
  PipelineControllerLike,
} from "../electron/kanban/automation-engine";
import type {
  AutomationRule,
  KanbanCard,
} from "../electron/kanban/kanban-types";

// Mock electron-log
vi.mock("electron-log", () => ({
  default: { warn: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

// ── Helpers ──────────────────────────────────────────────────────

function makeCard(overrides: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id: "card-1",
    source: "github",
    externalId: "42",
    externalUrl: "https://github.com/o/r/issues/42",
    title: "Fix the bug",
    description: "There is a bug that needs fixing",
    columnId: "in-progress",
    positionIndex: 0,
    priority: "high",
    labels: ["bug", "frontend"],
    pipelineState: "idle",
    iterationCount: 0,
    createdAt: 1000,
    updatedAt: 2000,
    ...overrides,
  };
}

function makeRule(overrides: Partial<AutomationRule> = {}): AutomationRule {
  return {
    id: "rule-1",
    name: "Test Rule",
    enabled: true,
    trigger: { type: "card_moved" },
    conditions: [],
    actions: [{ type: "spawn_agent", params: { repoPath: "/repo" } }],
    continueOnError: false,
    ...overrides,
  };
}

function makeContext(overrides: Partial<TriggerContext> = {}): TriggerContext {
  return {
    card: makeCard(),
    currentUser: "testuser",
    ...overrides,
  };
}

function createMockOrchestrator(): OrchestratorLike {
  return {
    createTask: vi.fn().mockResolvedValue({ id: "task-1" }),
    iterate: vi.fn().mockResolvedValue(undefined),
  };
}

function createMockPipelineController(): PipelineControllerLike {
  return {
    createPr: vi.fn().mockResolvedValue(undefined),
    runTests: vi.fn().mockResolvedValue(undefined),
    requestReview: vi.fn().mockResolvedValue(undefined),
    transitionIssue: vi.fn().mockResolvedValue(undefined),
    addComment: vi.fn().mockResolvedValue(undefined),
    mergePr: vi.fn().mockResolvedValue(undefined),
  };
}

// Mock RuleStore that operates in-memory
function createMockRuleStore() {
  let rules: AutomationRule[] = [];
  return {
    getRules: vi.fn(async () => [...rules]),
    save: vi.fn(async (data: { rules: AutomationRule[] }) => {
      rules = [...data.rules];
    }),
    load: vi.fn(async () => ({ version: 1 as const, rules: [...rules] })),
    saveRule: vi.fn(),
    deleteRule: vi.fn(),
    _setRules(r: AutomationRule[]) { rules = [...r]; },
  };
}

// ── Tests ────────────────────────────────────────────────────────

describe("AutomationEngine", () => {
  let engine: AutomationEngine;
  let orchestrator: ReturnType<typeof createMockOrchestrator>;
  let pipeline: ReturnType<typeof createMockPipelineController>;
  let ruleStore: ReturnType<typeof createMockRuleStore>;

  beforeEach(() => {
    vi.useFakeTimers();
    orchestrator = createMockOrchestrator();
    pipeline = createMockPipelineController();
    ruleStore = createMockRuleStore();
    engine = new AutomationEngine(orchestrator, pipeline, ruleStore as any);
  });

  afterEach(() => {
    engine.dispose();
    vi.useRealTimers();
  });

  describe("fire() and rule matching", () => {
    it("executes matching rules for a trigger", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("does not execute rules with non-matching trigger type", async () => {
      const rule = makeRule({ trigger: { type: "label_added" } });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("does not execute disabled rules", async () => {
      const rule = makeRule({ enabled: false });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("limits to 50 rules per event", async () => {
      const rules = Array.from({ length: 60 }, (_, i) =>
        makeRule({ id: `rule-${i}` }),
      );
      engine.loadRulesSync(rules);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).toHaveBeenCalledTimes(50);
    });
  });

  describe("deduplication", () => {
    it("rejects duplicate triggers within 5s window", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());
      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("allows triggers after 5s window expires", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      vi.advanceTimersByTime(5001);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).toHaveBeenCalledTimes(2);
    });

    it("allows triggers for different cards within 5s", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({ card: makeCard({ id: "card-1" }) }));
      await engine.fire("card_moved", makeContext({ card: makeCard({ id: "card-2" }) }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(2);
    });

    it("cleans up dedup entries after 30s", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      // Advance past the cleanup interval (30s)
      vi.advanceTimersByTime(31_000);

      // The entry should be cleaned up, so a new trigger should work
      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).toHaveBeenCalledTimes(2);
    });
  });

  describe("condition evaluation", () => {
    it("label_matches: passes when a label matches the glob pattern", async () => {
      const rule = makeRule({
        conditions: [{ type: "label_matches", params: { pattern: "bug*" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ labels: ["bugfix", "frontend"] }),
      }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("label_matches: fails when no label matches", async () => {
      const rule = makeRule({
        conditions: [{ type: "label_matches", params: { pattern: "feature*" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ labels: ["bug", "frontend"] }),
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("label_matches: fails when labels are empty", async () => {
      const rule = makeRule({
        conditions: [{ type: "label_matches", params: { pattern: "*" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ labels: [] }),
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("complexity_below: passes when complexity is below threshold", async () => {
      const rule = makeRule({
        conditions: [{ type: "complexity_below", params: { threshold: 5 } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard(),
        eventData: { complexity: 3 },
      }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("complexity_below: fails when complexity is at or above threshold", async () => {
      const rule = makeRule({
        conditions: [{ type: "complexity_below", params: { threshold: 5 } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard(),
        eventData: { complexity: 5 },
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("complexity_below: fails when complexity data is unavailable", async () => {
      const rule = makeRule({
        conditions: [{ type: "complexity_below", params: { threshold: 5 } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("has_description: passes when description is non-empty", async () => {
      const rule = makeRule({
        conditions: [{ type: "has_description", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ description: "Some description" }),
      }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("has_description: fails when description is empty", async () => {
      const rule = makeRule({
        conditions: [{ type: "has_description", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ description: "" }),
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("has_description: fails when description is whitespace only", async () => {
      const rule = makeRule({
        conditions: [{ type: "has_description", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ description: "   " }),
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("no_linked_pr: passes when no PR is linked", async () => {
      const rule = makeRule({
        conditions: [{ type: "no_linked_pr", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ linkedPrNumber: undefined }),
      }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("no_linked_pr: fails when a PR is linked", async () => {
      const rule = makeRule({
        conditions: [{ type: "no_linked_pr", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ linkedPrNumber: 123 }),
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("assignee_is_me: passes when assignee matches current user", async () => {
      const rule = makeRule({
        conditions: [{ type: "assignee_is_me", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ assignee: "testuser" }),
        currentUser: "testuser",
      }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });

    it("assignee_is_me: fails when assignee does not match", async () => {
      const rule = makeRule({
        conditions: [{ type: "assignee_is_me", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ assignee: "otheruser" }),
        currentUser: "testuser",
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("assignee_is_me: fails when currentUser is not provided", async () => {
      const rule = makeRule({
        conditions: [{ type: "assignee_is_me", params: {} }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ assignee: "testuser" }),
        currentUser: undefined,
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("short-circuits on first false condition", async () => {
      const rule = makeRule({
        conditions: [
          { type: "has_description", params: {} },
          { type: "no_linked_pr", params: {} },
        ],
      });
      engine.loadRulesSync([rule]);

      // First condition fails (empty description)
      await engine.fire("card_moved", makeContext({
        card: makeCard({ description: "", linkedPrNumber: undefined }),
      }));

      expect(orchestrator.createTask).not.toHaveBeenCalled();
    });

    it("all conditions must pass for actions to execute", async () => {
      const rule = makeRule({
        conditions: [
          { type: "has_description", params: {} },
          { type: "no_linked_pr", params: {} },
        ],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ description: "Has desc", linkedPrNumber: undefined }),
      }));

      expect(orchestrator.createTask).toHaveBeenCalledTimes(1);
    });
  });

  describe("action execution", () => {
    it("executes spawn_agent action via orchestrator", async () => {
      const rule = makeRule({
        actions: [{ type: "spawn_agent", params: { repoPath: "/my/repo", agentType: "codex" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(orchestrator.createTask).toHaveBeenCalledWith(
        expect.any(String),
        "codex",
        "/my/repo",
      );
    });

    it("executes transition_issue action via pipeline controller", async () => {
      const rule = makeRule({
        actions: [{ type: "transition_issue", params: { targetStatus: "done" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(pipeline.transitionIssue).toHaveBeenCalledWith("card-1", "done");
    });

    it("executes add_comment action via pipeline controller", async () => {
      const rule = makeRule({
        actions: [{ type: "add_comment", params: { body: "Hello world" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(pipeline.addComment).toHaveBeenCalledWith("card-1", "Hello world");
    });

    it("executes iterate_agent action via orchestrator", async () => {
      const rule = makeRule({
        actions: [{ type: "iterate_agent", params: { feedback: "Fix the tests" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ linkedTaskId: "task-42" }),
      }));

      expect(orchestrator.iterate).toHaveBeenCalledWith("task-42", "Fix the tests");
    });

    it("iterate_agent fails when no linked task exists", async () => {
      const rule = makeRule({
        actions: [{ type: "iterate_agent", params: { feedback: "Fix it" } }],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext({
        card: makeCard({ linkedTaskId: undefined }),
      }));

      const log = engine.getExecutionLog();
      expect(log[0].actionResults[0].status).toBe("failed");
      expect(log[0].actionResults[0].error).toContain("No linked task");
    });

    it("executes actions sequentially in order", async () => {
      const callOrder: string[] = [];
      (orchestrator.createTask as any).mockImplementation(async () => {
        callOrder.push("spawn");
        return { id: "t1" };
      });
      (pipeline.addComment as any).mockImplementation(async () => {
        callOrder.push("comment");
      });

      const rule = makeRule({
        actions: [
          { type: "spawn_agent", params: { repoPath: "/repo" } },
          { type: "add_comment", params: { body: "Started" } },
        ],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(callOrder).toEqual(["spawn", "comment"]);
    });

    it("limits actions to 20 per rule", async () => {
      const actions = Array.from({ length: 25 }, () => ({
        type: "add_comment" as const,
        params: { body: "hi" },
      }));
      const rule = makeRule({ actions });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(pipeline.addComment).toHaveBeenCalledTimes(20);
    });
  });

  describe("action timeout", () => {
    it("fails action that exceeds 300s timeout", async () => {
      (orchestrator.createTask as any).mockImplementation(
        () => new Promise((resolve) => setTimeout(resolve, 400_000)),
      );

      const rule = makeRule();
      engine.loadRulesSync([rule]);

      const firePromise = engine.fire("card_moved", makeContext());
      vi.advanceTimersByTime(300_001);
      await firePromise;

      const log = engine.getExecutionLog();
      expect(log[0].actionResults[0].status).toBe("failed");
      expect(log[0].actionResults[0].error).toContain("timed out");
    });
  });

  describe("continueOnError", () => {
    it("halts on action failure when continueOnError is false", async () => {
      (orchestrator.createTask as any).mockRejectedValue(new Error("spawn failed"));

      const rule = makeRule({
        continueOnError: false,
        actions: [
          { type: "spawn_agent", params: { repoPath: "/repo" } },
          { type: "add_comment", params: { body: "done" } },
        ],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(pipeline.addComment).not.toHaveBeenCalled();
      const log = engine.getExecutionLog();
      expect(log[0].status).toBe("failed");
      expect(log[0].actionResults).toHaveLength(1);
    });

    it("continues to next action on failure when continueOnError is true", async () => {
      (orchestrator.createTask as any).mockRejectedValue(new Error("spawn failed"));

      const rule = makeRule({
        continueOnError: true,
        actions: [
          { type: "spawn_agent", params: { repoPath: "/repo" } },
          { type: "add_comment", params: { body: "done" } },
        ],
      });
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      expect(pipeline.addComment).toHaveBeenCalledTimes(1);
      const log = engine.getExecutionLog();
      expect(log[0].status).toBe("completed");
      expect(log[0].actionResults[0].status).toBe("failed");
      expect(log[0].actionResults[1].status).toBe("success");
    });
  });

  describe("execution log and events", () => {
    it("records execution for each rule evaluation", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      await engine.fire("card_moved", makeContext());

      const log = engine.getExecutionLog();
      expect(log).toHaveLength(1);
      expect(log[0].ruleId).toBe("rule-1");
      expect(log[0].cardId).toBe("card-1");
      expect(log[0].status).toBe("completed");
      expect(log[0].triggeredAt).toBeGreaterThan(0);
      expect(log[0].completedAt).toBeGreaterThan(0);
    });

    it("emits rule:executed event on success", async () => {
      const rule = makeRule();
      engine.loadRulesSync([rule]);

      const events: unknown[] = [];
      engine.on("rule:executed", (e) => events.push(e));

      await engine.fire("card_moved", makeContext());

      expect(events).toHaveLength(1);
      expect(events[0]).toEqual({ ruleId: "rule-1", cardId: "card-1" });
    });

    it("emits rule:failed event on failure", async () => {
      (orchestrator.createTask as any).mockRejectedValue(new Error("boom"));

      const rule = makeRule({ continueOnError: false });
      engine.loadRulesSync([rule]);

      const events: unknown[] = [];
      engine.on("rule:failed", (e) => events.push(e));

      await engine.fire("card_moved", makeContext());

      expect(events).toHaveLength(1);
      expect((events[0] as any).ruleId).toBe("rule-1");
      expect((events[0] as any).error).toContain("boom");
    });
  });

  describe("addRule / removeRule", () => {
    it("adds a rule and persists", async () => {
      const rule = makeRule();
      engine.addRule(rule);

      expect(engine.getRules()).toHaveLength(1);
      expect(ruleStore.save).toHaveBeenCalled();
    });

    it("updates existing rule by id", () => {
      const rule = makeRule();
      engine.addRule(rule);
      engine.addRule({ ...rule, name: "Updated" });

      expect(engine.getRules()).toHaveLength(1);
      expect(engine.getRules()[0].name).toBe("Updated");
    });

    it("removes a rule and persists", () => {
      const rule = makeRule();
      engine.addRule(rule);
      engine.removeRule("rule-1");

      expect(engine.getRules()).toHaveLength(0);
    });
  });

  describe("loadRules", () => {
    it("loads rules from the store", async () => {
      ruleStore._setRules([makeRule()]);

      engine.loadRules();
      // loadRules is async internally, advance microtasks
      await vi.advanceTimersByTimeAsync(0);

      expect(engine.getRules()).toHaveLength(1);
    });

    it("handles store errors gracefully without crashing", async () => {
      (ruleStore.getRules as any).mockRejectedValue(new Error("disk error"));

      engine.loadRules();
      await vi.advanceTimersByTimeAsync(0);

      expect(engine.getRules()).toHaveLength(0);
    });
  });
});

describe("Cron Parser", () => {
  describe("parseCronField", () => {
    it("parses wildcard", () => {
      const result = parseCronField("*", 0, 59);
      expect(result).toHaveLength(60);
      expect(result[0]).toBe(0);
      expect(result[59]).toBe(59);
    });

    it("parses single value", () => {
      expect(parseCronField("5", 0, 59)).toEqual([5]);
    });

    it("parses range", () => {
      expect(parseCronField("1-5", 0, 59)).toEqual([1, 2, 3, 4, 5]);
    });

    it("parses step with wildcard", () => {
      expect(parseCronField("*/15", 0, 59)).toEqual([0, 15, 30, 45]);
    });

    it("parses step with range", () => {
      expect(parseCronField("1-10/3", 0, 59)).toEqual([1, 4, 7, 10]);
    });

    it("parses comma-separated values", () => {
      expect(parseCronField("1,5,10", 0, 59)).toEqual([1, 5, 10]);
    });

    it("ignores values outside range", () => {
      expect(parseCronField("70", 0, 59)).toEqual([]);
    });
  });

  describe("parseCronExpression", () => {
    it("parses a valid 5-field cron expression", () => {
      const result = parseCronExpression("0 9 * * 1-5");
      expect(result).not.toBeNull();
      expect(result!.minutes).toEqual([0]);
      expect(result!.hours).toEqual([9]);
      expect(result!.daysOfWeek).toEqual([1, 2, 3, 4, 5]);
    });

    it("returns null for invalid expression (wrong number of fields)", () => {
      expect(parseCronExpression("0 9 *")).toBeNull();
    });

    it("parses every-minute expression", () => {
      const result = parseCronExpression("* * * * *");
      expect(result).not.toBeNull();
      expect(result!.minutes).toHaveLength(60);
      expect(result!.hours).toHaveLength(24);
    });
  });

  describe("cronMatchesNow", () => {
    it("matches when current time fits the schedule", () => {
      const now = new Date();
      const schedule = {
        minutes: [now.getMinutes()],
        hours: [now.getHours()],
        daysOfMonth: [now.getDate()],
        months: [now.getMonth() + 1],
        daysOfWeek: [now.getDay()],
      };
      expect(cronMatchesNow(schedule)).toBe(true);
    });

    it("does not match when minute differs", () => {
      const now = new Date();
      const wrongMinute = (now.getMinutes() + 1) % 60;
      const schedule = {
        minutes: [wrongMinute],
        hours: [now.getHours()],
        daysOfMonth: [now.getDate()],
        months: [now.getMonth() + 1],
        daysOfWeek: [now.getDay()],
      };
      expect(cronMatchesNow(schedule)).toBe(false);
    });
  });
});
