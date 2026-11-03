/**
 * Pull Latest & Rebase + Conflict Resolution for the Crab Desktop Orchestrator.
 *
 * Keeps workspace branches up-to-date with their base branch via rebase.
 * When conflicts arise, builds context from conflict markers and sends
 * them to the agent for resolution. Validates that no conflict markers
 * remain after resolution.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as path from "node:path";

import type { Orchestrator, Task } from "./orchestrator";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface RebaseResult {
  success: boolean;
  conflicts: string[];
}

export interface ConflictContext {
  file: string;
  markers: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────

/** Regex matching git conflict markers. */
const CONFLICT_MARKER_RE = /^(<{7}|={7}|>{7})/m;

/** Maximum bytes to read from a single conflicted file for context. */
const MAX_CONFLICT_FILE_SIZE = 50_000;

// ─── WorkspaceRebase ─────────────────────────────────────────────────────────

/**
 * Manages rebasing workspace branches onto the latest base branch.
 * Detects conflicts, builds context for agent-assisted resolution,
 * and validates that resolutions are clean.
 *
 * Events emitted:
 * - `rebase:success` — { taskId }
 * - `rebase:conflicts` — { taskId, conflicts: string[] }
 * - `rebase:resolution-valid` — { taskId }
 * - `rebase:resolution-invalid` — { taskId, filesWithMarkers: string[] }
 */
export class WorkspaceRebase extends EventEmitter {
  private orchestrator: Orchestrator;

  constructor(orchestrator: Orchestrator) {
    super();
    this.orchestrator = orchestrator;
  }

  // ─── Core API ──────────────────────────────────────────────────────────

  /**
   * Fetches the latest base branch and rebases the workspace onto it.
   *
   * Returns a `RebaseResult` indicating success or listing conflicted files.
   */
  async pullLatest(task: Task): Promise<RebaseResult> {
    const baseBranch = this.getBaseBranch(task);

    // Fetch the latest from origin.
    await execFile("git", ["fetch", "origin", baseBranch], {
      cwd: task.worktreePath,
    });

    // Attempt the rebase.
    try {
      await execFile("git", ["rebase", `origin/${baseBranch}`], {
        cwd: task.worktreePath,
      });

      this.emit("rebase:success", { taskId: task.id });
      return { success: true, conflicts: [] };
    } catch {
      // Rebase failed — likely due to conflicts.
      const conflicts = await this.getConflictFiles(task.worktreePath);

      if (conflicts.length > 0) {
        this.emit("rebase:conflicts", { taskId: task.id, conflicts });
        return { success: false, conflicts };
      }

      // Non-conflict failure (e.g., dirty worktree). Abort and report.
      try {
        await execFile("git", ["rebase", "--abort"], {
          cwd: task.worktreePath,
        });
      } catch {
        // Best-effort abort.
      }

      return { success: false, conflicts: [] };
    }
  }

  /**
   * Returns the number of commits the workspace is behind the base branch.
   *
   * Requires a prior `git fetch` to have up-to-date remote refs.
   */
  async getCommitsBehind(task: Task): Promise<number> {
    const baseBranch = this.getBaseBranch(task);

    try {
      // Fetch to ensure we have the latest remote refs.
      await execFile("git", ["fetch", "origin", baseBranch], {
        cwd: task.worktreePath,
      });

      const { stdout } = await execFile(
        "git",
        ["rev-list", "--count", `HEAD..origin/${baseBranch}`],
        { cwd: task.worktreePath },
      );

      return parseInt(stdout.trim(), 10) || 0;
    } catch {
      return 0;
    }
  }

  /**
   * Sends conflict context to the agent for resolution.
   *
   * Builds a prompt containing the conflict markers and surrounding
   * context for each conflicted file, then iterates the agent.
   */
  async resolveWithAgent(task: Task, conflicts: string[]): Promise<void> {
    const conflictContext = await this.buildConflictContext(task, conflicts);

    const resolvePrompt = [
      "Merge conflicts need resolution. For each conflicted file below,",
      "choose the correct resolution that preserves both sides' intent.",
      "Remove all conflict markers (<<<<<<, ======, >>>>>>) and produce",
      "the final merged content.",
      "",
      conflictContext,
      "",
      "After resolving all conflicts, stage the files with `git add`",
      "and run `git rebase --continue`.",
    ].join("\n");

    await this.orchestrator.iterate(task.id, resolvePrompt);
  }

  // ─── Conflict Detection ────────────────────────────────────────────────

  /**
   * Parses the list of conflicted files from git's unmerged state.
   *
   * Uses `git diff --name-only --diff-filter=U` to find files with
   * unresolved conflicts.
   */
  async getConflictFiles(worktreePath: string): Promise<string[]> {
    try {
      const { stdout } = await execFile(
        "git",
        ["diff", "--name-only", "--diff-filter=U"],
        { cwd: worktreePath },
      );

      return stdout
        .split("\n")
        .map((f) => f.trim())
        .filter(Boolean);
    } catch {
      return [];
    }
  }

  // ─── Conflict Context Building ─────────────────────────────────────────

  /**
   * Reads conflict markers from each conflicted file and builds a
   * context string suitable for sending to the agent.
   *
   * For each file, includes the file path and the full content with
   * conflict markers highlighted.
   */
  async buildConflictContext(
    task: Task,
    files: string[],
  ): Promise<string> {
    const sections: string[] = [];

    for (const file of files) {
      const filePath = path.join(task.worktreePath, file);

      try {
        const stat = await fs.promises.stat(filePath);

        // Skip files that are too large to include in a prompt.
        if (stat.size > MAX_CONFLICT_FILE_SIZE) {
          sections.push(
            `### ${file}\n\n(File too large to include — ${stat.size} bytes. Please resolve manually.)`,
          );
          continue;
        }

        const content = await fs.promises.readFile(filePath, "utf-8");
        const markers = this.extractConflictRegions(content);

        sections.push(
          `### ${file}\n\n\`\`\`\n${markers}\n\`\`\``,
        );
      } catch {
        sections.push(`### ${file}\n\n(Unable to read file.)"`);
      }
    }

    return sections.join("\n\n");
  }

  // ─── Conflict Resolution Validation ────────────────────────────────────

  /**
   * Validates that no conflict markers remain in the given files.
   *
   * Returns `true` if all files are clean (no markers), `false` otherwise.
   */
  async validateResolution(
    worktreePath: string,
    files: string[],
  ): Promise<boolean> {
    const filesWithMarkers: string[] = [];

    for (const file of files) {
      const filePath = path.join(worktreePath, file);

      try {
        const content = await fs.promises.readFile(filePath, "utf-8");
        if (CONFLICT_MARKER_RE.test(content)) {
          filesWithMarkers.push(file);
        }
      } catch {
        // File doesn't exist or can't be read — skip.
      }
    }

    if (filesWithMarkers.length > 0) {
      this.emit("rebase:resolution-invalid", { filesWithMarkers });
      return false;
    }

    this.emit("rebase:resolution-valid", {});
    return true;
  }

  // ─── Internal Helpers ──────────────────────────────────────────────────

  /**
   * Extracts conflict regions from file content.
   *
   * If the file contains conflict markers, returns the full content
   * (so the agent has surrounding context). If no markers are found,
   * returns the content as-is.
   */
  private extractConflictRegions(content: string): string {
    // Return the full file content so the agent has context around
    // the conflict markers to make informed resolution decisions.
    return content;
  }

  /**
   * Determines the base branch for a task.
   * Falls back to "main" if no baseRef is set.
   */
  private getBaseBranch(task: Task): string {
    return task.baseRef || "main";
  }
}
