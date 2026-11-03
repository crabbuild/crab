import * as fs from "node:fs/promises";
import * as path from "node:path";
import { Liquid } from "liquidjs";
import { parse as parseYaml } from "yaml";

import type { SymphonyIssue, WorkflowDefinition } from "./types";

export class SymphonyWorkflowError extends Error {
  constructor(
    public readonly code: string,
    message: string,
    public readonly causeValue?: unknown,
  ) {
    super(message);
    this.name = "SymphonyWorkflowError";
  }
}

export interface PromptRenderInput {
  issue: SymphonyIssue;
  attempt: number | null;
}

const DEFAULT_PROMPT = "You are working on an issue from the configured tracker.";

const liquid = new Liquid({
  strictVariables: true,
  strictFilters: true,
});

export async function loadWorkflow(workflowPath: string): Promise<WorkflowDefinition> {
  const absolutePath = path.resolve(workflowPath);
  let content: string;
  let mtimeMs: number | null = null;

  try {
    const [raw, stat] = await Promise.all([
      fs.readFile(absolutePath, "utf-8"),
      fs.stat(absolutePath),
    ]);
    content = raw;
    mtimeMs = stat.mtimeMs;
  } catch (err) {
    throw new SymphonyWorkflowError(
      "missing_workflow_file",
      `Missing WORKFLOW.md at ${absolutePath}`,
      err,
    );
  }

  const parsed = parseWorkflowContent(content, absolutePath);
  return {
    ...parsed,
    workflowPath: absolutePath,
    workflowDir: path.dirname(absolutePath),
    loadedAtMs: Date.now(),
    mtimeMs,
  };
}

export function parseWorkflowContent(
  content: string,
  workflowPath = "WORKFLOW.md",
): Omit<WorkflowDefinition, "workflowPath" | "workflowDir" | "loadedAtMs" | "mtimeMs"> {
  const { frontMatter, body } = splitFrontMatter(content);
  let config: Record<string, unknown> = {};

  if (frontMatter.trim()) {
    let decoded: unknown;
    try {
      decoded = parseYaml(frontMatter);
    } catch (err) {
      throw new SymphonyWorkflowError(
        "workflow_parse_error",
        `Failed to parse workflow YAML in ${workflowPath}`,
        err,
      );
    }

    if (!isPlainObject(decoded)) {
      throw new SymphonyWorkflowError(
        "workflow_front_matter_not_a_map",
        `Workflow front matter must be a map in ${workflowPath}`,
      );
    }

    config = decoded;
  }

  return {
    config,
    promptTemplate: body.trim(),
  };
}

export async function renderPrompt(
  workflow: Pick<WorkflowDefinition, "promptTemplate">,
  input: PromptRenderInput,
): Promise<string> {
  const template = workflow.promptTemplate.trim() || DEFAULT_PROMPT;
  try {
    return await liquid.parseAndRender(template, {
      issue: liquidIssue(input.issue),
      attempt: input.attempt,
    });
  } catch (err) {
    throw new SymphonyWorkflowError(
      "template_render_error",
      "Failed to render workflow prompt",
      err,
    );
  }
}

function splitFrontMatter(content: string): { frontMatter: string; body: string } {
  const normalized = content.replace(/^\uFEFF/, "");
  const lines = normalized.split(/\r?\n/);

  if (lines[0]?.trim() !== "---") {
    return { frontMatter: "", body: normalized };
  }

  const closingIndex = lines.findIndex((line, index) => index > 0 && line.trim() === "---");
  if (closingIndex < 0) {
    return {
      frontMatter: lines.slice(1).join("\n"),
      body: "",
    };
  }

  return {
    frontMatter: lines.slice(1, closingIndex).join("\n"),
    body: lines.slice(closingIndex + 1).join("\n"),
  };
}

function liquidIssue(issue: SymphonyIssue): Record<string, unknown> {
  return {
    id: issue.id,
    identifier: issue.identifier,
    title: issue.title,
    description: issue.description,
    priority: issue.priority,
    state: issue.state,
    branch_name: issue.branchName,
    branchName: issue.branchName,
    url: issue.url,
    labels: issue.labels,
    blocked_by: issue.blockedBy,
    blockedBy: issue.blockedBy,
    assignee: issue.assignee,
    assignee_id: issue.assigneeId,
    assigneeId: issue.assigneeId,
    created_at: issue.createdAt,
    createdAt: issue.createdAt,
    updated_at: issue.updatedAt,
    updatedAt: issue.updatedAt,
  };
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
