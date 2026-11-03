import { describe, expect, it } from "vitest";

import type { AgentWorkflowDefinition } from "../electron/agent-workflows/types";
import {
  evaluateWorkflowEdgeCondition,
  validateAgentWorkflowDefinition,
} from "../electron/agent-workflows/validator";

function baseDefinition(): AgentWorkflowDefinition {
  return {
    schemaVersion: 2,
    id: "wf-test",
    name: "Test Workflow",
    definitionVersion: 1,
    revisionId: "rev-1",
    maxParallelNodes: 2,
    defaultAgentType: "codex",
    nodes: [
      { id: "a", type: "agent_task", title: "A", instructions: "Do A" },
      { id: "b", type: "human_review", title: "B", instructions: "Review B" },
    ],
    edges: [{ id: "a-b", sourceNodeId: "a", targetNodeId: "b", condition: { sourceStatus: "completed" } }],
    layout: {},
    createdAt: 1,
    updatedAt: 1,
  };
}

describe("Agent Workflow DAG validation", () => {
  it("accepts a valid DAG and returns topological order", () => {
    const result = validateAgentWorkflowDefinition(baseDefinition());

    expect(result.valid).toBe(true);
    expect(result.errors).toEqual([]);
    expect(result.topologicalNodeIds).toEqual(["a", "b"]);
  });

  it("rejects empty graphs", () => {
    const definition = baseDefinition();
    definition.nodes = [];
    definition.edges = [];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors).toContain("Workflow must contain at least one node.");
  });

  it("rejects missing edge endpoints", () => {
    const definition = baseDefinition();
    definition.edges = [{ id: "missing", sourceNodeId: "a", targetNodeId: "z" }];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors.some((error) => error.includes("missing target node"))).toBe(true);
  });

  it("allows multiple incoming edges and duplicate node pairs when edge ids differ", () => {
    const definition = baseDefinition();
    definition.nodes.push({ id: "c", type: "join", title: "Join", instructions: "" });
    definition.edges = [
      { id: "a-c-success", sourceNodeId: "a", targetNodeId: "c", condition: { sourceStatus: "completed" } },
      { id: "a-c-failure", sourceNodeId: "a", targetNodeId: "c", condition: { sourceStatus: "failed" } },
      { id: "b-c", sourceNodeId: "b", targetNodeId: "c" },
    ];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(true);
  });

  it("rejects duplicate edge ids and self edges", () => {
    const definition = baseDefinition();
    definition.edges = [
      { id: "dup", sourceNodeId: "a", targetNodeId: "b" },
      { id: "dup", sourceNodeId: "a", targetNodeId: "b" },
      { id: "self", sourceNodeId: "a", targetNodeId: "a" },
    ];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors).toContain("Duplicate edge id: dup.");
    expect(result.errors).toContain("Edge self cannot connect a node to itself.");
  });

  it("rejects cycles", () => {
    const definition = baseDefinition();
    definition.edges = [
      { id: "a-b", sourceNodeId: "a", targetNodeId: "b" },
      { id: "b-a", sourceNodeId: "b", targetNodeId: "a" },
    ];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors).toContain("Workflow graph contains a cycle.");
    expect(result.cyclePath?.length).toBeGreaterThan(0);
  });

  it("validates conditional edge predicates", () => {
    const definition = baseDefinition();
    definition.edges = [{
      id: "bad-condition",
      sourceNodeId: "a",
      targetNodeId: "b",
      condition: {
        sourceStatus: "completed",
        outputPredicate: { path: "risk.level", operator: "in", value: "high" },
      },
    }];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors).toContain('Edge bad-condition operator "in" requires an array value.');
  });

  it("accepts if/else and bounded loop nodes", () => {
    const definition = baseDefinition();
    definition.nodes = [
      {
        id: "branch",
        type: "if_else",
        title: "Branch",
        instructions: "",
        ifElse: {
          condition: {
            left: { source: "runInput", path: "flag" },
            operator: "equals",
            value: true,
          },
        },
      },
      {
        id: "loop",
        type: "loop",
        title: "Loop",
        instructions: "",
        subWorkflowDefinitionId: "child",
        loop: { maxIterations: 3 },
      },
    ];
    definition.edges = [{ id: "branch-loop", sourceNodeId: "branch", targetNodeId: "loop" }];

    const child = baseDefinition();
    child.id = "child";
    const result = validateAgentWorkflowDefinition(definition, {
      definitionsById: new Map([[child.id, child]]),
    });

    expect(result.valid).toBe(true);
  });

  it("accepts ordered if/elif/else branches and extended condition operators", () => {
    const definition = baseDefinition();
    definition.nodes = [
      {
        id: "branch",
        type: "if_else",
        title: "Branch",
        instructions: "",
        ifElse: {
          branches: [
            {
              id: "if-title",
              label: "Title",
              kind: "if",
              condition: {
                logic: "and",
                conditions: [
                  { left: { source: "runInput", path: "title" }, operator: "starts_with", value: "Fix" },
                  { left: { source: "runInput", path: "body" }, operator: "not_contains", value: "WIP" },
                ],
              },
            },
            {
              id: "elif-empty",
              label: "Empty",
              kind: "elif",
              condition: {
                logic: "or",
                conditions: [
                  { left: { source: "runInput", path: "assignee" }, operator: "is_empty" },
                  { left: { source: "runInput", path: "labels" }, operator: "is_not_empty" },
                ],
              },
            },
            { id: "else", label: "Else", kind: "else" },
          ],
        },
      },
      { id: "next", type: "human_review", title: "Next", instructions: "Review" },
    ];
    definition.edges = [{ id: "branch-next", sourceNodeId: "branch", targetNodeId: "next" }];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(true);
  });

  it("accepts CEL if/elif/else branches and branch dock edges", () => {
    const definition = baseDefinition();
    definition.nodes = [
      {
        id: "branch",
        type: "if_else",
        title: "Branch",
        instructions: "",
        ifElse: {
          branches: [
            { id: "gold", label: "Gold", kind: "if", celExpression: 'state.customer.tier == "gold"' },
            { id: "else", label: "Else", kind: "else" },
          ],
        },
      },
      { id: "next", type: "human_review", title: "Next", instructions: "Review" },
    ];
    definition.edges = [{ id: "branch-next", sourceNodeId: "branch", targetNodeId: "next", condition: { sourceStatus: "completed", branchId: "gold" } }];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(true);
  });

  it("rejects invalid CEL, missing branch docks, and invalid state update paths", () => {
    const definition = baseDefinition();
    definition.nodes = [
      {
        id: "branch",
        type: "if_else",
        title: "Branch",
        instructions: "",
        ifElse: {
          branches: [
            { id: "gold", label: "Gold", kind: "if", celExpression: "input." },
            { id: "else", label: "Else", kind: "else" },
          ],
        },
      },
      {
        id: "next",
        type: "human_review",
        title: "Next",
        instructions: "Review",
        stateUpdates: { "__proto__.polluted": { source: "runInput" } },
      },
    ];
    definition.edges = [{ id: "branch-next", sourceNodeId: "branch", targetNodeId: "next", condition: { sourceStatus: "completed", branchId: "missing" } }];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors.some((error) => error.includes("CEL expression is invalid"))).toBe(true);
    expect(result.errors).toContain("Edge branch-next references missing if/else branch missing.");
    expect(result.errors).toContain('Node Next state update path "__proto__.polluted" is invalid.');
  });

  it("rejects malformed if/else branch ordering", () => {
    const definition = baseDefinition();
    definition.nodes = [{
      id: "branch",
      type: "if_else",
      title: "Branch",
      instructions: "",
      ifElse: {
        branches: [
          { id: "else", label: "Else", kind: "else" },
          { id: "elif", label: "Elif", kind: "elif", condition: { logic: "and", conditions: [{ left: { source: "runInput" }, operator: "exists" }] } },
        ],
      },
    }];
    definition.edges = [];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors).toContain("If/else node Branch first branch must be IF.");
    expect(result.errors).toContain("If/else node Branch ELSE branch must be last.");
  });

  it("rejects invalid loop configuration", () => {
    const definition = baseDefinition();
    definition.nodes = [{
      id: "loop",
      type: "loop",
      title: "Loop",
      instructions: "",
      loop: { maxIterations: 0 },
    }];
    definition.edges = [];

    const result = validateAgentWorkflowDefinition(definition);

    expect(result.valid).toBe(false);
    expect(result.errors).toContain("Loop node Loop maxIterations must be at least 1.");
    expect(result.errors).toContain("Loop node Loop must reference subWorkflowDefinitionId.");
  });

  it("evaluates safe output predicates without code execution", () => {
    const result = evaluateWorkflowEdgeCondition(
      {
        id: "edge",
        sourceNodeId: "a",
        targetNodeId: "b",
        condition: {
          sourceStatus: "completed",
          outputPredicate: { path: "risk.level", operator: "equals", value: "high" },
        },
      },
      {
        id: "run-a",
        nodeId: "a",
        status: "completed",
        attempt: 1,
        outputJson: { risk: { level: "high" } },
      },
    );

    expect(result.activated).toBe(true);
  });
});
