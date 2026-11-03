import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  Check,
  ExternalLink,
  GitBranch,
  Loader2,
} from "lucide-react";
import type { KanbanBoardConfig, KanbanTrackerSource } from "../../ipc";

interface TrackerConnectProps {
  board: KanbanBoardConfig;
  onSave: (config: Partial<KanbanBoardConfig>) => void;
}

type TrackerOption = {
  value: KanbanTrackerSource;
  label: string;
  desc: string;
};

const TRACKER_OPTIONS: TrackerOption[] = [
  {
    value: "github",
    label: "GitHub Issues",
    desc: "Sync with GitHub Issues via REST API",
  },
  {
    value: "github-projects",
    label: "GitHub Projects",
    desc: "Sync with GitHub Projects v2 via GraphQL",
  },
  {
    value: "jira",
    label: "Jira",
    desc: "Sync with Jira Cloud or Server/DC",
  },
  {
    value: "linear",
    label: "Linear",
    desc: "Sync Linear issues for Symphony execution",
  },
];

interface GitHubIssuesForm {
  owner: string;
  repo: string;
}

interface GitHubProjectsForm {
  projectNumber: string;
  owner: string;
  statusFieldName: string;
}

interface JiraForm {
  instanceUrl: string;
  projectKey: string;
  authType: "oauth" | "pat";
}

interface LinearForm {
  projectSlug: string;
}

interface FilterForm {
  labels: string;
  assignees: string;
  milestones: string;
  excludeLabels: string;
}

/**
 * Tracker connection component. Allows selecting a tracker type,
 * configuring connection details, and editing filter criteria.
 * For GitHub Issues, auto-detects owner/repo from git remote.
 */
export function TrackerConnect({ board, onSave }: TrackerConnectProps) {
  const [trackerType, setTrackerType] = useState<KanbanTrackerSource>(
    board.trackerType || "github",
  );
  const [connecting, setConnecting] = useState(false);
  const [connectError, setConnectError] = useState<string | null>(null);
  const [connectSuccess, setConnectSuccess] = useState(false);

  // GitHub Issues form
  const [githubForm, setGithubForm] = useState<GitHubIssuesForm>(() => {
    const cfg = board.trackerConfig as Record<string, unknown>;
    return {
      owner: (cfg?.owner as string) ?? "",
      repo: (cfg?.repo as string) ?? "",
    };
  });
  const [autoDetected, setAutoDetected] = useState(false);

  // GitHub Projects form
  const [projectsForm, setProjectsForm] = useState<GitHubProjectsForm>(() => {
    const cfg = board.trackerConfig as Record<string, unknown>;
    return {
      projectNumber: String(cfg?.projectNumber ?? ""),
      owner: (cfg?.owner as string) ?? "",
      statusFieldName: (cfg?.statusFieldName as string) ?? "Status",
    };
  });

  // Jira form
  const [jiraForm, setJiraForm] = useState<JiraForm>(() => {
    const cfg = board.trackerConfig as Record<string, unknown>;
    return {
      instanceUrl: (cfg?.instanceUrl as string) ?? "",
      projectKey: (cfg?.projectKey as string) ?? "",
      authType: (cfg?.authType as "oauth" | "pat") ?? "pat",
    };
  });

  const [linearForm, setLinearForm] = useState<LinearForm>(() => {
    const cfg = board.trackerConfig as Record<string, unknown>;
    return {
      projectSlug: (cfg?.projectSlug as string) ?? "",
    };
  });

  // Filter criteria
  const [filterForm, setFilterForm] = useState<FilterForm>(() => ({
    labels: board.filterCriteria.labels?.join(", ") ?? "",
    assignees: board.filterCriteria.assignees?.join(", ") ?? "",
    milestones: board.filterCriteria.milestones?.join(", ") ?? "",
    excludeLabels: board.filterCriteria.excludeLabels?.join(", ") ?? "",
  }));

  // Auto-detect owner/repo from git remote for GitHub Issues
  useEffect(() => {
    if (trackerType !== "github" || githubForm.owner || githubForm.repo) return;

    async function detectRemote() {
      try {
        // Use the rpc to get git remote info
        const result = await window.crab.rpc("git_remotes", {
          repo_id: board.repoPath,
        }) as Array<{ name: string; url: string }> | null;

        if (!result || result.length === 0) return;

        const origin = result.find((r) => r.name === "origin") ?? result[0];
        const match = origin.url.match(
          /github\.com[:/]([^/]+)\/([^/.]+)/,
        );
        if (match) {
          setGithubForm({ owner: match[1], repo: match[2] });
          setAutoDetected(true);
        }
      } catch {
        // Detection is best-effort — user can fill in manually
      }
    }
    detectRemote();
  }, [trackerType, githubForm.owner, githubForm.repo, board.repoPath]);

  // Also auto-detect for GitHub Projects owner
  useEffect(() => {
    if (trackerType !== "github-projects" || projectsForm.owner) return;

    async function detectOwner() {
      try {
        const result = await window.crab.rpc("git_remotes", {
          repo_id: board.repoPath,
        }) as Array<{ name: string; url: string }> | null;

        if (!result || result.length === 0) return;

        const origin = result.find((r) => r.name === "origin") ?? result[0];
        const match = origin.url.match(
          /github\.com[:/]([^/]+)\//,
        );
        if (match) {
          setProjectsForm((prev) => ({ ...prev, owner: match[1] }));
        }
      } catch {
        // Best-effort
      }
    }
    detectOwner();
  }, [trackerType, projectsForm.owner, board.repoPath]);

  // ── Connect Handler ───────────────────────────────────────────

  const handleConnect = useCallback(async () => {
    setConnecting(true);
    setConnectError(null);
    setConnectSuccess(false);

    let config: Record<string, unknown> = {};

    if (trackerType === "github") {
      if (!githubForm.owner || !githubForm.repo) {
        setConnectError("Owner and repo are required.");
        setConnecting(false);
        return;
      }
      config = { owner: githubForm.owner, repo: githubForm.repo };
    } else if (trackerType === "github-projects") {
      const num = parseInt(projectsForm.projectNumber, 10);
      if (!projectsForm.owner || isNaN(num)) {
        setConnectError("Owner and project number are required.");
        setConnecting(false);
        return;
      }
      config = {
        owner: projectsForm.owner,
        projectNumber: num,
        statusFieldName: projectsForm.statusFieldName || "Status",
      };
    } else if (trackerType === "jira") {
      if (!jiraForm.instanceUrl || !jiraForm.projectKey) {
        setConnectError("Instance URL and project key are required.");
        setConnecting(false);
        return;
      }
      config = {
        instanceUrl: jiraForm.instanceUrl.replace(/\/+$/, ""),
        projectKey: jiraForm.projectKey.toUpperCase(),
        authType: jiraForm.authType,
      };
    } else if (trackerType === "linear") {
      if (!linearForm.projectSlug.trim()) {
        setConnectError("Linear project slug is required.");
        setConnecting(false);
        return;
      }
      config = {
        projectSlug: linearForm.projectSlug.trim(),
      };
    }

    try {
      const result = await window.crab.kanban.connectTracker(trackerType, config);
      if (result.success) {
        setConnectSuccess(true);
        // Persist the tracker config to the board
        onSave({
          trackerType,
          trackerConfig: config,
        });
      } else {
        setConnectError(result.error ?? "Connection failed.");
      }
    } catch (err) {
      setConnectError(
        err instanceof Error ? err.message : "Connection failed.",
      );
    } finally {
      setConnecting(false);
    }
  }, [trackerType, githubForm, projectsForm, jiraForm, linearForm, onSave]);

  // ── Filter Persistence ────────────────────────────────────────

  const handleFilterChange = useCallback(
    (field: keyof FilterForm, value: string) => {
      setFilterForm((prev) => ({ ...prev, [field]: value }));

      // Parse and persist filter criteria with debounce handled by parent
      const parseList = (s: string) =>
        s
          .split(",")
          .map((v) => v.trim())
          .filter(Boolean);

      const updated = { ...filterForm, [field]: value };
      onSave({
        filterCriteria: {
          labels: parseList(updated.labels).length > 0 ? parseList(updated.labels) : undefined,
          assignees: parseList(updated.assignees).length > 0 ? parseList(updated.assignees) : undefined,
          milestones: parseList(updated.milestones).length > 0 ? parseList(updated.milestones) : undefined,
          excludeLabels: parseList(updated.excludeLabels).length > 0 ? parseList(updated.excludeLabels) : undefined,
        },
      });
    },
    [filterForm, onSave],
  );

  return (
    <div className="space-y-5">
      {/* ── Tracker Type Selector ──────────────────────────── */}
      <section>
        <h3 className="text-xs font-medium text-gx-text mb-2">
          Tracker Type
        </h3>
        <div className="flex flex-col gap-1.5">
          {TRACKER_OPTIONS.map((opt) => (
            <label
              key={opt.value}
              className={clsx(
                "flex items-start gap-2 rounded border p-2 cursor-pointer transition",
                trackerType === opt.value
                  ? "border-gx-accent/60 bg-gx-accent/5"
                  : "border-gx-border hover:border-gx-accent/30",
              )}
            >
              <input
                type="radio"
                name="tracker-type"
                value={opt.value}
                checked={trackerType === opt.value}
                onChange={() => {
                  setTrackerType(opt.value);
                  setConnectError(null);
                  setConnectSuccess(false);
                }}
                className="mt-0.5 accent-gx-accent"
              />
              <div className="min-w-0">
                <span className="text-xs font-medium text-gx-text">
                  {opt.label}
                </span>
                <p className="text-[10px] text-gx-muted">{opt.desc}</p>
              </div>
            </label>
          ))}
        </div>
      </section>

      {/* ── Connection Config ──────────────────────────────── */}
      <section>
        <h3 className="text-xs font-medium text-gx-text mb-2">
          Connection
        </h3>

        {/* GitHub Issues */}
        {trackerType === "github" && (
          <div className="space-y-2">
            {autoDetected && (
              <div className="flex items-center gap-1.5 text-[10px] text-gx-success">
                <GitBranch size={10} />
                Auto-detected from git remote
              </div>
            )}
            <div className="flex gap-2">
              <div className="flex-1">
                <label className="text-[10px] text-gx-muted block mb-0.5">
                  Owner
                </label>
                <input
                  type="text"
                  className="input-field w-full"
                  placeholder="owner"
                  value={githubForm.owner}
                  onChange={(e) =>
                    setGithubForm((prev) => ({ ...prev, owner: e.target.value }))
                  }
                />
              </div>
              <div className="flex-1">
                <label className="text-[10px] text-gx-muted block mb-0.5">
                  Repository
                </label>
                <input
                  type="text"
                  className="input-field w-full"
                  placeholder="repo"
                  value={githubForm.repo}
                  onChange={(e) =>
                    setGithubForm((prev) => ({ ...prev, repo: e.target.value }))
                  }
                />
              </div>
            </div>
          </div>
        )}

        {/* GitHub Projects */}
        {trackerType === "github-projects" && (
          <div className="space-y-2">
            <div className="flex gap-2">
              <div className="flex-1">
                <label className="text-[10px] text-gx-muted block mb-0.5">
                  Owner
                </label>
                <input
                  type="text"
                  className="input-field w-full"
                  placeholder="org or user"
                  value={projectsForm.owner}
                  onChange={(e) =>
                    setProjectsForm((prev) => ({ ...prev, owner: e.target.value }))
                  }
                />
              </div>
              <div className="w-24">
                <label className="text-[10px] text-gx-muted block mb-0.5">
                  Project #
                </label>
                <input
                  type="number"
                  className="input-field w-full"
                  placeholder="1"
                  min={1}
                  value={projectsForm.projectNumber}
                  onChange={(e) =>
                    setProjectsForm((prev) => ({
                      ...prev,
                      projectNumber: e.target.value,
                    }))
                  }
                />
              </div>
            </div>
            <div>
              <label className="text-[10px] text-gx-muted block mb-0.5">
                Status Field Name
              </label>
              <input
                type="text"
                className="input-field w-full"
                placeholder="Status"
                value={projectsForm.statusFieldName}
                onChange={(e) =>
                  setProjectsForm((prev) => ({
                    ...prev,
                    statusFieldName: e.target.value,
                  }))
                }
              />
              <p className="mt-0.5 text-[10px] text-gx-muted">
                The single-select field used for column mapping (default: &quot;Status&quot;)
              </p>
            </div>
          </div>
        )}

        {/* Jira */}
        {trackerType === "jira" && (
          <div className="space-y-2">
            <div>
              <label className="text-[10px] text-gx-muted block mb-0.5">
                Instance URL
              </label>
              <input
                type="url"
                className="input-field w-full"
                placeholder="https://your-org.atlassian.net"
                value={jiraForm.instanceUrl}
                onChange={(e) =>
                  setJiraForm((prev) => ({ ...prev, instanceUrl: e.target.value }))
                }
              />
            </div>
            <div>
              <label className="text-[10px] text-gx-muted block mb-0.5">
                Project Key
              </label>
              <input
                type="text"
                className="input-field w-full"
                placeholder="PROJ"
                value={jiraForm.projectKey}
                onChange={(e) =>
                  setJiraForm((prev) => ({
                    ...prev,
                    projectKey: e.target.value.toUpperCase(),
                  }))
                }
              />
            </div>
            <div>
              <label className="text-[10px] text-gx-muted block mb-0.5">
                Authentication
              </label>
              <div className="flex gap-3">
                <label className="flex items-center gap-1.5 cursor-pointer">
                  <input
                    type="radio"
                    name="jira-auth"
                    value="oauth"
                    checked={jiraForm.authType === "oauth"}
                    onChange={() =>
                      setJiraForm((prev) => ({ ...prev, authType: "oauth" }))
                    }
                    className="accent-gx-accent"
                  />
                  <span className="text-xs text-gx-text">OAuth 2.0</span>
                </label>
                <label className="flex items-center gap-1.5 cursor-pointer">
                  <input
                    type="radio"
                    name="jira-auth"
                    value="pat"
                    checked={jiraForm.authType === "pat"}
                    onChange={() =>
                      setJiraForm((prev) => ({ ...prev, authType: "pat" }))
                    }
                    className="accent-gx-accent"
                  />
                  <span className="text-xs text-gx-text">Personal Access Token</span>
                </label>
              </div>
              <p className="mt-1 text-[10px] text-gx-muted">
                {jiraForm.authType === "oauth"
                  ? "OAuth 2.0 (3LO) for Jira Cloud — opens browser for authorization"
                  : "PAT for Jira Server/Data Center — stored securely in OS keychain"}
              </p>
            </div>
          </div>
        )}

        {/* Linear */}
        {trackerType === "linear" && (
          <div className="space-y-2">
            <div>
              <label className="text-[10px] text-gx-muted block mb-0.5">
                Project Slug
              </label>
              <input
                type="text"
                className="input-field w-full"
                placeholder="ENG"
                value={linearForm.projectSlug}
                onChange={(e) =>
                  setLinearForm({ projectSlug: e.target.value })
                }
              />
              <p className="mt-0.5 text-[10px] text-gx-muted">
                Linear authentication is resolved from WORKFLOW.md $ENV references or LINEAR_API_KEY.
              </p>
            </div>
          </div>
        )}

        {/* Connect button + status */}
        <div className="mt-3 flex items-center gap-2">
          <button
            type="button"
            className="btn-primary flex items-center gap-1.5 text-xs px-3 py-1.5"
            onClick={handleConnect}
            disabled={connecting}
          >
            {connecting ? (
              <Loader2 size={12} className="animate-spin" />
            ) : connectSuccess ? (
              <Check size={12} />
            ) : (
              <ExternalLink size={12} />
            )}
            {connecting ? "Connecting…" : connectSuccess ? "Connected" : "Connect"}
          </button>
          {connectError && (
            <span className="text-[11px] text-gx-error">{connectError}</span>
          )}
          {connectSuccess && (
            <span className="text-[11px] text-gx-success">
              Successfully connected
            </span>
          )}
        </div>
      </section>

      {/* ── Filter Criteria ────────────────────────────────── */}
      <section>
        <h3 className="text-xs font-medium text-gx-text mb-2">
          Filter Criteria
        </h3>
        <p className="text-[10px] text-gx-muted mb-2">
          Comma-separated values. Leave empty to include all.
        </p>
        <div className="space-y-2">
          <div>
            <label className="text-[10px] text-gx-muted block mb-0.5">
              Labels (include)
            </label>
            <input
              type="text"
              className="input-field w-full"
              placeholder="bug, feature, enhancement"
              value={filterForm.labels}
              onChange={(e) => handleFilterChange("labels", e.target.value)}
            />
          </div>
          <div>
            <label className="text-[10px] text-gx-muted block mb-0.5">
              Assignees
            </label>
            <input
              type="text"
              className="input-field w-full"
              placeholder="username1, username2"
              value={filterForm.assignees}
              onChange={(e) => handleFilterChange("assignees", e.target.value)}
            />
          </div>
          <div>
            <label className="text-[10px] text-gx-muted block mb-0.5">
              Milestones
            </label>
            <input
              type="text"
              className="input-field w-full"
              placeholder="v1.0, Sprint 5"
              value={filterForm.milestones}
              onChange={(e) => handleFilterChange("milestones", e.target.value)}
            />
          </div>
          <div>
            <label className="text-[10px] text-gx-muted block mb-0.5">
              Exclude Labels
            </label>
            <input
              type="text"
              className="input-field w-full"
              placeholder="wontfix, duplicate"
              value={filterForm.excludeLabels}
              onChange={(e) => handleFilterChange("excludeLabels", e.target.value)}
            />
          </div>
        </div>
      </section>
    </div>
  );
}
