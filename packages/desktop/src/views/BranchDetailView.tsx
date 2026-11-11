import { Button } from "@/components/ui/button";
import { useEffect, useState } from "react";
import clsx from "clsx";
import {
  ArrowDownToLine,
  ArrowUpFromLine,
  Check,
  Clock,
  FileDiff,
  GitBranch,
  GitMerge,
  Loader2,
  RotateCcw,
  Trash2,
  Upload,
} from "lucide-react";
import { OpenRepo } from "../state";
import { mainProcess, rpc } from "../ipc";

interface Props {
  repo: OpenRepo;
  branchName: string;
  /** Navigate to the commit timeline with a specific commit. */
  onSelectCommit?: (sha: string) => void;
}

interface BranchInfo {
  branch: string;
  is_current: boolean;
  upstream: string | null;
  ahead: number;
  behind: number;
  upstream_ahead: number;
  upstream_behind: number;
  base: string;
  commits: Array<{ sha: string; author: string; date: string; subject: string }>;
  changed_files: Array<{ path: string; additions: number; deletions: number }>;
  total_additions: number;
  total_deletions: number;
  created_at: string | null;
}

/**
 * Branch detail panel — shown in the main area when a branch is
 * selected in the Branches sidebar. Gives at-a-glance context:
 *
 *   - Branch name + current/tracking status
 *   - Ahead/behind vs HEAD and upstream
 *   - Quick actions (checkout, merge, rebase, delete, push)
 *   - Recent commits on the branch
 *   - Files changed vs base (diffstat summary)
 */
export function BranchDetailView({ repo, branchName, onSelectCommit }: Props) {
  const [info, setInfo] = useState<BranchInfo | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [actionMsg, setActionMsg] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setErr(null);
    setInfo(null);
    setActionMsg(null);
    rpc
      .gitBranchInfo(repo.repo_id, branchName)
      .then((d) => { if (!cancelled) setInfo(d); })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [repo.repo_id, branchName]);

  const runAction = async (label: string, fn: () => Promise<unknown>) => {
    setActionMsg(null);
    try {
      await fn();
      setActionMsg(`✓ ${label} succeeded`);
      // Refresh info.
      rpc.gitBranchInfo(repo.repo_id, branchName).then(setInfo).catch(() => {});
    } catch (e) {
      setActionMsg(`✗ ${label} failed: ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Loading branch info…
      </div>
    );
  }
  if (err) {
    return (
      <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{err}</div>
    );
  }
  if (!info) return null;

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mx-auto max-w-4xl space-y-5">
        {/* Header */}
        <div className="flex items-start justify-between gap-4">
          <div>
            <div className="flex items-center gap-2">
              <GitBranch size={18} className="text-primary" />
              <h1 className="text-xl font-semibold">{info.branch}</h1>
              {info.is_current && (
                <span className="flex items-center gap-1 rounded-full bg-success/20 px-2 py-0.5 text-[10px] font-medium text-success">
                  <Check size={10} /> current
                </span>
              )}
            </div>
            {info.upstream && (
              <div className="mt-1 flex items-center gap-2 text-[11px] text-muted-foreground">
                <Upload size={10} /> Tracks <code className="font-mono">{info.upstream}</code>
                {(info.upstream_ahead > 0 || info.upstream_behind > 0) && (
                  <span>
                    · {info.upstream_ahead > 0 && <span className="text-success">↑{info.upstream_ahead}</span>}
                    {info.upstream_behind > 0 && <span className="ml-1 text-destructive">↓{info.upstream_behind}</span>}
                  </span>
                )}
              </div>
            )}
            {!info.upstream && !info.is_current && (
              <div className="mt-1 text-[11px] text-muted-foreground">No upstream tracking configured</div>
            )}
          </div>
        </div>

        {/* Ahead/Behind vs base */}
        <div className="grid grid-cols-2 gap-3">
          <StatCard
            icon={<ArrowUpFromLine size={14} className="text-success" />}
            label={`Ahead of ${info.base}`}
            value={String(info.ahead)}
            hint={`${info.ahead} commit${info.ahead === 1 ? "" : "s"} not in ${info.base}`}
          />
          <StatCard
            icon={<ArrowDownToLine size={14} className="text-destructive" />}
            label={`Behind ${info.base}`}
            value={String(info.behind)}
            hint={`${info.behind} commit${info.behind === 1 ? "" : "s"} in ${info.base} not here`}
          />
        </div>

        {/* Quick actions */}
        <div className="flex flex-wrap gap-2">
          {!info.is_current && (
            <ActionButton
              icon={<Check size={12} />}
              label="Checkout"
              onClick={() => runAction("Checkout", () => rpc.gitCheckout(repo.repo_id, info.branch))}
            />
          )}
          {!info.is_current && (
            <ActionButton
              icon={<GitMerge size={12} />}
              label="Merge into current"
              onClick={() => runAction("Merge", () => rpc.gitMerge(repo.repo_id, info.branch, { no_ff: true }))}
            />
          )}
          {!info.is_current && (
            <ActionButton
              icon={<RotateCcw size={12} />}
              label="Rebase onto current"
              onClick={() => runAction("Rebase", () => rpc.gitRebase(repo.repo_id, info.branch))}
            />
          )}
          {!info.is_current && (
            <ActionButton
              icon={<Trash2 size={12} />}
              label="Delete"
              variant="danger"
              onClick={async () => {
                const ok = await mainProcess.dialogConfirm(`Delete branch "${info.branch}"? This cannot be undone.`);
                if (ok) {
                  void runAction("Delete", () => rpc.gitBranchDelete(repo.repo_id, info.branch));
                }
              }}
            />
          )}
          {info.upstream && (
            <ActionButton
              icon={<Upload size={12} />}
              label="Push"
              onClick={() => runAction("Push", () => rpc.gitPushBranch(repo.repo_id, info.branch))}
            />
          )}
        </div>

        {actionMsg && (
          <div className={clsx(
            "rounded border p-2 text-xs",
            actionMsg.startsWith("✓") ? "border-success/40 bg-success/10 text-success" : "border-destructive/40 bg-destructive/10 text-destructive",
          )}>
            {actionMsg}
          </div>
        )}

        {/* Changed files summary */}
        {info.changed_files.length > 0 && (
          <div className="rounded-lg border border-border bg-card p-4">
            <div className="mb-3 flex items-center justify-between text-[11px] text-muted-foreground">
              <span className="flex items-center gap-2">
                <FileDiff size={12} />
                {info.changed_files.length} file{info.changed_files.length === 1 ? "" : "s"} changed vs {info.base}
              </span>
              <span>
                <span className="font-semibold text-success">+{info.total_additions}</span>
                {" "}
                <span className="font-semibold text-destructive">−{info.total_deletions}</span>
              </span>
            </div>
            <ul className="max-h-[200px] space-y-0.5 overflow-auto text-[12px]">
              {info.changed_files.slice(0, 50).map((f) => (
                <li key={f.path} className="flex items-center gap-2 rounded px-2 py-1 hover:bg-accent">
                  <span className="min-w-0 flex-1 truncate font-mono">{f.path}</span>
                  <span className="shrink-0 text-[10px] text-success">+{f.additions}</span>
                  <span className="shrink-0 text-[10px] text-destructive">−{f.deletions}</span>
                </li>
              ))}
              {info.changed_files.length > 50 && (
                <li className="px-2 py-1 text-[10px] text-muted-foreground">
                  … and {info.changed_files.length - 50} more files
                </li>
              )}
            </ul>
          </div>
        )}

        {/* Recent commits */}
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="mb-3 flex items-center gap-2 text-[11px] text-muted-foreground">
            <Clock size={12} /> Recent commits ({info.commits.length})
          </div>
          {info.commits.length === 0 ? (
            <div className="text-xs text-muted-foreground">No commits found on this branch.</div>
          ) : (
            <ul className="space-y-0.5 text-[12px]">
              {info.commits.map((c) => (
                <li key={c.sha}>
                  <button
                    type="button"
                    onClick={() => onSelectCommit?.(c.sha)}
                    className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left transition hover:bg-accent"
                  >
                    <code className="shrink-0 font-mono text-[10px] text-muted-foreground">{c.sha.slice(0, 7)}</code>
                    <span className="min-w-0 flex-1 truncate">{c.subject}</span>
                    <span className="shrink-0 text-[10px] text-muted-foreground">{c.author}</span>
                    <span className="shrink-0 text-[10px] text-muted-foreground">{formatRelDate(c.date)}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>

        {info.created_at && (
          <div className="text-[11px] text-muted-foreground">
            Branch created around {new Date(info.created_at).toLocaleDateString()}
          </div>
        )}
      </div>
    </div>
  );
}

function StatCard({ icon, label, value, hint }: { icon: React.ReactNode; label: string; value: string; hint?: string }) {
  return (
    <div className="rounded-lg border border-border bg-card p-3">
      <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">{icon}{label}</div>
      <div className="mt-1 font-mono text-2xl text-foreground">{value}</div>
      {hint && <div className="mt-0.5 text-[10px] text-muted-foreground">{hint}</div>}
    </div>
  );
}

function ActionButton({ icon, label, variant, onClick }: {
  icon: React.ReactNode;
  label: string;
  variant?: "danger";
  onClick: () => void;
}) {
  return (
    <Button variant="outline"
      type="button"
      onClick={onClick}
      className={clsx(
 " text-xs",
 variant === "danger" && "border-destructive/40 text-destructive hover:bg-destructive/10",
 )}
    >
      {icon} {label}
    </Button>
  );
}

function formatRelDate(iso: string): string {
  try {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const delta = (Date.now() - d.getTime()) / 1000;
    if (delta < 3600) return `${Math.floor(delta / 60)}m`;
    if (delta < 86400) return `${Math.floor(delta / 3600)}h`;
    if (delta < 604800) return `${Math.floor(delta / 86400)}d`;
    return d.toLocaleDateString();
  } catch { return iso; }
}
