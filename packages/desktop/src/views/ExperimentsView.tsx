import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  ExternalLink,
  FlaskConical,
  Loader2,
  RefreshCw,
  Settings,
} from "lucide-react";
import { OpenRepo } from "../state";
import {
  ExperimentRun,
  ExperimentRunDetail,
  ExperimentRunStatus,
  rpc,
} from "../ipc";

interface Props {
  repo: OpenRepo;
  onNavigateToCommit?: (sha: string) => void;
  onNavigateToSettings?: () => void;
}

// ---- Status chip colours ----

const STATUS_STYLES: Record<ExperimentRunStatus, { bg: string; text: string; pulse?: boolean }> = {
  running: { bg: "bg-info/20", text: "text-info", pulse: true },
  completed: { bg: "bg-success/20", text: "text-success" },
  failed: { bg: "bg-destructive/20", text: "text-destructive" },
  killed: { bg: "bg-warning/20", text: "text-warning" },
  unknown: { bg: "bg-accent/20", text: "text-muted-foreground" },
};

function StatusChip({ status }: { status: ExperimentRunStatus }) {
  const style = STATUS_STYLES[status] ?? STATUS_STYLES.unknown;
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1 rounded-full px-1.5 py-[1px] text-[10px] font-medium capitalize",
        style.bg,
        style.text,
      )}
    >
      {style.pulse && (
        <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-current" />
      )}
      {status}
    </span>
  );
}

// ---- Helpers ----

function formatTime(iso: string): string {
  try {
    const d = new Date(iso);
    const now = Date.now();
    const diffMs = now - d.getTime();
    if (diffMs < 60_000) return "just now";
    if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
    if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
    if (diffMs < 604_800_000) return `${Math.floor(diffMs / 86_400_000)}d ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}

function formatMetricValue(v: number): string {
  if (Number.isInteger(v)) return String(v);
  if (Math.abs(v) < 0.001) return v.toExponential(2);
  return v.toFixed(4);
}

function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

// ---- Main view ----

/**
 * Experiment tracking sidebar. Shows recent experiment runs from
 * MLflow/W&B linked to commits. Click a run to expand its detail;
 * click a commit SHA to navigate to that commit in the Timeline.
 */
export function ExperimentsView({ repo, onNavigateToCommit, onNavigateToSettings }: Props) {
  const [runs, setRuns] = useState<ExperimentRun[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notConfigured, setNotConfigured] = useState(false);
  const [expandedRunId, setExpandedRunId] = useState<string | null>(null);

  const fetchRuns = useCallback(() => {
    setLoading(true);
    setError(null);
    setNotConfigured(false);
    rpc
      .experimentsRunsRecent(repo.repo_id, 50)
      .then((res) => {
        setRuns(res.runs);
        setLoading(false);
      })
      .catch((e) => {
        const msg = e instanceof Error ? e.message : String(e);
        // The agent returns a specific error when no provider is configured,
        // or "Method not found" when running a stale agent binary that
        // doesn't have the experiments verbs yet.
        if (
          msg.includes("not_configured") ||
          msg.includes("no provider") ||
          msg.includes("Method not found") ||
          msg.includes("unknown method") ||
          msg.includes("method not")
        ) {
          setNotConfigured(true);
          setLoading(false);
        } else {
          setError(msg);
          setLoading(false);
        }
      });
  }, [repo.repo_id]);

  useEffect(() => {
    fetchRuns();
  }, [fetchRuns]);

  const toggleExpand = (runId: string) => {
    setExpandedRunId((prev) => (prev === runId ? null : runId));
  };

  // ---- Not configured empty state ----
  if (notConfigured) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
        <FlaskConical size={32} className="text-muted-foreground" />
        <div className="text-sm text-foreground">No experiment provider configured</div>
        <p className="text-xs text-muted-foreground">
          Connect MLflow or Weights &amp; Biases to see experiment runs linked to your commits.
        </p>
        <button
          type="button"
          onClick={onNavigateToSettings}
          className="mt-2 flex items-center gap-1.5 rounded border border-border px-3 py-1.5 text-xs text-foreground hover:bg-accent"
        >
          <Settings size={12} />
          Open Settings
        </button>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* Header */}
      <div className="shrink-0 border-b border-border px-3 py-2">
        <div className="flex items-center justify-between text-[10px] uppercase tracking-wider text-muted-foreground">
          <span className="flex items-center gap-1">
            <FlaskConical size={11} /> Experiments
          </span>
          <button
            type="button"
            className="text-muted-foreground hover:text-foreground"
            onClick={fetchRuns}
            title="Refresh"
            aria-label="Refresh experiment runs"
          >
            <RefreshCw size={11} className={loading ? "animate-spin" : ""} />
          </button>
        </div>
      </div>

      {/* Content */}
      <div className="min-h-0 flex-1 overflow-auto">
        {loading && !runs && (
          <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Loading runs…
          </div>
        )}
        {error && (
          <div className="m-3 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {error}
          </div>
        )}
        {!loading && !error && runs && runs.length === 0 && (
          <div className="p-3 text-xs text-muted-foreground">No experiment runs found.</div>
        )}
        {runs && runs.length > 0 && (
          <ul className="py-1">
            {runs.map((run) => (
              <RunRow
                key={run.id}
                run={run}
                expanded={expandedRunId === run.id}
                onToggle={() => toggleExpand(run.id)}
                repoId={repo.repo_id}
                onNavigateToCommit={onNavigateToCommit}
              />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

// ---- Run row ----

function RunRow({
  run,
  expanded,
  onToggle,
  repoId,
  onNavigateToCommit,
}: {
  run: ExperimentRun;
  expanded: boolean;
  onToggle: () => void;
  repoId: string;
  onNavigateToCommit?: (sha: string) => void;
}) {
  // Summary metrics: show up to 3 key metrics inline
  const metricEntries = Object.entries(run.metrics).slice(0, 3);

  return (
    <li className="border-b border-border/50 last:border-b-0">
      <button
        type="button"
        onClick={onToggle}
        className={clsx(
          "flex w-full items-start gap-2 px-3 py-2 text-left text-xs transition",
          expanded
            ? "bg-primary/10"
            : "hover:bg-accent",
        )}
      >
        <span className="mt-0.5 shrink-0">
          {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate font-medium text-foreground">{run.name}</span>
            <StatusChip status={run.status} />
          </div>
          {metricEntries.length > 0 && (
            <div className="mt-0.5 flex flex-wrap gap-x-2 gap-y-0.5 text-[10px] text-muted-foreground">
              {metricEntries.map(([key, val]) => (
                <span key={key} className="font-mono">
                  {key}={formatMetricValue(val)}
                </span>
              ))}
            </div>
          )}
          <div className="mt-0.5 flex items-center gap-2 text-[10px] text-muted-foreground">
            <span>{formatTime(run.start_time)}</span>
            {run.commit_sha && (
              <button
                type="button"
                onClick={(e) => {
                  e.stopPropagation();
                  onNavigateToCommit?.(run.commit_sha!);
                }}
                className="font-mono text-primary hover:underline"
                title={`Go to commit ${run.commit_sha}`}
              >
                {shortSha(run.commit_sha)}
              </button>
            )}
          </div>
        </div>
      </button>

      {expanded && (
        <RunDetailPanel repoId={repoId} runId={run.id} url={run.url} />
      )}
    </li>
  );
}

// ---- Expandable detail panel ----

function RunDetailPanel({
  repoId,
  runId,
  url,
}: {
  repoId: string;
  runId: string;
  url: string;
}) {
  const [detail, setDetail] = useState<ExperimentRunDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setLoading(true);
    setError(null);
    rpc
      .experimentsRunDetail(repoId, runId)
      .then((d) => {
        setDetail(d);
        setLoading(false);
      })
      .catch((e) => {
        setError(e instanceof Error ? e.message : String(e));
        setLoading(false);
      });
  }, [repoId, runId]);

  if (loading) {
    return (
      <div className="flex items-center gap-2 px-7 py-2 text-[11px] text-muted-foreground">
        <Loader2 size={10} className="animate-spin" /> Loading detail…
      </div>
    );
  }

  if (error) {
    return (
      <div className="px-7 py-2 text-[11px] text-destructive">{error}</div>
    );
  }

  if (!detail) return null;

  const metricEntries = Object.entries(detail.metrics);
  const paramEntries = Object.entries(detail.params);

  return (
    <div className="space-y-2 bg-background px-7 py-2 text-[11px]">
      {/* Metrics */}
      {metricEntries.length > 0 && (
        <DetailSection title="Metrics">
          <div className="grid grid-cols-2 gap-x-3 gap-y-0.5">
            {metricEntries.map(([key, val]) => (
              <div key={key} className="flex items-baseline justify-between gap-1">
                <span className="truncate text-muted-foreground">{key}</span>
                <span className="shrink-0 font-mono text-foreground">
                  {formatMetricValue(val)}
                </span>
              </div>
            ))}
          </div>
        </DetailSection>
      )}

      {/* Hyperparams */}
      {paramEntries.length > 0 && (
        <DetailSection title="Hyperparams">
          <div className="grid grid-cols-2 gap-x-3 gap-y-0.5">
            {paramEntries.map(([key, val]) => (
              <div key={key} className="flex items-baseline justify-between gap-1">
                <span className="truncate text-muted-foreground">{key}</span>
                <span className="shrink-0 font-mono text-foreground">{val}</span>
              </div>
            ))}
          </div>
        </DetailSection>
      )}

      {/* External link */}
      {url && (
        <a
          href={url}
          target="_blank"
          rel="noopener noreferrer"
          className="inline-flex items-center gap-1 text-[10px] text-primary hover:underline"
        >
          <ExternalLink size={10} /> Open in provider
        </a>
      )}
    </div>
  );
}

function DetailSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="mb-0.5 text-[10px] uppercase tracking-wider text-muted-foreground">{title}</div>
      {children}
    </div>
  );
}
