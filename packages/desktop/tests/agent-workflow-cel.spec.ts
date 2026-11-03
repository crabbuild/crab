import { describe, expect, it } from "vitest";

import { buildWorkflowCelBindings, evaluateWorkflowCelCondition } from "../electron/agent-workflows/cel";
import type { AgentWorkflowRun } from "../electron/agent-workflows/types";

function runFixture(): AgentWorkflowRun {
  return {
    id: "run",
    definitionId: "wf",
    definitionSnapshot: {
      schemaVersion: 2,
      id: "wf",
      name: "Workflow",
      definitionVersion: 1,
      revisionId: "rev",
      maxParallelNodes: 2,
      defaultAgentType: "codex",
      nodes: [
        { id: "classify", type: "agent_task", title: "Classify", instructions: "Classify" },
        { id: "branch", type: "if_else", title: "Branch", instructions: "" },
      ],
      edges: [{ id: "classify-branch", sourceNodeId: "classify", targetNodeId: "branch", condition: { sourceStatus: "completed" } }],
      layout: {},
      createdAt: 1,
      updatedAt: 1,
    },
    repoPath: "/tmp/repo",
    status: "running",
    input: { input_as_text: "hello", score: 0.95 },
    state: {
      customer: { tier: "gold" },
      flags: { beta: true },
      employees: ["Patrick Star"],
      emails: ["a@example.com", "b@example.com"],
    },
    nodeRuns: [
      {
        id: "node-run-classify",
        nodeId: "classify",
        status: "completed",
        attempt: 1,
        outputJson: { results: [3], score: 0.95 },
        outputSummary: "classified",
      },
      { id: "node-run-branch", nodeId: "branch", status: "ready", attempt: 1 },
    ],
    edgeRuns: [{ edgeId: "classify-branch", status: "activated" }],
    startedAt: 1,
  };
}

describe("Agent workflow CEL", () => {
  it("builds state, input, workflow, and node bindings", () => {
    const bindings = buildWorkflowCelBindings(runFixture(), "branch");

    expect(bindings.workflow).toMatchObject({ input_as_text: "hello" });
    expect(bindings.input).toMatchObject({ results: [3], score: 0.95 });
    expect(bindings.state).toMatchObject({ customer: { tier: "gold" } });
    expect(bindings.node).toMatchObject({ classify: { summary: "classified", status: "completed" } });
  });

  it("evaluates CEL comparisons, ternaries, membership, and macros", () => {
    const run = runFixture();

    expect(evaluateWorkflowCelCondition('state.customer.tier == "gold"', run, "branch").activated).toBe(true);
    expect(evaluateWorkflowCelCondition("input.results[0] == 3", run, "branch").activated).toBe(true);
    expect(evaluateWorkflowCelCondition('workflow.input_as_text == "hello"', run, "branch").activated).toBe(true);
    expect(evaluateWorkflowCelCondition("input.score > (state.flags.beta ? 0.9 : 0.8)", run, "branch").activated).toBe(true);
    expect(evaluateWorkflowCelCondition('"Patrick Star" in state.employees', run, "branch").activated).toBe(true);
    expect(evaluateWorkflowCelCondition('state.emails.all(email, email.contains("@"))', run, "branch").activated).toBe(true);
  });

  it("returns an explicit failure reason for non-boolean expressions", () => {
    const result = evaluateWorkflowCelCondition("state.customer.tier", runFixture(), "branch");

    expect(result.activated).toBe(false);
    expect(result.reason).toContain("cel_non_boolean");
  });
});
