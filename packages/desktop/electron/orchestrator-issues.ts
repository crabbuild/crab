/**
 * Issue fetching for the orchestrator.
 *
 * Provides functions to fetch issues from GitHub (via the existing forge RPC)
 * and Linear (via their GraphQL API). Tokens are retrieved from the OS keychain.
 */

import { getCredential } from "./keychain";

// ── Types ───────────────────────────────────────────────────────────

export interface FetchedIssue {
  number: number;
  title: string;
  body?: string;
  state: string;
  author: string;
  url: string;
  labels: string[];
  assignee?: string;
  milestone?: string;
  discussion?: string;
}

interface FetchIssuesResult {
  success: boolean;
  issues?: FetchedIssue[];
  error?: string;
  authRequired?: boolean;
}

// ── GitHub Issues ───────────────────────────────────────────────────

/**
 * Fetch open issues from GitHub using the REST API.
 *
 * Reads the GitHub token from the keychain (stored by forge-auth).
 * Falls back to the `GITHUB_TOKEN` environment variable if no keychain
 * entry exists.
 */
export async function fetchGitHubIssues(): Promise<FetchIssuesResult> {
  // Try keychain first, then env var.
  const token =
    (await getCredential("github-token")) ??
    process.env.GITHUB_TOKEN ??
    null;

  if (!token) {
    return { success: false, authRequired: true };
  }

  // Determine the repo from git remote origin.
  const repoSlug = await detectGitHubRepo();
  if (!repoSlug) {
    return { success: false, error: "Could not detect GitHub repository from git remote" };
  }

  try {
    const resp = await fetch(
      `https://api.github.com/repos/${repoSlug}/issues?state=open&per_page=50`,
      {
        headers: {
          Authorization: `Bearer ${token}`,
          Accept: "application/vnd.github+json",
          "User-Agent": "crab-desktop",
        },
      },
    );

    if (resp.status === 401 || resp.status === 403) {
      return { success: false, authRequired: true };
    }

    if (!resp.ok) {
      const body = await resp.text();
      return { success: false, error: `GitHub API ${resp.status}: ${body.slice(0, 200)}` };
    }

    const data = await resp.json() as GhIssueResponse[];

    // Filter out pull requests (GitHub returns them in the issues endpoint).
    const issues: FetchedIssue[] = data
      .filter((item) => !item.pull_request)
      .map((item) => ({
        number: item.number,
        title: item.title,
        body: item.body ?? undefined,
        state: item.state,
        author: item.user?.login ?? "",
        url: item.html_url,
        labels: (item.labels ?? []).map((l) =>
          typeof l === "string" ? l : l.name,
        ),
        assignee: item.assignee?.login ?? undefined,
        milestone: item.milestone?.title ?? undefined,
      }));

    return { success: true, issues };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { success: false, error: message };
  }
}

// ── Linear Issues ───────────────────────────────────────────────────

/**
 * Fetch issues from Linear via their GraphQL API.
 *
 * Requires a Linear API token stored in the keychain under "linear-token".
 */
export async function fetchLinearIssues(): Promise<FetchIssuesResult> {
  const token = await getCredential("linear-token");

  if (!token) {
    return { success: false, authRequired: true };
  }

  const query = `
    query {
      issues(
        filter: { state: { type: { nin: ["completed", "canceled"] } } }
        first: 50
        orderBy: updatedAt
      ) {
        nodes {
          number
          title
          description
          state { name }
          creator { name }
          assignee { name }
          url
          labels { nodes { name } }
        }
      }
    }
  `;

  try {
    const resp = await fetch("https://api.linear.app/graphql", {
      method: "POST",
      headers: {
        Authorization: token,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ query }),
    });

    if (resp.status === 401 || resp.status === 403) {
      return { success: false, authRequired: true };
    }

    if (!resp.ok) {
      const body = await resp.text();
      return { success: false, error: `Linear API ${resp.status}: ${body.slice(0, 200)}` };
    }

    const json = await resp.json() as LinearGraphQLResponse;

    if (json.errors?.length) {
      return { success: false, error: json.errors[0].message };
    }

    const nodes = json.data?.issues?.nodes ?? [];
    const issues: FetchedIssue[] = nodes.map((node) => ({
      number: node.number,
      title: node.title,
      body: node.description ?? undefined,
      state: node.state?.name ?? "open",
      author: node.creator?.name ?? "",
      url: node.url,
      labels: (node.labels?.nodes ?? []).map((l) => l.name),
      assignee: node.assignee?.name ?? undefined,
    }));

    return { success: true, issues };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { success: false, error: message };
  }
}

// ── Helpers ─────────────────────────────────────────────────────────

/**
 * Detect the GitHub owner/repo from the git remote origin URL.
 * Returns a string like "owner/repo" or null if not detectable.
 */
async function detectGitHubRepo(): Promise<string | null> {
  try {
    const { execFile: execFileCb } = await import("node:child_process");
    const { promisify } = await import("node:util");
    const execFile = promisify(execFileCb);

    const { stdout } = await execFile("git", ["remote", "get-url", "origin"]);
    const url = stdout.trim();

    // Match HTTPS: https://github.com/owner/repo.git
    const httpsMatch = url.match(/github\.com[/:]([^/]+)\/([^/.]+)/);
    if (httpsMatch) {
      return `${httpsMatch[1]}/${httpsMatch[2]}`;
    }

    // Match SSH: git@github.com:owner/repo.git
    const sshMatch = url.match(/github\.com:([^/]+)\/([^/.]+)/);
    if (sshMatch) {
      return `${sshMatch[1]}/${sshMatch[2]}`;
    }

    return null;
  } catch {
    return null;
  }
}

// ── GitHub API response types ───────────────────────────────────────

interface GhIssueResponse {
  number: number;
  title: string;
  body: string | null;
  state: string;
  html_url: string;
  user: { login: string } | null;
  labels: Array<string | { name: string }>;
  assignee: { login: string } | null;
  milestone: { title: string } | null;
  pull_request?: unknown;
}

// ── Linear API response types ───────────────────────────────────────

interface LinearGraphQLResponse {
  data?: {
    issues?: {
      nodes: LinearIssueNode[];
    };
  };
  errors?: Array<{ message: string }>;
}

interface LinearIssueNode {
  number: number;
  title: string;
  description: string | null;
  state: { name: string } | null;
  creator: { name: string } | null;
  assignee: { name: string } | null;
  url: string;
  labels: { nodes: Array<{ name: string }> } | null;
}
