/**
 * IssueSelector — browse and select GitHub/Linear issues for task creation.
 *
 * Fetches issues via IPC handlers (`orchestrator:fetch-issues` for GitHub/GitLab,
 * `orchestrator:fetch-linear-issues` for Linear) and displays them in a
 * searchable, filterable list.
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { Loader2, Search, Tag, User, ExternalLink } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

export interface IssueItem {
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

export interface IssueRef {
  provider: "github" | "gitlab" | "linear";
  number: number;
  url: string;
}

interface Props {
  provider: "github" | "linear";
  onSelect: (issue: IssueItem) => void;
  selectedIssueNumber?: number;
}

type FetchState = "idle" | "loading" | "loaded" | "error" | "auth-required";

// ── Component ───────────────────────────────────────────────────────

export function IssueSelector({ provider, onSelect, selectedIssueNumber }: Props) {
  const [issues, setIssues] = useState<IssueItem[]>([]);
  const [fetchState, setFetchState] = useState<FetchState>("idle");
  const [errorMessage, setErrorMessage] = useState("");

  // Filters
  const [searchQuery, setSearchQuery] = useState("");
  const [labelFilter, setLabelFilter] = useState("");
  const [assigneeFilter, setAssigneeFilter] = useState("");
  const [showFilters, setShowFilters] = useState(false);

  // Fetch issues on mount or when provider changes.
  useEffect(() => {
    fetchIssues();
  }, [provider]);

  const fetchIssues = useCallback(async () => {
    setFetchState("loading");
    setErrorMessage("");

    try {
      const result = await (window as any).crab.orchestrator.fetchIssues({
        provider,
      });

      if (result?.authRequired) {
        setFetchState("auth-required");
        return;
      }

      if (!result?.success) {
        setFetchState("error");
        setErrorMessage(result?.error ?? "Failed to fetch issues");
        return;
      }

      setIssues(result.issues ?? []);
      setFetchState("loaded");
    } catch (err) {
      setFetchState("error");
      setErrorMessage(err instanceof Error ? err.message : "Failed to fetch issues");
    }
  }, [provider]);

  // Filter issues client-side.
  const filteredIssues = useMemo(() => {
    let result = issues;

    if (searchQuery.trim()) {
      const q = searchQuery.toLowerCase();
      result = result.filter(
        (issue) =>
          issue.title.toLowerCase().includes(q) ||
          `#${issue.number}`.includes(q),
      );
    }

    if (labelFilter.trim()) {
      const label = labelFilter.toLowerCase();
      result = result.filter((issue) =>
        issue.labels.some((l) => l.toLowerCase().includes(label)),
      );
    }

    if (assigneeFilter.trim()) {
      const assignee = assigneeFilter.toLowerCase();
      result = result.filter(
        (issue) => issue.assignee?.toLowerCase().includes(assignee) ?? false,
      );
    }

    return result;
  }, [issues, searchQuery, labelFilter, assigneeFilter]);

  // ── Render states ─────────────────────────────────────────────────

  if (fetchState === "loading") {
    return (
      <div className="flex items-center gap-2 rounded-md border border-gx-border bg-gx-bg-sub px-3 py-4 text-xs text-gx-muted">
        <Loader2 size={14} className="animate-spin" />
        <span>Fetching {provider === "linear" ? "Linear" : "GitHub"} issues…</span>
      </div>
    );
  }

  if (fetchState === "auth-required") {
    return (
      <div className="rounded-md border border-gx-border bg-gx-bg-sub px-3 py-4 text-xs">
        <p className="text-gx-muted">
          {provider === "linear"
            ? "Linear API token required. Add it in Settings → Integrations."
            : "GitHub authentication required. Connect via Settings → Forge."}
        </p>
        <button
          type="button"
          onClick={fetchIssues}
          className="mt-2 text-[11px] text-gx-accent hover:underline"
        >
          Retry
        </button>
      </div>
    );
  }

  if (fetchState === "error") {
    return (
      <div className="rounded-md border border-gx-border bg-gx-bg-sub px-3 py-4 text-xs">
        <p className="text-gx-error">{errorMessage || "Failed to fetch issues"}</p>
        <button
          type="button"
          onClick={fetchIssues}
          className="mt-2 text-[11px] text-gx-accent hover:underline"
        >
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-2">
      {/* Search bar */}
      <div className="relative">
        <Search size={12} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-gx-muted" />
        <input
          type="text"
          placeholder="Search issues by title or number…"
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          className="w-full rounded-md border border-gx-border bg-gx-bg-sub pl-7 pr-3 py-1.5 text-xs text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
        />
      </div>

      {/* Filter toggles */}
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => setShowFilters(!showFilters)}
          className="flex items-center gap-1 text-[10px] text-gx-muted hover:text-gx-text"
        >
          <Tag size={10} />
          Filters {showFilters ? "▾" : "▸"}
        </button>
        {issues.length > 0 && (
          <span className="text-[10px] text-gx-muted">
            {filteredIssues.length} of {issues.length} issues
          </span>
        )}
      </div>

      {/* Expanded filters */}
      {showFilters && (
        <div className="flex gap-2">
          <div className="flex-1">
            <div className="flex items-center gap-1 mb-0.5">
              <Tag size={9} className="text-gx-muted" />
              <span className="text-[10px] text-gx-muted">Label</span>
            </div>
            <input
              type="text"
              placeholder="e.g. bug, enhancement"
              value={labelFilter}
              onChange={(e) => setLabelFilter(e.target.value)}
              className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-[11px] text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
            />
          </div>
          <div className="flex-1">
            <div className="flex items-center gap-1 mb-0.5">
              <User size={9} className="text-gx-muted" />
              <span className="text-[10px] text-gx-muted">Assignee</span>
            </div>
            <input
              type="text"
              placeholder="e.g. octocat"
              value={assigneeFilter}
              onChange={(e) => setAssigneeFilter(e.target.value)}
              className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-[11px] text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
            />
          </div>
        </div>
      )}

      {/* Issue list */}
      <div className="max-h-48 overflow-y-auto rounded-md border border-gx-border bg-gx-bg-sub">
        {filteredIssues.length === 0 ? (
          <div className="px-3 py-4 text-center text-[11px] text-gx-muted">
            {issues.length === 0 ? "No open issues found" : "No issues match your filters"}
          </div>
        ) : (
          <ul className="divide-y divide-gx-border">
            {filteredIssues.map((issue) => (
              <li key={issue.number}>
                <button
                  type="button"
                  onClick={() => onSelect(issue)}
                  className={`w-full px-3 py-2 text-left transition hover:bg-gx-accent/10 ${
                    selectedIssueNumber === issue.number ? "bg-gx-accent/15" : ""
                  }`}
                >
                  <div className="flex items-start gap-2">
                    <span className="shrink-0 text-[10px] font-mono text-gx-muted">
                      #{issue.number}
                    </span>
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-xs text-gx-text">{issue.title}</p>
                      <div className="mt-0.5 flex items-center gap-2">
                        {issue.author && (
                          <span className="text-[10px] text-gx-muted">{issue.author}</span>
                        )}
                        {issue.labels.length > 0 && (
                          <div className="flex gap-1">
                            {issue.labels.slice(0, 3).map((label) => (
                              <span
                                key={label}
                                className="rounded-full bg-gx-accent/20 px-1.5 py-0.5 text-[9px] text-gx-accent"
                              >
                                {label}
                              </span>
                            ))}
                            {issue.labels.length > 3 && (
                              <span className="text-[9px] text-gx-muted">
                                +{issue.labels.length - 3}
                              </span>
                            )}
                          </div>
                        )}
                      </div>
                    </div>
                    <a
                      href={issue.url}
                      target="_blank"
                      rel="noopener noreferrer"
                      onClick={(e) => e.stopPropagation()}
                      className="shrink-0 text-gx-muted hover:text-gx-accent"
                      aria-label="Open issue in browser"
                    >
                      <ExternalLink size={10} />
                    </a>
                  </div>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
