import type {
  AgentWorkflowDefinition,
  AgentWorkflowEdge,
  AgentWorkflowNode,
  AgentWorkflowNodeRun,
  AgentWorkflowValidationResult,
  WorkflowConditionGroup,
  WorkflowEdgeCondition,
  WorkflowNodeCondition,
  WorkflowOutputPredicate,
} from "./types";
import { validateCelExpressionSyntax } from "./cel";

const SUPPORTED_NODE_TYPES = new Set([
  "agent_task",
  "human_review",
  "approval_gate",
  "join",
  "sub_workflow",
  "if_else",
  "loop",
]);

const SUPPORTED_SOURCE_STATUSES = new Set([
  "completed",
  "failed",
  "cancelled",
  "skipped",
  "always",
]);

const SUPPORTED_OPERATORS = new Set([
  "exists",
  "not_exists",
  "equals",
  "not_equals",
  "contains",
  "not_contains",
  "starts_with",
  "ends_with",
  "is_empty",
  "is_not_empty",
  "greater_than",
  "less_than",
  "in",
]);

export interface AgentWorkflowValidationOptions {
  definitionsById?: Map<string, AgentWorkflowDefinition>;
}

export function validateAgentWorkflowDefinition(
  definition: AgentWorkflowDefinition,
  options: AgentWorkflowValidationOptions = {},
): AgentWorkflowValidationResult {
  const errors: string[] = [];
  const warnings: string[] = [];

  if (!definition || typeof definition !== "object") {
    return {
      valid: false,
      errors: ["Workflow definition is required."],
      warnings,
      topologicalNodeIds: [],
    };
  }

  if (definition.schemaVersion !== 2) {
    errors.push("Workflow schemaVersion must be 2.");
  }
  if (!definition.id || typeof definition.id !== "string") {
    errors.push("Workflow id is required.");
  }
  if (!definition.name || typeof definition.name !== "string") {
    errors.push("Workflow name is required.");
  }
  if (!Number.isInteger(definition.definitionVersion) || definition.definitionVersion < 1) {
    errors.push("definitionVersion must be a positive integer.");
  }
  if (!definition.revisionId || typeof definition.revisionId !== "string") {
    errors.push("revisionId is required.");
  }
  if (!Number.isFinite(definition.maxParallelNodes) || definition.maxParallelNodes < 1) {
    errors.push("maxParallelNodes must be at least 1.");
  }
  if (!Array.isArray(definition.nodes) || definition.nodes.length === 0) {
    errors.push("Workflow must contain at least one node.");
  }
  if (!Array.isArray(definition.edges)) {
    errors.push("Workflow edges must be an array.");
  }
  if (!definition.layout || typeof definition.layout !== "object") {
    errors.push("Workflow layout must be an object.");
  }

  const nodeIds = new Set<string>();
  const duplicateNodeIds = new Set<string>();
  for (const node of definition.nodes ?? []) {
    validateNode(node, definition, errors, warnings);
    if (!node.id) continue;
    if (nodeIds.has(node.id)) duplicateNodeIds.add(node.id);
    nodeIds.add(node.id);
  }
  for (const id of duplicateNodeIds) {
    errors.push(`Duplicate node id: ${id}.`);
  }

  const edgeIds = new Set<string>();
  for (const edge of definition.edges ?? []) {
    if (!edge.id) {
      errors.push("Every edge must have an id.");
    } else if (edgeIds.has(edge.id)) {
      errors.push(`Duplicate edge id: ${edge.id}.`);
    }
    edgeIds.add(edge.id);

    if (!edge.sourceNodeId || !nodeIds.has(edge.sourceNodeId)) {
      errors.push(`Edge ${edge.id || "(missing id)"} references missing source node ${edge.sourceNodeId || "(empty)"}.`);
    }
    if (!edge.targetNodeId || !nodeIds.has(edge.targetNodeId)) {
      errors.push(`Edge ${edge.id || "(missing id)"} references missing target node ${edge.targetNodeId || "(empty)"}.`);
    }
    if (edge.sourceNodeId && edge.sourceNodeId === edge.targetNodeId) {
      errors.push(`Edge ${edge.id || edge.sourceNodeId} cannot connect a node to itself.`);
    }
    const sourceNode = definition.nodes.find((node) => node.id === edge.sourceNodeId);
    validateCondition(edge.condition, edge.id || "(missing id)", sourceNode, errors);
  }

  const sortResult = errors.length === 0
    ? topologicalSort(definition.nodes.map((node) => node.id), definition.edges)
    : { sorted: [] as string[], cyclePath: undefined as string[] | undefined };
  if (errors.length === 0 && sortResult.sorted.length !== definition.nodes.length) {
    errors.push("Workflow graph contains a cycle.");
  }

  const subWorkflowErrors = validateSubWorkflowReferences(definition, options.definitionsById);
  errors.push(...subWorkflowErrors);

  if (errors.length === 0) {
    const rootCount = definition.nodes.filter((node) =>
      !definition.edges.some((edge) => edge.targetNodeId === node.id),
    ).length;
    if (rootCount > definition.maxParallelNodes) {
      warnings.push(`Workflow has ${rootCount} root nodes but maxParallelNodes is ${definition.maxParallelNodes}.`);
    }
    if (definition.nodes.some((node) => node.type === "join") && definition.edges.length === 0) {
      warnings.push("Workflow contains a join node but no edges.");
    }
  }

  return {
    valid: errors.length === 0,
    errors,
    warnings,
    topologicalNodeIds: sortResult.sorted,
    cyclePath: sortResult.cyclePath,
  };
}

export function getUpstreamNodeIds(definition: AgentWorkflowDefinition, nodeId: string): string[] {
  return definition.edges
    .filter((edge) => edge.targetNodeId === nodeId)
    .map((edge) => edge.sourceNodeId);
}

export function getDownstreamNodeIds(definition: AgentWorkflowDefinition, nodeId: string): string[] {
  return definition.edges
    .filter((edge) => edge.sourceNodeId === nodeId)
    .map((edge) => edge.targetNodeId);
}

export function getIncomingEdges(definition: AgentWorkflowDefinition, nodeId: string): AgentWorkflowEdge[] {
  return definition.edges.filter((edge) => edge.targetNodeId === nodeId);
}

export function getOutgoingEdges(definition: AgentWorkflowDefinition, nodeId: string): AgentWorkflowEdge[] {
  return definition.edges.filter((edge) => edge.sourceNodeId === nodeId);
}

export function evaluateWorkflowEdgeCondition(
  edge: AgentWorkflowEdge,
  sourceNodeRun: AgentWorkflowNodeRun,
): { activated: boolean; reason: string } {
  const condition = edge.condition ?? { sourceStatus: "completed" as const };
  const sourceStatus = sourceNodeRun.status;

  if (condition.sourceStatus !== "always" && condition.sourceStatus !== sourceStatus) {
    return {
      activated: false,
      reason: `source_status_${sourceStatus}_did_not_match_${condition.sourceStatus}`,
    };
  }

  if (condition.branchId) {
    const branch = readPath(sourceNodeRun.outputJson, "branch");
    if (branch !== condition.branchId) {
      return {
        activated: false,
        reason: `branch_${String(branch ?? "none")}_did_not_match_${condition.branchId}`,
      };
    }
  }

  if (!condition.outputPredicate) {
    return {
      activated: true,
      reason: condition.branchId
        ? `source_status_${sourceStatus}_matched_branch_${condition.branchId}`
        : `source_status_${sourceStatus}_matched`,
    };
  }

  const predicateResult = evaluateOutputPredicate(sourceNodeRun.outputJson, condition.outputPredicate);
  return predicateResult;
}

export function evaluateOutputPredicate(
  outputJson: unknown,
  predicate: WorkflowOutputPredicate,
): { activated: boolean; reason: string } {
  const value = readPath(outputJson, predicate.path);
  let activated = false;

  switch (predicate.operator) {
    case "exists":
      activated = value !== undefined;
      break;
    case "not_exists":
      activated = value === undefined;
      break;
    case "equals":
      activated = deepEqual(value, predicate.value);
      break;
    case "not_equals":
      activated = !deepEqual(value, predicate.value);
      break;
    case "contains":
      activated = containsValue(value, predicate.value);
      break;
    case "not_contains":
      activated = !containsValue(value, predicate.value);
      break;
    case "starts_with":
      activated = typeof value === "string" && typeof predicate.value === "string" && value.startsWith(predicate.value);
      break;
    case "ends_with":
      activated = typeof value === "string" && typeof predicate.value === "string" && value.endsWith(predicate.value);
      break;
    case "is_empty":
      activated = isEmptyValue(value);
      break;
    case "is_not_empty":
      activated = !isEmptyValue(value);
      break;
    case "greater_than":
      activated = typeof value === "number" && typeof predicate.value === "number" && value > predicate.value;
      break;
    case "less_than":
      activated = typeof value === "number" && typeof predicate.value === "number" && value < predicate.value;
      break;
    case "in":
      activated = Array.isArray(predicate.value) && predicate.value.some((candidate) => deepEqual(candidate, value));
      break;
  }

  return {
    activated,
    reason: activated
      ? `predicate_${predicate.operator}_matched_${predicate.path}`
      : `predicate_${predicate.operator}_not_matched_${predicate.path}`,
  };
}

export function readPath(source: unknown, path: string | undefined): unknown {
  if (!path) return source;
  const parts = path.split(".").filter(Boolean);
  let current = source;
  for (const part of parts) {
    if (current === null || current === undefined) return undefined;
    if (Array.isArray(current)) {
      const index = Number(part);
      if (!Number.isInteger(index) || index < 0) return undefined;
      current = current[index];
    } else if (typeof current === "object") {
      current = (current as Record<string, unknown>)[part];
    } else {
      return undefined;
    }
  }
  return current;
}

function validateNode(
  node: AgentWorkflowNode,
  definition: AgentWorkflowDefinition,
  errors: string[],
  warnings: string[],
): void {
  if (!node.id) {
    errors.push("Every node must have an id.");
    return;
  }
  if (!node.title?.trim()) {
    errors.push(`Node ${node.id} must have a title.`);
  }
  if (!SUPPORTED_NODE_TYPES.has(node.type)) {
    errors.push(`Node ${node.id} has unsupported type ${String(node.type)}.`);
  }
  if (!["join", "if_else", "loop"].includes(node.type) && !node.instructions?.trim()) {
    errors.push(`Node ${node.title || node.id} must have instructions.`);
  }
  if (node.type === "join" && node.instructions?.trim()) {
    warnings.push(`Join node ${node.title || node.id} ignores instructions.`);
  }
  if (node.type === "approval_gate") {
    const outEdges = definition.edges.filter((edge) => edge.sourceNodeId === node.id);
    const hasApproved = outEdges.some((edge) => edge.condition?.branchId === "approved");
    const hasRejected = outEdges.some((edge) => edge.condition?.branchId === "rejected");
    if (!hasApproved) {
      warnings.push(`Approval gate node ${node.title || node.id} has no edge for the "approved" branch.`);
    }
    if (!hasRejected) {
      warnings.push(`Approval gate node ${node.title || node.id} has no edge for the "rejected" branch.`);
    }
  }
  if (node.type === "human_review") {
    if (node.humanReview?.reviewTargetNodeId) {
      const targetExists = definition.nodes.some((n) => n.id === node.humanReview!.reviewTargetNodeId);
      if (!targetExists) {
        errors.push(`Human review node ${node.title || node.id} reviewTargetNodeId "${node.humanReview.reviewTargetNodeId}" does not reference an existing node.`);
      }
    }
    const hasIncomingAgent = definition.edges
      .filter((edge) => edge.targetNodeId === node.id)
      .some((edge) => {
        const sourceNode = definition.nodes.find((n) => n.id === edge.sourceNodeId);
        return sourceNode?.type === "agent_task";
      });
    if (!hasIncomingAgent && !node.humanReview?.reviewTargetNodeId) {
      warnings.push(`Human review node ${node.title || node.id} has no incoming agent_task and no explicit reviewTargetNodeId.`);
    }
  }

    if (node.type === "if_else") {
    if (node.ifElse?.branches?.length) {
      validateIfElseBranches(node, errors, warnings);
    } else {
      validateNodeCondition(node.ifElse?.condition, `If/else node ${node.title || node.id}`, errors);
    }
  }
  if (node.type === "loop") {
    if (!Number.isInteger(node.loop?.maxIterations) || (node.loop?.maxIterations ?? 0) < 1) {
      errors.push(`Loop node ${node.title || node.id} maxIterations must be at least 1.`);
    }
    if ((node.loop?.maxIterations ?? 0) > 50) {
      errors.push(`Loop node ${node.title || node.id} maxIterations cannot exceed 50.`);
    }
    if (node.loop?.continueCondition) {
      validateNodeCondition(node.loop.continueCondition, `Loop node ${node.title || node.id} continue condition`, errors);
    }
    if (node.loop?.terminateCondition) {
      validateConditionGroup(node.loop.terminateCondition, `Loop node ${node.title || node.id} terminate condition`, errors);
    }
    if (node.loop?.initialState !== undefined && !isPlainObject(node.loop.initialState)) {
      errors.push(`Loop node ${node.title || node.id} initialState must be an object.`);
    }
    if (node.loop?.stateMapping !== undefined && !isPlainObject(node.loop.stateMapping)) {
      errors.push(`Loop node ${node.title || node.id} stateMapping must be an object.`);
    }
  }
  if (node.type === "sub_workflow" || node.type === "loop") {
    if (!node.subWorkflowDefinitionId?.trim()) {
      errors.push(`${node.type === "loop" ? "Loop" : "Sub-workflow"} node ${node.title || node.id} must reference subWorkflowDefinitionId.`);
    } else if (node.subWorkflowDefinitionId === definition.id) {
      errors.push(`${node.type === "loop" ? "Loop" : "Sub-workflow"} node ${node.title || node.id} cannot reference its own workflow.`);
    }
  }
  if (node.retryPolicy && (!Number.isInteger(node.retryPolicy.maxAttempts) || node.retryPolicy.maxAttempts < 1)) {
    errors.push(`Node ${node.title || node.id} retry maxAttempts must be at least 1.`);
  }
  if (node.outputSchema !== undefined && !isPlainObject(node.outputSchema)) {
    errors.push(`Node ${node.title || node.id} outputSchema must be an object.`);
  }
  if (node.stateUpdates !== undefined) {
    if (!isPlainObject(node.stateUpdates)) {
      errors.push(`Node ${node.title || node.id} stateUpdates must be an object.`);
    } else {
      for (const path of Object.keys(node.stateUpdates)) {
        if (!isValidStatePath(path)) {
          errors.push(`Node ${node.title || node.id} state update path "${path}" is invalid.`);
        }
      }
    }
  }
  if (node.inputMapping !== undefined && !isPlainObject(node.inputMapping)) {
    errors.push(`Node ${node.title || node.id} inputMapping must be an object.`);
  }
  if (node.outputMapping !== undefined && !isPlainObject(node.outputMapping)) {
    errors.push(`Node ${node.title || node.id} outputMapping must be an object.`);
  }
}

function validateIfElseBranches(
  node: AgentWorkflowNode,
  errors: string[],
  warnings: string[],
): void {
  const branches = node.ifElse?.branches ?? [];
  const label = `If/else node ${node.title || node.id}`;
  const ids = new Set<string>();
  let elseCount = 0;

  branches.forEach((branch, index) => {
    if (!branch.id?.trim()) {
      errors.push(`${label} branch ${index + 1} requires an id.`);
    } else if (ids.has(branch.id)) {
      errors.push(`${label} has duplicate branch id ${branch.id}.`);
    }
    ids.add(branch.id);

    if (!["if", "elif", "else"].includes(branch.kind)) {
      errors.push(`${label} branch ${branch.label || branch.id || index + 1} has unsupported kind ${String(branch.kind)}.`);
    }
    if (index === 0 && branch.kind !== "if") {
      errors.push(`${label} first branch must be IF.`);
    }
    if (branch.kind === "if" && index !== 0) {
      errors.push(`${label} can only have one IF branch and it must be first.`);
    }
    if (branch.kind === "else") {
      elseCount += 1;
      if (index !== branches.length - 1) {
        errors.push(`${label} ELSE branch must be last.`);
      }
      return;
    }
    if (branch.celExpression?.trim()) {
      const syntaxError = validateCelExpressionSyntax(branch.celExpression);
      if (syntaxError) {
        errors.push(`${label} branch ${branch.label || branch.id} CEL expression is invalid: ${syntaxError}`);
      }
    } else if (branch.condition) {
      validateConditionGroup(branch.condition, `${label} branch ${branch.label || branch.id}`, errors);
      warnings.push(`${label} branch ${branch.label || branch.id} uses a legacy condition. Convert it to CEL.`);
    } else {
      errors.push(`${label} branch ${branch.label || branch.id} requires a CEL expression.`);
    }
  });

  if (elseCount > 1) {
    errors.push(`${label} can only have one ELSE branch.`);
  }
  if (elseCount === 0) {
    warnings.push(`${label} has no ELSE branch; unmatched input will use the last branch as fallback.`);
  }
}

function validateNodeCondition(
  condition: WorkflowNodeCondition | undefined,
  label: string,
  errors: string[],
): void {
  if (!condition) {
    errors.push(`${label} requires a condition.`);
    return;
  }
  if (!condition.left || typeof condition.left !== "object") {
    errors.push(`${label} left value is required.`);
  }
  if (!SUPPORTED_OPERATORS.has(condition.operator)) {
    errors.push(`${label} has unsupported operator ${String(condition.operator)}.`);
  }
  if (condition.operator === "in" && !Array.isArray(condition.value) && !condition.right) {
    errors.push(`${label} operator "in" requires an array value or right value.`);
  }
}

function validateConditionGroup(
  group: WorkflowConditionGroup | undefined,
  label: string,
  errors: string[],
): void {
  if (!group) {
    errors.push(`${label} requires a condition group.`);
    return;
  }
  if (group.logic !== "and" && group.logic !== "or") {
    errors.push(`${label} condition group logic must be "and" or "or".`);
  }
  if (!Array.isArray(group.conditions) || group.conditions.length === 0) {
    errors.push(`${label} condition group requires at least one condition.`);
    return;
  }
  group.conditions.forEach((condition, index) => {
    validateNodeCondition(condition, `${label} condition ${index + 1}`, errors);
  });
}

function validateCondition(
  condition: WorkflowEdgeCondition | undefined,
  edgeId: string,
  sourceNode: AgentWorkflowNode | undefined,
  errors: string[],
): void {
  if (!condition) return;
  if (!SUPPORTED_SOURCE_STATUSES.has(condition.sourceStatus)) {
    errors.push(`Edge ${edgeId} has unsupported sourceStatus ${String(condition.sourceStatus)}.`);
  }
  if (condition.outputPredicate) {
    if (!condition.outputPredicate.path || typeof condition.outputPredicate.path !== "string") {
      errors.push(`Edge ${edgeId} output predicate path is required.`);
    }
    if (!SUPPORTED_OPERATORS.has(condition.outputPredicate.operator)) {
      errors.push(`Edge ${edgeId} has unsupported output predicate operator ${String(condition.outputPredicate.operator)}.`);
    }
    if (condition.outputPredicate.operator === "in" && !Array.isArray(condition.outputPredicate.value)) {
      errors.push(`Edge ${edgeId} operator "in" requires an array value.`);
    }
  }
  if (condition.branchId !== undefined) {
    if (!condition.branchId.trim()) {
      errors.push(`Edge ${edgeId} branchId is required when provided.`);
    } else if (sourceNode?.type !== "if_else" && sourceNode?.type !== "approval_gate") {
      errors.push(`Edge ${edgeId} branchId can only be used on edges from if/else or approval_gate nodes.`);
    } else if (sourceNode?.type === "approval_gate") {
      if (!["approved", "rejected"].includes(condition.branchId)) {
        errors.push(`Edge ${edgeId} approval_gate branchId must be "approved" or "rejected", got "${condition.branchId}".`);
      }
    } else if (!ifElseBranchIds(sourceNode).has(condition.branchId)) {
      errors.push(`Edge ${edgeId} references missing if/else branch ${condition.branchId}.`);
    }
  }
}

function ifElseBranchIds(node: AgentWorkflowNode): Set<string> {
  const branches = node.ifElse?.branches;
  if (branches?.length) return new Set(branches.map((branch) => branch.id).filter(Boolean));
  return new Set(["true", "false"]);
}

function isValidStatePath(path: string): boolean {
  if (!path.trim()) return false;
  const parts = path.split(".");
  return parts.every((part) =>
    /^[A-Za-z_][A-Za-z0-9_]*$/.test(part)
    && part !== "__proto__"
    && part !== "prototype"
    && part !== "constructor",
  );
}

function validateSubWorkflowReferences(
  definition: AgentWorkflowDefinition,
  definitionsById: Map<string, AgentWorkflowDefinition> | undefined,
): string[] {
  const errors: string[] = [];
  if (!definitionsById) return errors;

  for (const node of definition.nodes) {
    if ((node.type !== "sub_workflow" && node.type !== "loop") || !node.subWorkflowDefinitionId) continue;
    const child = definitionsById.get(node.subWorkflowDefinitionId);
    if (!child) {
      errors.push(`${node.type === "loop" ? "Loop" : "Sub-workflow"} node ${node.title || node.id} references missing workflow ${node.subWorkflowDefinitionId}.`);
      continue;
    }
    if (hasTransitiveSubWorkflowReference(child.id, definition.id, definitionsById, new Set([definition.id]))) {
      errors.push(`${node.type === "loop" ? "Loop" : "Sub-workflow"} node ${node.title || node.id} creates a recursive workflow reference.`);
    }
  }

  return errors;
}

function hasTransitiveSubWorkflowReference(
  currentId: string,
  targetId: string,
  definitionsById: Map<string, AgentWorkflowDefinition>,
  seen: Set<string>,
): boolean {
  if (currentId === targetId) return true;
  if (seen.has(currentId)) return false;
  seen.add(currentId);
  const current = definitionsById.get(currentId);
  if (!current) return false;
  for (const node of current.nodes) {
    if ((node.type !== "sub_workflow" && node.type !== "loop") || !node.subWorkflowDefinitionId) continue;
    if (hasTransitiveSubWorkflowReference(node.subWorkflowDefinitionId, targetId, definitionsById, seen)) {
      return true;
    }
  }
  return false;
}

function topologicalSort(
  nodeIds: string[],
  edges: AgentWorkflowEdge[],
): { sorted: string[]; cyclePath?: string[] } {
  const inDegree = new Map<string, number>();
  const outgoing = new Map<string, string[]>();

  for (const nodeId of nodeIds) {
    inDegree.set(nodeId, 0);
    outgoing.set(nodeId, []);
  }

  for (const edge of edges) {
    outgoing.get(edge.sourceNodeId)?.push(edge.targetNodeId);
    inDegree.set(edge.targetNodeId, (inDegree.get(edge.targetNodeId) ?? 0) + 1);
  }

  const queue = nodeIds.filter((nodeId) => (inDegree.get(nodeId) ?? 0) === 0);
  const sorted: string[] = [];

  while (queue.length > 0) {
    const nodeId = queue.shift();
    if (!nodeId) continue;
    sorted.push(nodeId);

    for (const targetId of outgoing.get(nodeId) ?? []) {
      const nextDegree = (inDegree.get(targetId) ?? 0) - 1;
      inDegree.set(targetId, nextDegree);
      if (nextDegree === 0) queue.push(targetId);
    }
  }

  if (sorted.length === nodeIds.length) {
    return { sorted };
  }

  return { sorted, cyclePath: findCyclePath(nodeIds, outgoing) };
}

function findCyclePath(nodeIds: string[], outgoing: Map<string, string[]>): string[] | undefined {
  const visiting = new Set<string>();
  const visited = new Set<string>();
  const stack: string[] = [];

  function visit(nodeId: string): string[] | undefined {
    if (visiting.has(nodeId)) {
      const start = stack.indexOf(nodeId);
      return start >= 0 ? [...stack.slice(start), nodeId] : [nodeId];
    }
    if (visited.has(nodeId)) return undefined;
    visiting.add(nodeId);
    stack.push(nodeId);
    for (const next of outgoing.get(nodeId) ?? []) {
      const cycle = visit(next);
      if (cycle) return cycle;
    }
    stack.pop();
    visiting.delete(nodeId);
    visited.add(nodeId);
    return undefined;
  }

  for (const nodeId of nodeIds) {
    const cycle = visit(nodeId);
    if (cycle) return cycle;
  }
  return undefined;
}

function containsValue(value: unknown, expected: unknown): boolean {
  if (typeof value === "string") return typeof expected === "string" && value.includes(expected);
  if (Array.isArray(value)) return value.some((item) => deepEqual(item, expected));
  if (isPlainObject(value)) return typeof expected === "string" && Object.prototype.hasOwnProperty.call(value, expected);
  return false;
}

function isEmptyValue(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (typeof value === "string") return value.trim().length === 0;
  if (Array.isArray(value)) return value.length === 0;
  if (isPlainObject(value)) return Object.keys(value).length === 0;
  return false;
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
