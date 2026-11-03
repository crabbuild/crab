/**
 * Branch Auto-Rename for the Crab Desktop Orchestrator.
 *
 * After an agent starts working on a task, this module generates a
 * descriptive branch name from the task prompt and offers to rename
 * the generic `agent/<uuid>` branch to something meaningful like
 * `feat/async-push-pipeline`.
 */

import { EventEmitter } from "node:events";
import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";

import type { Task } from "./orchestrator";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export type BranchRenameMode = "ask" | "auto" | "never";

export interface BranchRenameConfig {
  mode: BranchRenameMode;
  maxLength: number;
}

export interface BranchRenameSuggestion {
  taskId: string;
  oldName: string;
  suggestedName: string;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const DEFAULT_CONFIG: BranchRenameConfig = {
  mode: "ask",
  maxLength: 50,
};

/** Keywords that indicate a fix-type branch. */
const FIX_KEYWORDS = [
  "fix", "bug", "patch", "repair", "resolve", "hotfix", "error",
  "crash", "broken", "issue", "failing", "incorrect",
];

/** Keywords that indicate a refactor-type branch. */
const REFACTOR_KEYWORDS = [
  "refactor", "restructure", "reorganize", "cleanup", "clean up",
  "simplify", "extract", "rename", "move", "split", "consolidate",
  "optimize", "improve performance",
];

/** Common stop words to filter out of branch names. */
const STOP_WORDS = new Set([
  "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
  "have", "has", "had", "do", "does", "did", "will", "would", "could",
  "should", "may", "might", "shall", "can", "need", "must", "to", "of",
  "in", "for", "on", "with", "at", "by", "from", "as", "into", "through",
  "during", "before", "after", "above", "below", "between", "and", "but",
  "or", "nor", "not", "so", "yet", "both", "either", "neither", "each",
  "every", "all", "any", "few", "more", "most", "other", "some", "such",
  "no", "only", "own", "same", "than", "too", "very", "just", "because",
  "if", "when", "while", "where", "how", "what", "which", "who", "whom",
  "this", "that", "these", "those", "it", "its", "my", "your", "his",
  "her", "our", "their", "me", "him", "us", "them", "i", "you", "he",
  "she", "we", "they", "please", "add", "implement", "create", "make",
  "update", "change", "modify",
]);

// ─── BranchRenamer ───────────────────────────────────────────────────────────

/**
 * Generates descriptive branch names from task prompts and renames
 * the workspace branch accordingly.
 *
 * Events emitted:
 * - `branch-rename-suggestion` — { taskId, oldName, suggestedName }
 */
export class BranchRenamer extends EventEmitter {
  private configs = new Map<string, BranchRenameConfig>();

  // ─── Configuration ─────────────────────────────────────────────────────

  /** Sets the branch rename configuration for a repository. */
  setConfig(repoPath: string, config: Partial<BranchRenameConfig>): void {
    const existing = this.configs.get(repoPath) ?? { ...DEFAULT_CONFIG };
    this.configs.set(repoPath, { ...existing, ...config });
  }

  /** Gets the branch rename configuration for a repository. */
  getConfig(repoPath: string): BranchRenameConfig {
    return this.configs.get(repoPath) ?? { ...DEFAULT_CONFIG };
  }

  // ─── Core API ──────────────────────────────────────────────────────────

  /**
   * Generates a branch name suggestion from the task prompt.
   *
   * Uses a simple heuristic: extract keywords from the prompt, determine
   * a prefix based on content (feat/, fix/, refactor/), and produce a
   * kebab-case name capped at the configured max length.
   */
  async suggestBranchName(task: Task): Promise<string> {
    const config = this.getConfig(task.repoPath);
    const prompt = task.prompt.toLowerCase();

    const prefix = this.determinePrefix(prompt);
    const keywords = this.extractKeywords(prompt);
    const slug = this.toKebabCase(keywords);

    // Combine prefix + slug, respecting max length.
    const maxSlugLength = config.maxLength - prefix.length;
    const trimmedSlug = slug.slice(0, maxSlugLength);

    // Remove trailing hyphen if the slug was truncated mid-word.
    const cleanSlug = trimmedSlug.replace(/-$/, "");

    return `${prefix}${cleanSlug}`;
  }

  /**
   * Renames the branch for a task's worktree.
   *
   * Executes `git branch -m <old> <new>` and updates the task metadata.
   */
  async renameBranch(task: Task, newName: string): Promise<void> {
    const oldBranch = task.branch;

    await execFile("git", ["branch", "-m", oldBranch, newName], {
      cwd: task.worktreePath,
    });

    task.branch = newName;
  }

  /**
   * Triggers the branch rename suggestion flow for a task.
   *
   * Based on the configured mode:
   * - "auto": renames immediately without asking
   * - "ask": emits a suggestion event for the UI to present
   * - "never": does nothing
   */
  async onFirstEdit(task: Task): Promise<void> {
    const config = this.getConfig(task.repoPath);

    if (config.mode === "never") return;

    const suggestedName = await this.suggestBranchName(task);

    if (config.mode === "auto") {
      await this.renameBranch(task, suggestedName);
      this.emit("branch-rename-suggestion", {
        taskId: task.id,
        oldName: task.branch,
        suggestedName,
      } satisfies BranchRenameSuggestion);
      return;
    }

    // mode === "ask"
    this.emit("branch-rename-suggestion", {
      taskId: task.id,
      oldName: task.branch,
      suggestedName,
    } satisfies BranchRenameSuggestion);
  }

  // ─── Internal Helpers ──────────────────────────────────────────────────

  /**
   * Determines the branch prefix based on prompt content.
   * Scans for fix/refactor keywords; defaults to "feat/".
   */
  private determinePrefix(prompt: string): string {
    if (FIX_KEYWORDS.some((kw) => prompt.includes(kw))) {
      return "fix/";
    }
    if (REFACTOR_KEYWORDS.some((kw) => prompt.includes(kw))) {
      return "refactor/";
    }
    return "feat/";
  }

  /**
   * Extracts meaningful keywords from a prompt string.
   * Filters out stop words and short tokens.
   */
  private extractKeywords(prompt: string): string[] {
    // Remove punctuation and split on whitespace.
    const tokens = prompt
      .replace(/[^a-z0-9\s-]/g, " ")
      .split(/\s+/)
      .filter((t) => t.length > 2)
      .filter((t) => !STOP_WORDS.has(t));

    // Take the first 6 meaningful tokens to keep the name concise.
    return tokens.slice(0, 6);
  }

  /**
   * Converts an array of keywords to a kebab-case string.
   */
  private toKebabCase(keywords: string[]): string {
    return keywords.join("-").replace(/--+/g, "-");
  }
}
