import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
/**
 * IssueSelector — browse and select GitHub/Linear issues for task creation.
 *
 * Fetches issues via IPC handlers (`orchestrator:fetch-issues` for GitHub/GitLab,
 * `orchestrator:fetch-linear-issues` for Linear) and displays them in a
 * searchable, filterable list.
 */

import { ExternalLink, Loader2, Search, Tag, User } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

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
      <div className="flex items-center gap-2 rounded-md border border-border bg-background px-3 py-4 text-xs text-muted-foreground">
        <Loader2 size={14} className="animate-spin" />
        <span>
          Fetching {provider === "linear" ? "Linear" : "GitHub"} issues…
        </span>
      </div>
    );
  }

  if (fetchState === "auth-required") {
    return (
      <div className="rounded-md border border-border bg-background px-3 py-4 text-xs">
        <p className="text-muted-foreground">
          {provider === "linear"
            ? "Linear API token required. Add it in Settings → Integrations."
            : "GitHub authentication required. Connect via Settings → Forge."}
        </p>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={fetchIssues}
          className="mt-2 text-[11px] text-primary hover:underline"
        >
          Retry
        </Button>
      </div>
    );
  }

  if (fetchState === "error") {
    return (
      <div className="rounded-md border border-border bg-background px-3 py-4 text-xs">
        <p className="text-destructive">
          {errorMessage || "Failed to fetch issues"}
        </p>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={fetchIssues}
          className="mt-2 text-[11px] text-primary hover:underline"
        >
          Retry
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-2">
      {/* Search bar */}
      <div className="relative">
        <Search size={12} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-muted-foreground" />
        <Input
          type="text"
          placeholder="Search issues by title or number…"
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          className="w-full rounded-md border border-border bg-background pl-7 pr-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
        />
      </div>

      {/* Filter toggles */}
      <div className="flex items-center gap-2">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setShowFilters(!showFilters)}
          className="flex items-center gap-1 text-[10px] text-muted-foreground hover:text-foreground"
        >
          <Tag size={10} />
          Filters {showFilters ? "▾" : "▸"}
        </Button>
        {issues.length > 0 && (
          <span className="text-[10px] text-muted-foreground">
            {filteredIssues.length} of {issues.length} issues
          </span>
        )}
      </div>

      {/* Expanded filters */}
      {showFilters && (
        <div className="flex gap-2">
          <div className="flex-1">
            <div className="flex items-center gap-1 mb-0.5">
              <Tag size={9} className="text-muted-foreground" />
              <span className="text-[10px] text-muted-foreground">Label</span>
            </div>
            <Input
              type="text"
              placeholder="e.g. bug, enhancement"
              value={labelFilter}
              onChange={(e) => setLabelFilter(e.target.value)}
              className="w-full rounded border border-border bg-background px-2 py-1 text-[11px] text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
            />
          </div>
          <div className="flex-1">
            <div className="flex items-center gap-1 mb-0.5">
              <User size={9} className="text-muted-foreground" />
              <span className="text-[10px] text-muted-foreground">
                Assignee
              </span>
            </div>
            <Input
              type="text"
              placeholder="e.g. octocat"
              value={assigneeFilter}
              onChange={(e) => setAssigneeFilter(e.target.value)}
              className="w-full rounded border border-border bg-background px-2 py-1 text-[11px] text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
            />
          </div>
        </div>
      )}

      {/* Issue list */}
      <div className="max-h-48 overflow-y-auto rounded-md border border-border bg-background">
        {filteredIssues.length === 0 ? (
          <div className="px-3 py-4 text-center text-[11px] text-muted-foreground">
            {issues.length === 0 ? "No open issues found" : "No issues match your filters"}
          </div>
        ) : (
          <ul className="divide-y divide-border">
            {filteredIssues.map((issue) => (
              <li key={issue.number}>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  onClick={() => onSelect(issue)}
                  className={`w-full px-3 py-2 text-left transition hover:bg-primary/10 ${
                    selectedIssueNumber === issue.number ? "bg-primary/15" : ""
                  }`}
                >
                  <div className="flex items-start gap-2">
                    <span className="shrink-0 text-[10px] font-mono text-muted-foreground">
                      #{issue.number}
                    </span>
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-xs text-foreground">
                        {issue.title}
                      </p>
                      <div className="mt-0.5 flex items-center gap-2">
                        {issue.author && (
                          <span className="text-[10px] text-muted-foreground">
                            {issue.author}
                          </span>
                        )}
                        {issue.labels.length > 0 && (
                          <div className="flex gap-1">
                            {issue.labels.slice(0, 3).map((label) => (
                              <span
                                key={label}
                                className="rounded-full bg-primary/20 px-1.5 py-0.5 text-[9px] text-primary"
                              >
                                {label}
                              </span>
                            ))}
                            {issue.labels.length > 3 && (
                              <span className="text-[9px] text-muted-foreground">
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
                      className="shrink-0 text-muted-foreground hover:text-primary"
                      aria-label="Open issue in browser"
                    >
                      <ExternalLink size={10} />
                    </a>
                  </div>
                </Button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
