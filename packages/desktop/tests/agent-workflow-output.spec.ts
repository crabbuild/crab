import { describe, expect, it } from "vitest";

import type { AgentWorkflowRun } from "../electron/agent-workflows/types";
import { parseStructuredOutput, resolveTemplate } from "../electron/agent-workflows/output";

describe("Agent workflow structured output", () => {
  it("parses the last fenced json block and validates schema", () => {
    const result = parseStructuredOutput(
      [
        "Summary",
        "```json",
        "{\"risk\":{\"level\":\"low\"}}",
        "```",
        "```json",
        "{\"risk\":{\"level\":\"high\"},\"files\":[\"a.ts\"]}",
        "```",
      ].join("\n"),
      {
        type: "object",
        required: ["risk"],
        properties: {
          risk: {
            type: "object",
            required: ["level"],
            properties: { level: { type: "string" } },
          },
          files: { type: "array", items: { type: "string" } },
        },
      },
    );

    expect(result.outputJson).toEqual({ risk: { level: "high" }, files: ["a.ts"] });
    expect(result.outputJsonError).toBeUndefined();
  });

  it("returns an error when JSON does not satisfy required schema fields", () => {
    const result = parseStructuredOutput(
      "```json\n{\"risk\":{}}\n```",
      {
        type: "object",
        properties: {
          risk: {
            type: "object",
            required: ["level"],
            properties: { level: { type: "string" } },
          },
        },
      },
    );

    expect(result.outputJson).toBeUndefined();
    expect(result.outputJsonError).toContain("$.risk.level is required");
  });

  it("resolves known template tokens and preserves unknown families", () => {
    const run = {
      id: "run",
      definitionId: "wf",
      repoPath: "/tmp/repo",
      status: "running",
      definitionSnapshot: {
        schemaVersion: 2,
        id: "wf",
        name: "Workflow",
        definitionVersion: 1,
        revisionId: "rev",
        maxParallelNodes: 1,
        defaultAgentType: "codex",
        nodes: [],
        edges: [],
        layout: {},
        createdAt: 1,
        updatedAt: 1,
      },
      input: { scope: "auth" },
      nodeRuns: [
        {
          id: "node-run-a",
          nodeId: "a",
          status: "completed",
          attempt: 1,
          outputSummary: "A summary",
          outputJson: { risk: { level: "high" } },
        },
      ],
      edgeRuns: [],
      startedAt: 1,
    } satisfies AgentWorkflowRun;

    expect(resolveTemplate(
      "{{run.input.scope}} {{node.summary.a}} {{node.output.a.risk.level}} {{unknown.value}}",
      { run },
    )).toBe("auth A summary high {{unknown.value}}");
  });
});
