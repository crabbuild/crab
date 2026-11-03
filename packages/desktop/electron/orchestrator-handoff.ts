/**
 * Workspace Handoff & Context Sharing for the Crab Desktop Orchestrator.
 *
 * When a dependent workspace starts (from a task graph), this module
 * builds context from the completed parent workspace and injects it
 * into the child workspace's agent prompt. This ensures continuity
 * across multi-workspace task decompositions.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as path from "node:path";

import type { Task } from "./orchestrator";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface HandoffContext {
  parentTaskId: string;
  parentBranch: string;
  parentPrompt: string;
  diffSummary: string;
  filesChanged: string[];
  contextNotes: string;
  createdAt: number;
}

export interface CrossWorkspaceReference {
  workspaceId: string;
  name: string;
  summary: string;
  branch: string;
  filesChanged: string[];
}

// ─── WorkspaceHandoff ────────────────────────────────────────────────────────

export class WorkspaceHandoff extends EventEmitter {
  /**
   * Builds a handoff context from a completed parent workspace to pass
   * to a child workspace. Summarises the parent's diff, extracts key
   * decisions, and includes any .context/ notes.
   */
  async buildHandoffContext(
    parentTask: Task,
    childTask: Task,
  ): Promise<HandoffContext> {
    // Get the diff summary from the parent workspace.
    const diffSummary = await this.summarizeDiff(parentTask);

    // Get the list of files changed.
    const filesChanged = await this.getChangedFiles(parentTask);

    // Read .context/ notes if they exist.
    const contextNotes = await this.readContextNotes(parentTask);

    const context: HandoffContext = {
      parentTaskId: parentTask.id,
      parentBranch: parentTask.branch,
      parentPrompt: parentTask.prompt,
      diffSummary,
      filesChanged,
      contextNotes,
      createdAt: Date.now(),
    };

    this.emit("orchestrator:handoff-ready", {
      childId: childTask.id,
      context,
    });

    return context;
  }

  /**
   * Injects handoff context into a child task's prompt. Prepends the
   * context as a structured section that the agent can reference.
   */
  injectHandoff(childTask: Task, context: HandoffContext): string {
    const sections: string[] = [
      "## Context from parent workspace",
      "",
      `Parent task: "${context.parentPrompt}"`,
      `Branch: ${context.parentBranch}`,
      "",
    ];

    if (context.diffSummary) {
      sections.push("### Summary of changes");
      sections.push(context.diffSummary);
      sections.push("");
    }

    if (context.filesChanged.length > 0) {
      sections.push("### Files changed");
      sections.push(
        context.filesChanged.map((f) => `- ${f}`).join("\n"),
      );
      sections.push("");
    }

    if (context.contextNotes) {
      sections.push("### Notes");
      sections.push(context.contextNotes);
      sections.push("");
    }

    const handoffText = sections.join("\n");

    // Prepend to the child task's prompt.
    const augmentedPrompt = `${handoffText}\n---\n\n${childTask.prompt}`;
    return augmentedPrompt;
  }

  /**
   * Builds a cross-workspace reference summary for @workspace mentions.
   * Returns a concise summary of another workspace's changes.
   */
  async buildCrossReference(task: Task): Promise<CrossWorkspaceReference> {
    const filesChanged = await this.getChangedFiles(task);
    const summary = await this.summarizeDiff(task);

    return {
      workspaceId: task.id,
      name: task.branch,
      summary,
      branch: task.branch,
      filesChanged,
    };
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  /**
   * Summarises the diff of a workspace by computing a stat summary
   * and extracting key changes.
   */
  private async summarizeDiff(task: Task): Promise<string> {
    try {
      // Get diff stat against the base ref.
      const { stdout: stat } = await execFile(
        "git",
        ["diff", "--stat", task.baseRef, "HEAD"],
        { cwd: task.worktreePath, maxBuffer: 256 * 1024 },
      );

      // Get a short diff summary (first 2000 chars of the patch).
      const { stdout: patch } = await execFile(
        "git",
        ["diff", "--shortstat", task.baseRef, "HEAD"],
        { cwd: task.worktreePath },
      );

      const lines: string[] = [];
      if (patch.trim()) {
        lines.push(patch.trim());
      }
      if (stat.trim()) {
        // Include only the summary line (last line of stat output).
        const statLines = stat.trim().split("\n");
        const summaryLine = statLines[statLines.length - 1];
        if (summaryLine) {
          lines.push(summaryLine.trim());
        }
      }

      return lines.join("\n") || "No changes detected.";
    } catch {
      return "Unable to compute diff summary.";
    }
  }

  /**
   * Gets the list of files changed in the workspace relative to the base ref.
   */
  private async getChangedFiles(task: Task): Promise<string[]> {
    try {
      const { stdout } = await execFile(
        "git",
        ["diff", "--name-only", task.baseRef, "HEAD"],
        { cwd: task.worktreePath },
      );
      return stdout.trim().split("\n").filter(Boolean);
    } catch {
      return [];
    }
  }

  /**
   * Reads .context/NOTES.md from the workspace if it exists.
   */
  private async readContextNotes(task: Task): Promise<string> {
    const notesPath = path.join(task.worktreePath, ".context", "NOTES.md");
    try {
      if (fs.existsSync(notesPath)) {
        return fs.readFileSync(notesPath, "utf-8");
      }
    } catch {
      // Best-effort.
    }
    return "";
  }
}
