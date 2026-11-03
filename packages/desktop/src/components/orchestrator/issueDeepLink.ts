/**
 * Issue deep-link detection.
 *
 * Listens for clicks on GitHub issue URLs within the app and dispatches
 * a custom event that the AgentTasksView can listen for to open the
 * New Task dialog with the issue pre-loaded.
 *
 * Pattern: https://github.com/{owner}/{repo}/issues/{number}
 */

import type { IssueItem } from "./IssueSelector";

// ── URL Pattern ─────────────────────────────────────────────────────

const GITHUB_ISSUE_PATTERN =
  /^https?:\/\/github\.com\/([^/]+)\/([^/]+)\/issues\/(\d+)\/?$/;

/**
 * Parse a GitHub issue URL into its components.
 * Returns null if the URL doesn't match the expected pattern.
 */
export function parseGitHubIssueUrl(
  url: string,
): { owner: string; repo: string; number: number } | null {
  const match = url.match(GITHUB_ISSUE_PATTERN);
  if (!match) return null;
  return {
    owner: match[1],
    repo: match[2],
    number: parseInt(match[3], 10),
  };
}

/**
 * Dispatch a custom event to create a task from a GitHub issue URL.
 *
 * The AgentTasksView listens for `crab:create-task-from-issue` events
 * and opens the New Task dialog with the issue pre-loaded.
 */
export function dispatchCreateTaskFromIssue(issue: IssueItem): void {
  window.dispatchEvent(
    new CustomEvent("crab:create-task-from-issue", {
      detail: { issue, source: "github" as const },
    }),
  );
}

/**
 * Install a global click listener that intercepts clicks on GitHub issue
 * URLs and offers to create a task from them.
 *
 * Call this once at app startup. Returns a cleanup function.
 */
export function installIssueDeepLinkListener(): () => void {
  const handler = (e: MouseEvent) => {
    const target = e.target as HTMLElement;
    const anchor = target.closest("a[href]") as HTMLAnchorElement | null;
    if (!anchor) return;

    const href = anchor.href;
    const parsed = parseGitHubIssueUrl(href);
    if (!parsed) return;

    // Don't prevent default — let the link still work if the user
    // wants to open it. Instead, dispatch the event so the UI can
    // offer the "Create task from this issue" action.
    const issue: IssueItem = {
      number: parsed.number,
      title: `Issue #${parsed.number}`, // Placeholder — will be fetched
      state: "open",
      author: "",
      url: href,
      labels: [],
    };

    dispatchCreateTaskFromIssue(issue);
  };

  document.addEventListener("click", handler, { capture: true });
  return () => document.removeEventListener("click", handler, { capture: true });
}
