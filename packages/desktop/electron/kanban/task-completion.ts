import type { KanbanCard, KanbanColumn, PipelineState } from "./kanban-types";

export interface TaskResult {
  exitCode: number;
  prUrl?: string;
  prNumber?: number;
  summary?: string;
  modifiedFiles?: string[];
  agentSummary?: string;
}

export interface CompletionUpdate {
  pipelineState: PipelineState;
  targetColumnId?: string;
  positionIndex?: number;
  linkedPrUrl?: string;
  linkedPrNumber?: number;
  descriptionAppend?: string;
}

const SUMMARY_MAX_LENGTH = 500;
const FILES_MAX_COUNT = 20;
const AGENT_SUMMARY_MAX_LENGTH = 2000;

/**
 * Finds a column by name using case-insensitive comparison.
 * Returns the first match if multiple columns share the same name.
 */
export function findColumnByName(
  columns: KanbanColumn[],
  name: string,
): KanbanColumn | undefined {
  const lower = name.toLowerCase();
  return columns.find((col) => col.name.toLowerCase() === lower);
}

/**
 * Formats the "## Agent Results" section to append to card description.
 * Truncates summary to 500 chars, limits files to 20.
 */
export function formatAgentResults(
  summary: string,
  modifiedFiles: string[],
): string {
  const truncatedSummary =
    summary.length > SUMMARY_MAX_LENGTH
      ? summary.slice(0, SUMMARY_MAX_LENGTH)
      : summary;

  const limitedFiles = modifiedFiles.slice(0, FILES_MAX_COUNT);

  let result = "## Agent Results\n\n";
  if (truncatedSummary) {
    result += truncatedSummary + "\n\n";
  }
  if (limitedFiles.length > 0) {
    result += "### Modified Files\n\n";
    result += limitedFiles.map((f) => `- ${f}`).join("\n");
    result += "\n";
  }

  return result;
}

/**
 * Formats the "## What was done" section from agentSummary.
 * Truncates to 2000 chars.
 */
export function formatAgentSummary(agentSummary: string): string {
  const truncated =
    agentSummary.length > AGENT_SUMMARY_MAX_LENGTH
      ? agentSummary.slice(0, AGENT_SUMMARY_MAX_LENGTH)
      : agentSummary;

  return `## What was done\n\n${truncated}\n`;
}

/**
 * Computes the card update from a task result.
 * Pure function — no side effects.
 *
 * - exit code 0 → pipelineState "done", target column "Done"
 * - non-zero exit code → pipelineState "blocked", target column "Blocked"
 * - If no matching column exists, card stays in current column
 * - PR URL and number passed through exactly
 * - positionIndex equals cardsInTargetColumn (appended at end)
 */
export function computeCompletionUpdate(
  result: TaskResult,
  _card: KanbanCard,
  columns: KanbanColumn[],
  cardsInTargetColumn: number,
): CompletionUpdate {
  const isSuccess = result.exitCode === 0;
  const pipelineState: PipelineState = isSuccess ? "done" : "blocked";
  const targetColumnName = isSuccess ? "Done" : "Blocked";

  const targetColumn = findColumnByName(columns, targetColumnName);

  const update: CompletionUpdate = {
    pipelineState,
  };

  if (targetColumn) {
    update.targetColumnId = targetColumn.id;
    update.positionIndex = cardsInTargetColumn;
  }

  if (result.prUrl) {
    update.linkedPrUrl = result.prUrl;
  }
  if (result.prNumber !== undefined) {
    update.linkedPrNumber = result.prNumber;
  }

  // Build description append
  const parts: string[] = [];

  const summary = result.summary ?? "";
  const modifiedFiles = result.modifiedFiles ?? [];

  if (summary || modifiedFiles.length > 0) {
    parts.push(formatAgentResults(summary, modifiedFiles));
  }

  if (result.agentSummary) {
    parts.push(formatAgentSummary(result.agentSummary));
  }

  if (parts.length > 0) {
    update.descriptionAppend = "\n\n" + parts.join("\n");
  }

  return update;
}
