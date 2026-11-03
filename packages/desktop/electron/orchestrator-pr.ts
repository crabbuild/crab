/**
 * PR Creation & Management for the Crab Desktop Orchestrator.
 *
 * Handles pushing workspace branches to remote, creating PRs via the
 * GitHub API, AI-drafted descriptions, PR template support, and
 * updating existing PRs.
 */

import { execFile as execFileCb } from "node:child_process";
import { promisify } from "node:util";
import * as fs from "node:fs";
import * as path from "node:path";

const execFile = promisify(execFileCb);

// ─── Types ───────────────────────────────────────────────────────────────────

export interface PRMetadata {
  url: string;
  number: number;
  title: string;
  body: string;
  state: "open" | "closed" | "merged";
  createdAt: string;
}

export interface PRCreateOptions {
  repoPath: string;
  branch: string;
  baseBranch?: string;
  title?: string;
  body?: string;
  draft?: boolean;
}

export interface PRUpdateOptions {
  repoPath: string;
  branch: string;
  prNumber: number;
  title?: string;
  body?: string;
}

// ─── Push Branch ─────────────────────────────────────────────────────────────

/**
 * Pushes the workspace branch to the remote origin.
 */
export async function pushBranch(repoPath: string, branch: string): Promise<{ success: boolean; error?: string }> {
  try {
    await execFile("git", ["push", "-u", "origin", branch], { cwd: repoPath });
    return { success: true };
  } catch (err) {
    const message = err instanceof Error ? (err as any).stderr ?? err.message : String(err);
    return { success: false, error: message };
  }
}

// ─── PR Template ─────────────────────────────────────────────────────────────

/**
 * Loads the PR template from `.github/pull_request_template.md` if it exists.
 */
export function loadPRTemplate(repoPath: string): string | null {
  const templatePaths = [
    path.join(repoPath, ".github", "pull_request_template.md"),
    path.join(repoPath, ".github", "PULL_REQUEST_TEMPLATE.md"),
    path.join(repoPath, "pull_request_template.md"),
    path.join(repoPath, "PULL_REQUEST_TEMPLATE.md"),
  ];

  for (const templatePath of templatePaths) {
    try {
      if (fs.existsSync(templatePath)) {
        return fs.readFileSync(templatePath, "utf-8");
      }
    } catch {
      // Continue to next path
    }
  }
  return null;
}

// ─── AI-Drafted PR Description ───────────────────────────────────────────────

/**
 * Builds a prompt for the AI to generate a PR title and body from the diff.
 */
export function buildPRDescriptionPrompt(
  diff: string,
  taskPrompt: string,
  template: string | null,
): string {
  const maxDiffLength = 10_000;
  const truncatedDiff =
    diff.length > maxDiffLength
      ? diff.slice(0, maxDiffLength) + "\n\n[... diff truncated ...]"
      : diff;

  const parts = [
    "Generate a pull request title and description based on the following changes.",
    "Return your response as JSON: { \"title\": \"...\", \"body\": \"...\" }",
    "",
    `Task description: ${taskPrompt}`,
    "",
  ];

  if (template) {
    parts.push("Use this PR template as the base structure for the body:");
    parts.push("```");
    parts.push(template);
    parts.push("```");
    parts.push("");
  }

  parts.push("Diff:");
  parts.push("```");
  parts.push(truncatedDiff);
  parts.push("```");

  return parts.join("\n");
}

// ─── Create PR via GitHub CLI ────────────────────────────────────────────────

/**
 * Creates a PR using the `gh` CLI (GitHub CLI). Falls back to error if
 * `gh` is not available.
 */
export async function createPR(options: PRCreateOptions): Promise<{ success: boolean; pr?: PRMetadata; error?: string }> {
  const { repoPath, branch, baseBranch, title, body, draft } = options;

  try {
    // First push the branch
    const pushResult = await pushBranch(repoPath, branch);
    if (!pushResult.success) {
      return { success: false, error: `Push failed: ${pushResult.error}` };
    }

    // Create PR via gh CLI
    const args = ["pr", "create", "--head", branch];
    if (baseBranch) args.push("--base", baseBranch);
    if (title) args.push("--title", title);
    if (body) args.push("--body", body);
    if (draft) args.push("--draft");

    const { stdout } = await execFile("gh", args, { cwd: repoPath });
    const prUrl = stdout.trim();

    // Extract PR number from URL
    const prNumberMatch = prUrl.match(/\/pull\/(\d+)/);
    const prNumber = prNumberMatch ? parseInt(prNumberMatch[1], 10) : 0;

    return {
      success: true,
      pr: {
        url: prUrl,
        number: prNumber,
        title: title ?? "",
        body: body ?? "",
        state: "open",
        createdAt: new Date().toISOString(),
      },
    };
  } catch (err) {
    const message = err instanceof Error ? (err as any).stderr ?? err.message : String(err);
    return { success: false, error: message };
  }
}

// ─── Update PR ───────────────────────────────────────────────────────────────

/**
 * Updates an existing PR: pushes new commits and optionally updates title/body.
 */
export async function updatePR(options: PRUpdateOptions): Promise<{ success: boolean; error?: string }> {
  const { repoPath, branch, prNumber, title, body } = options;

  try {
    // Push new commits
    const pushResult = await pushBranch(repoPath, branch);
    if (!pushResult.success) {
      return { success: false, error: `Push failed: ${pushResult.error}` };
    }

    // Update PR metadata if title or body provided
    if (title || body) {
      const args = ["pr", "edit", String(prNumber)];
      if (title) args.push("--title", title);
      if (body) args.push("--body", body);
      await execFile("gh", args, { cwd: repoPath });
    }

    return { success: true };
  } catch (err) {
    const message = err instanceof Error ? (err as any).stderr ?? err.message : String(err);
    return { success: false, error: message };
  }
}

// ─── Get PR Checks (CI Status) ──────────────────────────────────────────────

export interface CICheck {
  name: string;
  status: "pass" | "fail" | "pending" | "queued";
  conclusion?: string;
  durationMs?: number;
  url?: string;
  output?: string;
}

export interface PRChecksData {
  pr?: {
    title: string;
    number: number;
    url: string;
    state: string;
    reviewers: Array<{ login: string; state: string }>;
    mergeable: boolean;
    mergeStateStatus: string;
  };
  checks: CICheck[];
  comments: Array<{ id: string; body: string; path?: string; line?: number; resolved: boolean }>;
}

/**
 * Fetches PR checks, review status, and CI status via `gh` CLI.
 */
export async function getPRChecks(repoPath: string, prNumber: number): Promise<{ success: boolean; data?: PRChecksData; error?: string }> {
  try {
    // Get PR status
    const { stdout: prJson } = await execFile(
      "gh",
      ["pr", "view", String(prNumber), "--json", "title,number,url,state,reviewDecision,reviews,statusCheckRollup,mergeable,mergeStateStatus"],
      { cwd: repoPath },
    );

    const prData = JSON.parse(prJson);

    // Parse checks
    const checks: CICheck[] = (prData.statusCheckRollup ?? []).map((check: any) => ({
      name: check.name ?? check.context ?? "Unknown",
      status: mapCheckStatus(check.status ?? check.state, check.conclusion),
      conclusion: check.conclusion,
      durationMs: check.startedAt && check.completedAt
        ? new Date(check.completedAt).getTime() - new Date(check.startedAt).getTime()
        : undefined,
      url: check.detailsUrl ?? check.targetUrl,
    }));

    // Parse reviewers
    const reviewers = (prData.reviews ?? []).map((r: any) => ({
      login: r.author?.login ?? "unknown",
      state: r.state ?? "PENDING",
    }));

    // Get comments
    let comments: PRChecksData["comments"] = [];
    try {
      const { stdout: commentsJson } = await execFile(
        "gh",
        ["pr", "view", String(prNumber), "--json", "comments,reviewComments"],
        { cwd: repoPath },
      );
      const commentsData = JSON.parse(commentsJson);
      const reviewComments = commentsData.reviewComments ?? [];
      comments = reviewComments.map((c: any) => ({
        id: c.id ?? String(Math.random()),
        body: c.body ?? "",
        path: c.path,
        line: c.line,
        resolved: c.isResolved ?? false,
      }));
    } catch {
      // Comments fetch is best-effort
    }

    return {
      success: true,
      data: {
        pr: {
          title: prData.title,
          number: prData.number,
          url: prData.url,
          state: prData.state,
          reviewers,
          mergeable: prData.mergeable === "MERGEABLE",
          mergeStateStatus: prData.mergeStateStatus ?? "UNKNOWN",
        },
        checks,
        comments,
      },
    };
  } catch (err) {
    const message = err instanceof Error ? (err as any).stderr ?? err.message : String(err);
    return { success: false, error: message };
  }
}

function mapCheckStatus(status: string, conclusion?: string): CICheck["status"] {
  if (status === "COMPLETED" || status === "completed") {
    if (conclusion === "SUCCESS" || conclusion === "success") return "pass";
    if (conclusion === "FAILURE" || conclusion === "failure") return "fail";
    return "fail";
  }
  if (status === "IN_PROGRESS" || status === "in_progress" || status === "PENDING" || status === "pending") {
    return "pending";
  }
  if (status === "QUEUED" || status === "queued") return "queued";
  return "pending";
}

// ─── Merge PR ────────────────────────────────────────────────────────────────

/**
 * Merges a PR via the `gh` CLI.
 */
export async function mergePR(
  repoPath: string,
  prNumber: number,
  method: "merge" | "squash" | "rebase" = "squash",
): Promise<{ success: boolean; error?: string }> {
  try {
    await execFile(
      "gh",
      ["pr", "merge", String(prNumber), `--${method}`, "--delete-branch"],
      { cwd: repoPath },
    );
    return { success: true };
  } catch (err) {
    const message = err instanceof Error ? (err as any).stderr ?? err.message : String(err);
    return { success: false, error: message };
  }
}

// ─── Git Status for Checks ───────────────────────────────────────────────────

export interface GitStatusInfo {
  clean: boolean;
  ahead: number;
  behind: number;
  conflicts: string[];
}

/**
 * Gets git status information for the workspace branch relative to base.
 */
export async function getGitStatus(worktreePath: string, baseRef: string): Promise<GitStatusInfo> {
  let clean = true;
  let ahead = 0;
  let behind = 0;
  const conflicts: string[] = [];

  try {
    // Check if working tree is clean
    const { stdout: statusOut } = await execFile("git", ["status", "--porcelain"], { cwd: worktreePath });
    clean = statusOut.trim() === "";

    // Check ahead/behind
    try {
      const { stdout: revList } = await execFile(
        "git",
        ["rev-list", "--left-right", "--count", `${baseRef}...HEAD`],
        { cwd: worktreePath },
      );
      const parts = revList.trim().split(/\s+/);
      behind = parseInt(parts[0], 10) || 0;
      ahead = parseInt(parts[1], 10) || 0;
    } catch {
      // rev-list may fail if baseRef doesn't exist locally
    }

    // Check for conflicts
    const { stdout: conflictOut } = await execFile(
      "git",
      ["diff", "--name-only", "--diff-filter=U"],
      { cwd: worktreePath },
    );
    if (conflictOut.trim()) {
      conflicts.push(...conflictOut.trim().split("\n"));
    }
  } catch {
    // Best-effort status
  }

  return { clean, ahead, behind, conflicts };
}
