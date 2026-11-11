/**
 * Checks Tab — Fifth workspace tab aggregating merge readiness signals.
 *
 * Sections: Git status, PR info, CI checks, comments, todos, merge button.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import {
  AlertTriangle,
  Check,
  CheckCircle2,
  Circle,
  Clock,
  ExternalLink,
  GitBranch,
  GitMerge,
  GitPullRequest,
  Loader2,
  MessageSquare,
  RefreshCw,
  Sparkles,
  XCircle,
} from "lucide-react";
import { OrchestratorTask } from "./TaskCard";
import { TodoItem, TodoList } from "./TodoList";

// ── Types ───────────────────────────────────────────────────────────

interface CICheck {
  name: string;
  status: "pass" | "fail" | "pending" | "queued";
  conclusion?: string;
  durationMs?: number;
  url?: string;
  output?: string;
}

interface PRInfo {
  title: string;
  number: number;
  url: string;
  state: string;
  reviewers: Array<{ login: string; state: string }>;
  mergeable: boolean;
  mergeStateStatus: string;
}

interface GitStatusInfo {
  clean: boolean;
  ahead: number;
  behind: number;
  conflicts: string[];
}

interface CommentInfo {
  id: string;
  body: string;
  path?: string;
  line?: number;
  resolved: boolean;
}

interface ChecksData {
  gitStatus: GitStatusInfo;
  pr?: PRInfo;
  checks: CICheck[];
  comments: CommentInfo[];
}

interface Props {
  task: OrchestratorTask;
  prMetadata?: { url: string; number: number } | null;
  todos?: TodoItem[];
  onToggleTodo?: (todoId: string) => void;
  onAddTodo?: (text: string, source?: "user" | "agent" | "comment") => void;
  onRemoveTodo?: (todoId: string) => void;
  onMerge?: () => void;
  onFixWithAgent?: (checkName: string, output: string) => void;
}

// ── Component ───────────────────────────────────────────────────────

export function ChecksTab({
  task,
  prMetadata,
  todos = [],
  onToggleTodo,
  onAddTodo,
  onRemoveTodo,
  onMerge,
  onFixWithAgent,
}: Props) {
  const [checksData, setChecksData] = useState<ChecksData | null>(null);
  const [loading, setLoading] = useState(false);
  const [merging, setMerging] = useState(false);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchChecks = useCallback(async () => {
    try {
      setLoading(true);
      const result = await (window as any).crab.orchestrator?.getChecks(task.id);
      if (result?.success && result.data) {
        setChecksData(result.data);
      }
    } catch {
      // Best-effort
    } finally {
      setLoading(false);
    }
  }, [task.id]);

  // Initial fetch + polling every 30s
  useEffect(() => {
    fetchChecks();
    pollRef.current = setInterval(fetchChecks, 30_000);
    return () => {
      if (pollRef.current) clearInterval(pollRef.current);
    };
  }, [fetchChecks]);

  const handleMerge = useCallback(async () => {
    if (!onMerge) return;
    setMerging(true);
    try {
      onMerge();
    } finally {
      setMerging(false);
    }
  }, [onMerge]);

  // Compute blockers
  const incompleteTodos = todos.filter((t) => !t.done);
  const unresolvedComments = checksData?.comments.filter((c) => !c.resolved) ?? [];
  const failingChecks = checksData?.checks.filter((c) => c.status === "fail") ?? [];
  const pendingChecks = checksData?.checks.filter((c) => c.status === "pending" || c.status === "queued") ?? [];
  const hasConflicts = (checksData?.gitStatus.conflicts.length ?? 0) > 0;
  const allChecksPassed = checksData?.checks.every((c) => c.status === "pass") ?? false;
  const canMerge =
    prMetadata &&
    incompleteTodos.length === 0 &&
    unresolvedComments.length === 0 &&
    failingChecks.length === 0 &&
    pendingChecks.length === 0 &&
    !hasConflicts &&
    allChecksPassed;

  return (
    <div className="flex h-full flex-col overflow-auto">
      <div className="flex-1 space-y-4 p-4">
        {/* Git Status Section */}
        <Section title="Git Status" icon={<GitBranch size={13} />}>
          {checksData?.gitStatus ? (
            <div className="space-y-1.5">
              <StatusRow
                ok={checksData.gitStatus.clean}
                label={checksData.gitStatus.clean ? "Working tree clean" : "Working tree has uncommitted changes"}
              />
              <StatusRow
                ok={checksData.gitStatus.ahead > 0}
                neutral={checksData.gitStatus.ahead === 0}
                label={`${checksData.gitStatus.ahead} commit${checksData.gitStatus.ahead !== 1 ? "s" : ""} ahead, ${checksData.gitStatus.behind} behind base`}
              />
              <StatusRow
                ok={!hasConflicts}
                label={hasConflicts ? `${checksData.gitStatus.conflicts.length} conflict(s) detected` : "No conflicts"}
              />
            </div>
          ) : (
            <LoadingPlaceholder />
          )}
        </Section>

        {/* PR Section */}
        {prMetadata && (
          <Section title="Pull Request" icon={<GitPullRequest size={13} />}>
            {checksData?.pr ? (
              <div className="space-y-2">
                <div className="flex items-center gap-2">
                  <span className="text-xs font-medium text-foreground">
                    #{checksData.pr.number} {checksData.pr.title}
                  </span>
                  <a
                    href={checksData.pr.url}
                    onClick={(e) => {
                      e.preventDefault();
                      (window as any).crab?.openUrl(checksData.pr!.url);
                    }}
                    className="text-primary hover:underline"
                    aria-label="Open PR in browser"
                  >
                    <ExternalLink size={11} />
                  </a>
                </div>
                {checksData.pr.reviewers.length > 0 && (
                  <div className="flex flex-wrap gap-2">
                    {checksData.pr.reviewers.map((r) => (
                      <span
                        key={r.login}
                        className={clsx(
                          "rounded-full px-2 py-0.5 text-[10px]",
                          r.state === "APPROVED" && "bg-success/15 text-success",
                          r.state === "CHANGES_REQUESTED" && "bg-destructive/15 text-destructive",
                          r.state !== "APPROVED" && r.state !== "CHANGES_REQUESTED" && "bg-card text-muted-foreground",
                        )}
                      >
                        {r.login}: {r.state.toLowerCase().replace("_", " ")}
                      </span>
                    ))}
                  </div>
                )}
                <StatusRow
                  ok={checksData.pr.mergeable}
                  label={checksData.pr.mergeable ? "Mergeable" : "Not mergeable"}
                />
              </div>
            ) : (
              <LoadingPlaceholder />
            )}
          </Section>
        )}

        {/* CI Section */}
        <Section title="CI Checks" icon={<CheckCircle2 size={13} />}>
          {checksData ? (
            checksData.checks.length === 0 ? (
              <p className="text-xs text-muted-foreground">No CI checks configured</p>
            ) : (
              <div className="space-y-1.5">
                {checksData.checks.map((check) => (
                  <div key={check.name} className="flex items-center gap-2">
                    <CheckStatusIcon status={check.status} />
                    <span className="flex-1 truncate text-xs text-foreground">{check.name}</span>
                    {check.durationMs != null && (
                      <span className="text-[10px] text-muted-foreground">
                        {formatDuration(check.durationMs)}
                      </span>
                    )}
                    {check.status === "fail" && onFixWithAgent && (
                      <button
                        type="button"
                        onClick={() => onFixWithAgent(check.name, check.output ?? "")}
                        className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-primary hover:bg-primary/10 transition"
                        aria-label={`Fix ${check.name} with agent`}
                      >
                        <Sparkles size={10} /> Fix
                      </button>
                    )}
                    {check.url && (
                      <a
                        href={check.url}
                        onClick={(e) => {
                          e.preventDefault();
                          (window as any).crab?.openUrl(check.url!);
                        }}
                        className="text-muted-foreground hover:text-primary"
                        aria-label={`View ${check.name} details`}
                      >
                        <ExternalLink size={10} />
                      </a>
                    )}
                  </div>
                ))}
              </div>
            )
          ) : (
            <LoadingPlaceholder />
          )}
        </Section>

        {/* Comments Section */}
        <Section title="Comments" icon={<MessageSquare size={13} />}>
          {unresolvedComments.length === 0 ? (
            <p className="text-xs text-muted-foreground">No unresolved comments</p>
          ) : (
            <div className="space-y-1.5">
              <p className="text-xs text-warning">
                {unresolvedComments.length} unresolved comment{unresolvedComments.length !== 1 ? "s" : ""}
              </p>
              {unresolvedComments.slice(0, 5).map((comment) => (
                <div key={comment.id} className="rounded border border-border/50 bg-background px-2 py-1.5">
                  <div className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
                    {comment.path && <span className="font-mono">{comment.path}</span>}
                    {comment.line && <span>:{comment.line}</span>}
                  </div>
                  <p className="mt-0.5 text-xs text-foreground line-clamp-2">{comment.body}</p>
                </div>
              ))}
              {unresolvedComments.length > 5 && (
                <p className="text-[10px] text-muted-foreground">
                  +{unresolvedComments.length - 5} more
                </p>
              )}
            </div>
          )}
        </Section>

        {/* Todos Section */}
        <Section title="Todos" icon={<Check size={13} />}>
          {todos.length === 0 ? (
            <p className="text-xs text-muted-foreground">No todos</p>
          ) : (
            <div>
              {incompleteTodos.length > 0 && (
                <p className="mb-2 text-xs text-warning">
                  {incompleteTodos.length} incomplete todo{incompleteTodos.length !== 1 ? "s" : ""}
                </p>
              )}
              {onToggleTodo && onAddTodo && onRemoveTodo && (
                <TodoList
                  taskId={task.id}
                  todos={todos}
                  onToggle={onToggleTodo}
                  onAdd={onAddTodo}
                  onRemove={onRemoveTodo}
                  compact
                />
              )}
            </div>
          )}
        </Section>
      </div>

      {/* Merge button + refresh */}
      <div className="shrink-0 border-t border-border bg-card px-4 py-3">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={handleMerge}
            disabled={!canMerge || merging}
            className={clsx(
              "flex flex-1 items-center justify-center gap-2 rounded-md px-4 py-2 text-xs font-medium transition",
              canMerge
                ? "bg-success text-success-foreground hover:bg-success"
                : "bg-card border border-border text-muted-foreground cursor-not-allowed",
            )}
            aria-label="Merge pull request"
          >
            {merging ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <GitMerge size={13} />
            )}
            {canMerge ? "Merge PR" : "Merge blocked"}
          </button>
          <button
            type="button"
            onClick={fetchChecks}
            disabled={loading}
            className="flex items-center gap-1 rounded-md border border-border px-3 py-2 text-xs text-muted-foreground hover:text-foreground transition"
            aria-label="Refresh checks"
          >
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
          </button>
        </div>
        {!canMerge && (
          <div className="mt-2 space-y-1">
            {hasConflicts && <BlockerLine text="Conflicts must be resolved" />}
            {failingChecks.length > 0 && <BlockerLine text={`${failingChecks.length} CI check(s) failing`} />}
            {pendingChecks.length > 0 && <BlockerLine text={`${pendingChecks.length} CI check(s) pending`} />}
            {unresolvedComments.length > 0 && <BlockerLine text={`${unresolvedComments.length} unresolved comment(s)`} />}
            {incompleteTodos.length > 0 && <BlockerLine text={`${incompleteTodos.length} incomplete todo(s)`} />}
            {!prMetadata && <BlockerLine text="No PR created yet" />}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Sub-components ──────────────────────────────────────────────────

function Section({ title, icon, children }: { title: string; icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border bg-card p-3">
      <div className="mb-2 flex items-center gap-1.5 text-xs font-medium text-foreground">
        {icon}
        {title}
      </div>
      {children}
    </div>
  );
}

function StatusRow({ ok, neutral, label }: { ok: boolean; neutral?: boolean; label: string }) {
  return (
    <div className="flex items-center gap-2 text-xs">
      {neutral ? (
        <Circle size={11} className="text-muted-foreground" />
      ) : ok ? (
        <CheckCircle2 size={11} className="text-success" />
      ) : (
        <XCircle size={11} className="text-destructive" />
      )}
      <span className={ok || neutral ? "text-foreground" : "text-destructive"}>{label}</span>
    </div>
  );
}

function CheckStatusIcon({ status }: { status: CICheck["status"] }) {
  switch (status) {
    case "pass":
      return <CheckCircle2 size={12} className="text-success shrink-0" />;
    case "fail":
      return <XCircle size={12} className="text-destructive shrink-0" />;
    case "pending":
      return <Loader2 size={12} className="text-warning animate-spin shrink-0" />;
    case "queued":
      return <Clock size={12} className="text-muted-foreground shrink-0" />;
  }
}

function BlockerLine({ text }: { text: string }) {
  return (
    <div className="flex items-center gap-1.5 text-[10px] text-warning">
      <AlertTriangle size={10} />
      {text}
    </div>
  );
}

function LoadingPlaceholder() {
  return (
    <div className="flex items-center gap-2 text-xs text-muted-foreground">
      <Loader2 size={11} className="animate-spin" />
      Loading…
    </div>
  );
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}
