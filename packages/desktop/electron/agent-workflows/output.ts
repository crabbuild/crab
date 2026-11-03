import type {
  AgentWorkflowDefinition,
  AgentWorkflowNodeRun,
  AgentWorkflowRun,
  JoinConfig,
  JsonSchemaObject,
  WorkflowValueRef,
} from "./types";
import { readPath } from "./validator";

export interface StructuredOutputResult {
  outputJson?: unknown;
  outputJsonError?: string;
}

export function parseStructuredOutput(
  text: string,
  schema: JsonSchemaObject | undefined,
): StructuredOutputResult {
  if (!schema) return {};

  const candidates = extractJsonCandidates(text);
  if (candidates.length === 0) {
    return { outputJsonError: "No JSON object found in node output." };
  }

  const parseErrors: string[] = [];
  for (const candidate of candidates) {
    try {
      const parsed = JSON.parse(candidate) as unknown;
      const errors = validateJsonSchemaSubset(parsed, schema);
      if (errors.length === 0) {
        return { outputJson: parsed };
      }
      parseErrors.push(errors.join("; "));
    } catch (err: unknown) {
      parseErrors.push(err instanceof Error ? err.message : String(err));
    }
  }

  return {
    outputJsonError: parseErrors[0] || "JSON output did not match the configured schema.",
  };
}

export function validateJsonSchemaSubset(value: unknown, schema: JsonSchemaObject, path = "$"): string[] {
  const errors: string[] = [];
  const type = schema.type;
  if (typeof type === "string" && !matchesType(value, type)) {
    errors.push(`${path} must be ${type}.`);
    return errors;
  }

  if (Array.isArray(schema.enum) && !schema.enum.some((candidate) => deepEqual(candidate, value))) {
    errors.push(`${path} must be one of the configured enum values.`);
  }

  if (type === "object" || (type === undefined && isPlainObject(value) && isPlainObject(schema.properties))) {
    if (!isPlainObject(value)) {
      errors.push(`${path} must be object.`);
      return errors;
    }
    const required = Array.isArray(schema.required) ? schema.required.filter((item): item is string => typeof item === "string") : [];
    for (const key of required) {
      if (value[key] === undefined) errors.push(`${path}.${key} is required.`);
    }
    const properties = isPlainObject(schema.properties) ? schema.properties : {};
    for (const [key, childSchema] of Object.entries(properties)) {
      if (value[key] !== undefined && isPlainObject(childSchema)) {
        errors.push(...validateJsonSchemaSubset(value[key], childSchema, `${path}.${key}`));
      }
    }
    if (schema.additionalProperties === false) {
      const allowed = new Set(Object.keys(properties));
      for (const key of Object.keys(value)) {
        if (!allowed.has(key)) errors.push(`${path}.${key} is not allowed.`);
      }
    }
  }

  if (type === "array" || (type === undefined && Array.isArray(value) && isPlainObject(schema.items))) {
    if (!Array.isArray(value)) {
      errors.push(`${path} must be array.`);
      return errors;
    }
    if (isPlainObject(schema.items)) {
      value.forEach((item, index) => {
        errors.push(...validateJsonSchemaSubset(item, schema.items as JsonSchemaObject, `${path}.${index}`));
      });
    }
  }

  return errors;
}

export function resolveTemplate(
  text: string,
  context: {
    card?: { title?: string; description?: string; labels?: string[] };
    run: AgentWorkflowRun;
  },
): string {
  return text.replace(/\{\{\s*([^}]+?)\s*\}\}/g, (token, rawPath: string) => {
    const value = resolveToken(rawPath.trim(), context);
    if (value === undefined) {
      return isKnownTokenFamily(rawPath.trim()) ? "" : token;
    }
    if (Array.isArray(value)) return value.join(", ");
    if (typeof value === "object" && value !== null) return JSON.stringify(value);
    return String(value);
  });
}

export function resolveValueRef(ref: WorkflowValueRef, run: AgentWorkflowRun): unknown {
  switch (ref.source) {
    case "literal":
      return ref.value;
    case "runInput":
      return readPath(run.input, ref.path);
    case "nodeSummary": {
      const nodeRun = ref.nodeId ? findNodeRunByNodeId(run, ref.nodeId) : undefined;
      return nodeRun?.outputSummary;
    }
    case "nodeOutput": {
      const nodeRun = ref.nodeId ? findNodeRunByNodeId(run, ref.nodeId) : undefined;
      return readPath(nodeRun?.outputJson, ref.path);
    }
    case "card":
      return undefined;
  }
}

export function aggregateJoinOutput(
  definition: AgentWorkflowDefinition,
  run: AgentWorkflowRun,
  nodeId: string,
  joinConfig?: JoinConfig,
): { outputSummary: string; outputJson: Record<string, unknown> } {
  const activatedSourceIds = definition.edges
    .filter((edge) => edge.targetNodeId === nodeId)
    .filter((edge) => run.edgeRuns.find((edgeRun) => edgeRun.edgeId === edge.id)?.status === "activated")
    .map((edge) => edge.sourceNodeId);

  const entries: Array<{ nodeId: string; title: string; summary?: string; outputJson?: unknown }> = [];
  for (const sourceNodeId of activatedSourceIds) {
    const sourceNode = definition.nodes.find((node) => node.id === sourceNodeId);
    const sourceRun = run.nodeRuns.find((nodeRun) => nodeRun.nodeId === sourceNodeId);
    if (!sourceRun) continue;
    entries.push({
      nodeId: sourceNodeId,
      title: sourceNode?.title ?? sourceNodeId,
      summary: sourceRun.outputSummary,
      outputJson: sourceRun.outputJson,
    });
  }

  const strategy = joinConfig?.strategy ?? "all";

  if (strategy === "first" && entries.length > 0) {
    return {
      outputSummary: entries[0].summary ?? "Join completed.",
      outputJson: (entries[0].outputJson as Record<string, unknown>) ?? {},
    };
  }
  if (strategy === "last" && entries.length > 0) {
    const last = entries[entries.length - 1];
    return {
      outputSummary: last.summary ?? "Join completed.",
      outputJson: (last.outputJson as Record<string, unknown>) ?? {},
    };
  }
  if (strategy === "concat") {
    const template = joinConfig?.customTemplate ?? "{{summaries}}";
    const summariesText = entries.map((e) => e.summary ?? "").filter(Boolean).join("\n");
    const summary = template.replace(/\{\{summaries\}\}/g, summariesText);
    const json: Record<string, unknown> = {};
    for (const entry of entries) {
      if (entry.outputJson !== undefined) json[entry.nodeId] = entry.outputJson;
    }
    return { outputSummary: summary || "Join completed.", outputJson: json };
  }

  // "all" (default)
  const summaries: string[] = [];
  const outputJson: Record<string, unknown> = {};
  for (const entry of entries) {
    if (entry.summary) summaries.push(`${entry.title}: ${entry.summary}`);
    if (entry.outputJson !== undefined) outputJson[entry.nodeId] = entry.outputJson;
  }
  return {
    outputSummary: summaries.length > 0 ? summaries.join("\n") : "Join completed.",
    outputJson,
  };
}

function resolveToken(
  path: string,
  context: { card?: { title?: string; description?: string; labels?: string[] }; run: AgentWorkflowRun },
): unknown {
  if (path === "card.title") return context.card?.title;
  if (path === "card.description") return context.card?.description;
  if (path === "card.labels") return context.card?.labels;
  if (path.startsWith("run.input.")) return readPath(context.run.input, path.slice("run.input.".length));
  if (path.startsWith("node.summary.")) {
    const nodeId = path.slice("node.summary.".length);
    return findNodeRunByNodeId(context.run, nodeId)?.outputSummary;
  }
  if (path.startsWith("node.output.")) {
    const rest = path.slice("node.output.".length);
    const [nodeId, ...pathParts] = rest.split(".");
    return readPath(findNodeRunByNodeId(context.run, nodeId)?.outputJson, pathParts.join("."));
  }
  return undefined;
}

function isKnownTokenFamily(path: string): boolean {
  return path.startsWith("card.") || path.startsWith("run.input.") || path.startsWith("node.summary.") || path.startsWith("node.output.");
}

function findNodeRunByNodeId(run: AgentWorkflowRun, nodeId: string): AgentWorkflowNodeRun | undefined {
  return run.nodeRuns.find((nodeRun) => nodeRun.nodeId === nodeId);
}

function extractJsonCandidates(text: string): string[] {
  const candidates: string[] = [];
  const fencedJson = [...text.matchAll(/```json\s*([\s\S]*?)```/gi)].map((match) => match[1]?.trim()).filter(Boolean) as string[];
  candidates.push(...fencedJson.reverse());

  const fenced = [...text.matchAll(/```\w*\s*([\s\S]*?)```/g)].map((match) => match[1]?.trim()).filter(Boolean) as string[];
  candidates.push(...fenced.reverse().filter((candidate) => !candidates.includes(candidate)));

  const objectCandidate = findLastJsonObject(text);
  if (objectCandidate) candidates.push(objectCandidate);

  const trimmed = text.trim();
  if ((trimmed.startsWith("{") && trimmed.endsWith("}")) || (trimmed.startsWith("[") && trimmed.endsWith("]"))) {
    candidates.push(trimmed);
  }

  return [...new Set(candidates)];
}

function findLastJsonObject(text: string): string | null {
  for (let end = text.length - 1; end >= 0; end -= 1) {
    if (text[end] !== "}") continue;
    let depth = 0;
    let inString = false;
    let escaped = false;
    for (let start = end; start >= 0; start -= 1) {
      const char = text[start];
      if (inString) {
        if (escaped) {
          escaped = false;
        } else if (char === "\\") {
          escaped = true;
        } else if (char === "\"") {
          inString = false;
        }
        continue;
      }
      if (char === "\"") {
        inString = true;
      } else if (char === "}") {
        depth += 1;
      } else if (char === "{") {
        depth -= 1;
        if (depth === 0) {
          return text.slice(start, end + 1).trim();
        }
      }
    }
  }
  return null;
}

function matchesType(value: unknown, type: string): boolean {
  switch (type) {
    case "object":
      return isPlainObject(value);
    case "array":
      return Array.isArray(value);
    case "string":
      return typeof value === "string";
    case "number":
      return typeof value === "number" && Number.isFinite(value);
    case "integer":
      return Number.isInteger(value);
    case "boolean":
      return typeof value === "boolean";
    case "null":
      return value === null;
    default:
      return true;
  }
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== typeof b) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((item, index) => deepEqual(item, b[index]));
  }
  if (isPlainObject(a) && isPlainObject(b)) {
    const aKeys = Object.keys(a);
    const bKeys = Object.keys(b);
    return aKeys.length === bKeys.length && aKeys.every((key) => deepEqual(a[key], b[key]));
  }
  return false;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
