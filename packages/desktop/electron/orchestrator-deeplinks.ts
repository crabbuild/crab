/**
 * Deep Link Protocol Handler for the Crab Desktop Orchestrator.
 *
 * Handles `crab://` URL protocol for external integrations (CI systems,
 * Slack bots, Linear webhooks, browser extensions, etc.).
 *
 * ## URL Format
 *
 * ```
 * crab://<path>?<query-params>
 * ```
 *
 * ### Supported Paths
 *
 * - `crab://task?prompt=<encoded>&repo=<path>` — Create a workspace with the given prompt
 * - `crab://task?prompt=<encoded>&template=<name>` — Create from a named template
 * - `crab://issue?github=<owner/repo#number>` — Fetch a GitHub issue and create workspace
 * - `crab://issue?linear=<issue-id>` — Fetch a Linear issue and create workspace
 * - `crab://workspace?id=<id>&action=<action>` — Navigate to a workspace
 * - `crab://plan?repo=<name>&plan=<base64>` — Create workspace with a pre-built plan
 *
 * ### Security Model
 *
 * Deep links do NOT auto-execute. The `handleDeepLink` function returns a
 * `DeepLinkAction` object describing the intended operation. The main process
 * must show a confirmation dialog to the user before executing the action.
 *
 * @example
 * ```typescript
 * import { registerDeepLinks, handleDeepLink } from "./orchestrator-deeplinks";
 *
 * // Register protocol on app startup
 * registerDeepLinks();
 *
 * // Handle incoming deep link
 * app.on("open-url", (event, url) => {
 *   event.preventDefault();
 *   const action = handleDeepLink(url);
 *   if (action.type === "error") {
 *     showErrorToast(action.message);
 *   } else {
 *     showConfirmationDialog(action);
 *   }
 * });
 * ```
 *
 * ### Platform Registration
 *
 * Protocol registration is handled per-platform:
 * - **macOS**: `Info.plist` CFBundleURLTypes (electron-builder `protocols` config)
 * - **Windows**: Registry key `HKCU\Software\Classes\crab` (electron-builder handles this)
 * - **Linux**: `xdg-open` / `.desktop` file with `MimeType=x-scheme-handler/crab`
 *
 * electron-builder handles all three via the `protocols` field in `electron-builder.yml`:
 * ```yaml
 * protocols:
 *   - name: Crab
 *     schemes:
 *       - crab
 * ```
 */

import { app } from "electron";

// ─── Types ───────────────────────────────────────────────────────────────────

/** Actions that can be triggered by workspace navigation deep links. */
export type WorkspaceAction = "open-diff" | "create-pr" | "merge";

/**
 * Describes what a deep link wants to do. The main process should present
 * this to the user in a confirmation dialog before executing.
 */
export type DeepLinkAction =
  | DeepLinkTaskAction
  | DeepLinkTemplateTaskAction
  | DeepLinkGitHubIssueAction
  | DeepLinkLinearIssueAction
  | DeepLinkWorkspaceAction
  | DeepLinkPlanAction
  | DeepLinkError;

export interface DeepLinkTaskAction {
  type: "create-task";
  prompt: string;
  repo: string;
}

export interface DeepLinkTemplateTaskAction {
  type: "create-task-from-template";
  prompt: string;
  template: string;
}

export interface DeepLinkGitHubIssueAction {
  type: "github-issue";
  owner: string;
  repo: string;
  number: number;
}

export interface DeepLinkLinearIssueAction {
  type: "linear-issue";
  issueId: string;
}

export interface DeepLinkWorkspaceAction {
  type: "navigate-workspace";
  workspaceId: string;
  action?: WorkspaceAction;
}

export interface DeepLinkPlanAction {
  type: "create-plan";
  repo: string;
  plan: string;
}

export interface DeepLinkError {
  type: "error";
  message: string;
  url: string;
}

// ─── Protocol Registration ───────────────────────────────────────────────────

/**
 * Registers the `crab://` protocol with the operating system.
 *
 * Call this once during app initialization (before `app.whenReady()`).
 * On macOS, this sets the app as the default handler for `crab://` URLs.
 * On Windows and Linux, electron-builder's `protocols` config handles the
 * system-level registration at install time; this call ensures the running
 * instance claims the protocol at runtime.
 *
 * NOTE: Full platform registration requires electron-builder configuration:
 * - macOS: Info.plist CFBundleURLTypes (added by electron-builder `protocols`)
 * - Windows: Registry HKCU\Software\Classes\crab (added by NSIS installer)
 * - Linux: .desktop file MimeType=x-scheme-handler/crab (added by deb/AppImage)
 */
export function registerDeepLinks(): void {
  app.setAsDefaultProtocolClient("crab");
}

// ─── URL Parsing & Routing ───────────────────────────────────────────────────

/**
 * Parses a `crab://` URL and returns a structured action object.
 *
 * Does NOT execute the action — the caller (main process) must show a
 * confirmation dialog before acting on the returned object.
 *
 * @param url - The full deep link URL (e.g., `crab://task?prompt=fix+bug&repo=/path`)
 * @returns A `DeepLinkAction` describing the intended operation, or a `DeepLinkError`
 *
 * @example
 * ```typescript
 * const action = handleDeepLink("crab://task?prompt=fix%20the%20login%20bug&repo=/home/user/myapp");
 * // action.type === "create-task"
 * // action.prompt === "fix the login bug"
 * // action.repo === "/home/user/myapp"
 * ```
 */
export function handleDeepLink(url: string): DeepLinkAction {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return {
      type: "error",
      message: `Invalid URL: could not parse "${url}"`,
      url,
    };
  }

  // Validate the protocol
  if (parsed.protocol !== "crab:") {
    return {
      type: "error",
      message: `Unsupported protocol "${parsed.protocol}". Expected "crab:".`,
      url,
    };
  }

  // The URL class parses `crab://task?...` with host="task" and pathname="/"
  // or `crab://task` with host="task". We use the host as the route.
  const route = parsed.host || parsed.pathname.replace(/^\//, "");
  const params = parsed.searchParams;

  switch (route) {
    case "task":
      return parseTaskDeepLink(params, url);
    case "issue":
      return parseIssueDeepLink(params, url);
    case "workspace":
      return parseWorkspaceDeepLink(params, url);
    case "plan":
      return parsePlanDeepLink(params, url);
    default:
      return {
        type: "error",
        message: `Unknown deep link path "${route}". Supported: task, issue, workspace, plan.`,
        url,
      };
  }
}

// ─── Route Parsers ───────────────────────────────────────────────────────────

/**
 * Parses `crab://task?prompt=<encoded>&repo=<path>` or
 * `crab://task?prompt=<encoded>&template=<name>`.
 *
 * @example
 * ```
 * crab://task?prompt=Fix%20the%20login%20bug&repo=/Users/dev/myapp
 * crab://task?prompt=Add%20tests&template=feature
 * ```
 */
function parseTaskDeepLink(params: URLSearchParams, url: string): DeepLinkAction {
  const prompt = params.get("prompt");
  if (!prompt) {
    return {
      type: "error",
      message: "Missing required parameter \"prompt\" for task deep link.",
      url,
    };
  }

  const template = params.get("template");
  if (template) {
    return {
      type: "create-task-from-template",
      prompt: decodeURIComponent(prompt),
      template,
    };
  }

  const repo = params.get("repo");
  if (!repo) {
    return {
      type: "error",
      message: "Missing required parameter \"repo\" or \"template\" for task deep link.",
      url,
    };
  }

  return {
    type: "create-task",
    prompt: decodeURIComponent(prompt),
    repo: decodeURIComponent(repo),
  };
}

/**
 * Parses `crab://issue?github=<owner/repo#number>` or
 * `crab://issue?linear=<issue-id>`.
 *
 * @example
 * ```
 * crab://issue?github=acme/webapp%2342
 * crab://issue?linear=ENG-1234
 * ```
 */
function parseIssueDeepLink(params: URLSearchParams, url: string): DeepLinkAction {
  const github = params.get("github");
  if (github) {
    return parseGitHubIssueParam(github, url);
  }

  const linear = params.get("linear");
  if (linear) {
    if (!linear.trim()) {
      return {
        type: "error",
        message: "Empty Linear issue ID in deep link.",
        url,
      };
    }
    return {
      type: "linear-issue",
      issueId: linear.trim(),
    };
  }

  return {
    type: "error",
    message: "Missing required parameter \"github\" or \"linear\" for issue deep link.",
    url,
  };
}

/**
 * Parses the `github` parameter value in the format `owner/repo#number`.
 */
function parseGitHubIssueParam(value: string, url: string): DeepLinkAction {
  // Expected format: owner/repo#number
  const match = value.match(/^([^/]+)\/([^#]+)#(\d+)$/);
  if (!match) {
    return {
      type: "error",
      message: `Invalid GitHub issue format "${value}". Expected "owner/repo#number".`,
      url,
    };
  }

  const [, owner, repo, numberStr] = match;
  const issueNumber = parseInt(numberStr, 10);

  if (isNaN(issueNumber) || issueNumber <= 0) {
    return {
      type: "error",
      message: `Invalid issue number in "${value}".`,
      url,
    };
  }

  return {
    type: "github-issue",
    owner,
    repo,
    number: issueNumber,
  };
}

/**
 * Parses `crab://workspace?id=<id>&action=<action>`.
 *
 * @example
 * ```
 * crab://workspace?id=ws_abc123&action=open-diff
 * crab://workspace?id=ws_abc123
 * ```
 */
function parseWorkspaceDeepLink(params: URLSearchParams, url: string): DeepLinkAction {
  const id = params.get("id");
  if (!id) {
    return {
      type: "error",
      message: "Missing required parameter \"id\" for workspace deep link.",
      url,
    };
  }

  const actionParam = params.get("action");
  let action: WorkspaceAction | undefined;

  if (actionParam) {
    const validActions: WorkspaceAction[] = ["open-diff", "create-pr", "merge"];
    if (!validActions.includes(actionParam as WorkspaceAction)) {
      return {
        type: "error",
        message: `Invalid workspace action "${actionParam}". Supported: ${validActions.join(", ")}.`,
        url,
      };
    }
    action = actionParam as WorkspaceAction;
  }

  return {
    type: "navigate-workspace",
    workspaceId: id,
    action,
  };
}

/**
 * Parses `crab://plan?repo=<name>&plan=<base64>`.
 *
 * The `plan` parameter is a base64-encoded plan document that will be
 * attached as context to the new workspace.
 *
 * @example
 * ```
 * crab://plan?repo=myapp&plan=eyJzdGVwcyI6WyJhZGQgdGVzdHMiLCJyZWZhY3RvciJdfQ==
 * ```
 */
function parsePlanDeepLink(params: URLSearchParams, url: string): DeepLinkAction {
  const repo = params.get("repo");
  if (!repo) {
    return {
      type: "error",
      message: "Missing required parameter \"repo\" for plan deep link.",
      url,
    };
  }

  const plan = params.get("plan");
  if (!plan) {
    return {
      type: "error",
      message: "Missing required parameter \"plan\" for plan deep link.",
      url,
    };
  }

  // Validate base64 encoding
  let decoded: string;
  try {
    decoded = Buffer.from(plan, "base64").toString("utf-8");
  } catch {
    return {
      type: "error",
      message: "Invalid base64 encoding in \"plan\" parameter.",
      url,
    };
  }

  return {
    type: "create-plan",
    repo,
    plan: decoded,
  };
}
