import { isCelError, parse, run as runCel } from "@bufbuild/cel";
import type { CelInput } from "@bufbuild/cel";
import { strings } from "@bufbuild/cel/ext";

import type { AgentWorkflowRun } from "./types";

export interface WorkflowCelEvaluation {
  activated: boolean;
  reason: string;
}

export function validateCelExpressionSyntax(expression: string): string | null {
  try {
    parse(expression);
    return null;
  } catch (err) {
    return err instanceof Error ? err.message : String(err);
  }
}

export function evaluateWorkflowCelCondition(
  expression: string,
  run: AgentWorkflowRun,
  nodeId: string,
): WorkflowCelEvaluation {
  const trimmed = expression.trim();
  if (!trimmed) {
    return { activated: false, reason: "cel_expression_missing" };
  }

  let result: unknown;
  try {
    result = runCel(trimmed, buildWorkflowCelBindings(run, nodeId), { funcs: strings });
  } catch (err) {
    return { activated: false, reason: `cel_error:${err instanceof Error ? err.message : String(err)}` };
  }
  if (isCelError(result)) return { activated: false, reason: `cel_error:${result.message}` };
  if (typeof result !== "boolean") {
    return { activated: false, reason: `cel_non_boolean:${typeof result}` };
  }
  return {
    activated: result,
    reason: result ? "cel_matched" : "cel_not_matched",
  };
}

export function buildWorkflowCelBindings(
  run: AgentWorkflowRun,
  nodeId: string,
): Record<string, CelInput> {
  const nodeEntries: Record<string, unknown> = {};
  for (const node of run.definitionSnapshot.nodes) {
    const nodeRun = run.nodeRuns.find((candidate) => candidate.nodeId === node.id);
    nodeEntries[node.id] = {
      output: nodeRun?.outputJson ?? null,
      summary: nodeRun?.outputSummary ?? "",
      status: nodeRun?.status ?? "blocked",
    };
  }

  return {
    state: toCelInput(run.state ?? {}),
    input: toCelInput(buildActivatedInput(run, nodeId)),
    workflow: toCelInput({
      input: run.input ?? {},
      input_as_text: workflowInputAsText(run.input),
    }),
    node: toCelInput(nodeEntries),
  };
}

function buildActivatedInput(run: AgentWorkflowRun, nodeId: string): unknown {
  const activatedIncoming = run.definitionSnapshot.edges
    .filter((edge) => edge.targetNodeId === nodeId)
    .filter((edge) => run.edgeRuns.find((edgeRun) => edgeRun.edgeId === edge.id)?.status === "activated");

  if (activatedIncoming.length === 0) return run.input ?? {};
  if (activatedIncoming.length === 1) {
    const sourceRun = run.nodeRuns.find((candidate) => candidate.nodeId === activatedIncoming[0].sourceNodeId);
    return sourceRun?.outputJson ?? {};
  }

  return Object.fromEntries(activatedIncoming.map((edge) => {
    const sourceRun = run.nodeRuns.find((candidate) => candidate.nodeId === edge.sourceNodeId);
    return [edge.sourceNodeId, sourceRun?.outputJson ?? {}];
  }));
}

function workflowInputAsText(input: Record<string, unknown> | undefined): string {
  if (!input) return "{}";
  const textValue = input.input_as_text ?? input.text ?? input.prompt ?? input.input;
  if (typeof textValue === "string") return textValue;
  return JSON.stringify(input);
}

function toCelInput(value: unknown): CelInput {
  if (value === undefined || value === null) return null;
  if (typeof value === "string" || typeof value === "boolean" || typeof value === "number") return value;
  if (typeof value === "bigint") return value;
  if (Array.isArray(value)) return value.map((item) => toCelInput(item));
  if (isPlainObject(value)) {
    return Object.fromEntries(
      Object.entries(value).map(([key, child]) => [key, toCelInput(child)]),
    );
  }
  return String(value);
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
