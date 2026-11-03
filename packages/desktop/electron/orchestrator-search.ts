/**
 * Workspace Search for the Crab Desktop Orchestrator.
 *
 * Provides full-text search across workspace metadata: titles (prompt first
 * 100 chars), branch names, PR titles, and agent types. Supports filtering
 * by status, agent type, repository, and date range. Results are ranked by
 * match quality.
 */

import type { Task } from "./orchestrator";

// ─── Types ───────────────────────────────────────────────────────────────────

export interface SearchFilters {
  status?: string[];
  agentType?: string;
  repository?: string;
  dateRange?: { start: number; end: number };
}

export interface WorkspaceSearchResult {
  workspaceId: string;
  matchField: "title" | "prompt" | "branch" | "pr" | "chat";
  snippet: string;
  score: number;
}

interface IndexEntry {
  workspaceId: string;
  title: string;
  prompt: string;
  branch: string;
  prTitle: string;
  agentType: string;
  status: string;
  repoPath: string;
  createdAt: number;
}

// ─── Scoring Constants ───────────────────────────────────────────────────────

const SCORE_EXACT_TITLE = 100;
const SCORE_SUBSTRING_TITLE = 80;
const SCORE_BRANCH_MATCH = 60;
const SCORE_PROMPT_MATCH = 40;
const SCORE_PR_MATCH = 70;

// ─── WorkspaceSearch ─────────────────────────────────────────────────────────

export class WorkspaceSearch {
  private index: IndexEntry[] = [];

  /**
   * Builds a searchable index from the given task list.
   * Call this before `search()` to ensure results reflect current state.
   */
  buildIndex(tasks: Task[]): void {
    this.index = tasks.map((task) => ({
      workspaceId: task.id,
      title: task.prompt.slice(0, 100),
      prompt: task.prompt,
      branch: task.branch,
      prTitle: task.prUrl ?? "",
      agentType: task.agentType,
      status: task.status,
      repoPath: task.repoPath,
      createdAt: task.createdAt,
    }));
  }

  /**
   * Searches the index for workspaces matching the query string.
   * Returns results ranked by match quality (highest score first).
   *
   * Matching is case-insensitive substring search. Scoring:
   * - Exact match in title: 100
   * - Substring in title: 80
   * - Match in PR title/URL: 70
   * - Match in branch name: 60
   * - Match in prompt (beyond title): 40
   */
  search(query: string, filters?: SearchFilters): WorkspaceSearchResult[] {
    if (!query.trim()) {
      return [];
    }

    const normalizedQuery = query.toLowerCase();
    const results: WorkspaceSearchResult[] = [];

    for (const entry of this.index) {
      // Apply filters before scoring.
      if (!this.passesFilters(entry, filters)) {
        continue;
      }

      const matches = this.scoreEntry(entry, normalizedQuery);
      results.push(...matches);
    }

    // Sort by score descending, then by workspace ID for stability.
    results.sort((a, b) => {
      if (b.score !== a.score) return b.score - a.score;
      return a.workspaceId.localeCompare(b.workspaceId);
    });

    return results;
  }

  // ─── Private Helpers ─────────────────────────────────────────────────────

  private passesFilters(entry: IndexEntry, filters?: SearchFilters): boolean {
    if (!filters) return true;

    if (filters.status && filters.status.length > 0) {
      if (!filters.status.includes(entry.status)) {
        return false;
      }
    }

    if (filters.agentType) {
      if (entry.agentType !== filters.agentType) {
        return false;
      }
    }

    if (filters.repository) {
      if (entry.repoPath !== filters.repository) {
        return false;
      }
    }

    if (filters.dateRange) {
      if (
        entry.createdAt < filters.dateRange.start ||
        entry.createdAt > filters.dateRange.end
      ) {
        return false;
      }
    }

    return true;
  }

  private scoreEntry(
    entry: IndexEntry,
    normalizedQuery: string,
  ): WorkspaceSearchResult[] {
    const results: WorkspaceSearchResult[] = [];

    // Check title (first 100 chars of prompt).
    const titleLower = entry.title.toLowerCase();
    if (titleLower === normalizedQuery) {
      results.push({
        workspaceId: entry.workspaceId,
        matchField: "title",
        snippet: entry.title,
        score: SCORE_EXACT_TITLE,
      });
    } else if (titleLower.includes(normalizedQuery)) {
      results.push({
        workspaceId: entry.workspaceId,
        matchField: "title",
        snippet: this.extractSnippet(entry.title, normalizedQuery),
        score: SCORE_SUBSTRING_TITLE,
      });
    }

    // Check branch name.
    const branchLower = entry.branch.toLowerCase();
    if (branchLower.includes(normalizedQuery)) {
      results.push({
        workspaceId: entry.workspaceId,
        matchField: "branch",
        snippet: entry.branch,
        score: SCORE_BRANCH_MATCH,
      });
    }

    // Check PR title/URL.
    if (entry.prTitle) {
      const prLower = entry.prTitle.toLowerCase();
      if (prLower.includes(normalizedQuery)) {
        results.push({
          workspaceId: entry.workspaceId,
          matchField: "pr",
          snippet: entry.prTitle,
          score: SCORE_PR_MATCH,
        });
      }
    }

    // Check full prompt (beyond title match).
    if (results.length === 0 || !results.some((r) => r.matchField === "title")) {
      const promptLower = entry.prompt.toLowerCase();
      if (promptLower.includes(normalizedQuery)) {
        results.push({
          workspaceId: entry.workspaceId,
          matchField: "prompt",
          snippet: this.extractSnippet(entry.prompt, normalizedQuery),
          score: SCORE_PROMPT_MATCH,
        });
      }
    }

    // Deduplicate: keep only the highest-scoring result per workspace.
    if (results.length > 1) {
      results.sort((a, b) => b.score - a.score);
      return [results[0]];
    }

    return results;
  }

  /**
   * Extracts a snippet around the matched query within the text.
   * Returns up to 80 characters centered on the match.
   */
  private extractSnippet(text: string, normalizedQuery: string): string {
    const lowerText = text.toLowerCase();
    const matchIdx = lowerText.indexOf(normalizedQuery);
    if (matchIdx === -1) return text.slice(0, 80);

    const snippetRadius = 40;
    const start = Math.max(0, matchIdx - snippetRadius);
    const end = Math.min(text.length, matchIdx + normalizedQuery.length + snippetRadius);

    let snippet = text.slice(start, end);
    if (start > 0) snippet = "…" + snippet;
    if (end < text.length) snippet = snippet + "…";

    return snippet;
  }
}
