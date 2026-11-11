import { useCallback, useEffect, useState } from "react";
import { GitBranch, Loader2, RefreshCw, Upload } from "lucide-react";
import { OpenRepo } from "../state";
import { rpc } from "../ipc";

interface GitStatusInfo {
  branch: string;
  upstream: string | null;
  ahead: number;
  behind: number;
  staged: number;
  modified: number;
  untracked: number;
  entries: Array<{ path: string; code: string }>;
}

/**
 * Status tab in the Activity panel. Shows `git status` info:
 * current branch, tracking, staged/modified/untracked counts,
 * and the full file list. Works for any git repo — doesn't
 * require the crab CLI.
 */
export function StatusPage({ repo }: { repo: OpenRepo }) {
  const [info, setInfo] = useState<GitStatusInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // Get current branch.
      const configRes = await rpc.gitConfigGet(repo.repo_id, ["branch.current"]);
      void configRes; // unused for now

      // Get status entries.
      const statusRes = await rpc.gitStatus(repo.repo_id);
      const entries = statusRes.entries;

      let staged = 0, modified = 0, untracked = 0;
      for (const e of entries) {
        const code = e.code;
        if (code === "??") untracked++;
        else if (code[0] !== " " && code[0] !== "?") staged++;
        if (code[1] === "M" || code[1] === "D") modified++;
      }

      // Get branch info via resolve_ref.
      let branch = "HEAD";
      let upstream: string | null = null;
      let ahead = 0, behind = 0;
      try {
        const refs = await rpc.listRefs(repo.repo_id, { kinds: ["branch"] });
        const headRef = refs.refs.find(r => r.kind === "head");
        if (headRef) branch = headRef.name.replace(/^refs\/heads\//, "");
        // Find upstream.
        const remote = refs.refs.find(r => r.kind === "branch" && r.name === branch);
        void remote;
      } catch { /* non-fatal */ }

      try {
        const branchInfo = await rpc.gitBranchInfo(repo.repo_id, branch);
        upstream = branchInfo.upstream;
        ahead = branchInfo.upstream_ahead;
        behind = branchInfo.upstream_behind;
      } catch { /* non-fatal */ }

      setInfo({ branch, upstream, ahead, behind, staged, modified, untracked, entries });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [repo.repo_id]);

  useEffect(() => { void refresh(); }, [refresh]);

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-2 border-b border-gx-border bg-gx-bg px-3 py-2">
        <button type="button" className="btn text-xs" onClick={() => void refresh()} disabled={loading}>
          {loading ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />} Refresh
        </button>
        <button
          type="button"
          className="btn btn-primary text-xs"
          onClick={async () => {
            try { await rpc.gitPushBranch(repo.repo_id, info?.branch ?? "HEAD"); void refresh(); }
            catch (e) { setError(e instanceof Error ? e.message : String(e)); }
          }}
        >
          <Upload size={12} /> Push
        </button>
      </div>

      {error && (
        <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{error}</div>
      )}

      <div className="min-h-0 flex-1 overflow-auto p-3">
        {!info && !error && (
          <div className="flex items-center gap-2 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading status…
          </div>
        )}
        {info && (
          <div className="space-y-3">
            {/* Branch info */}
            <div className="flex items-center gap-3 rounded border border-gx-border bg-gx-surface p-3">
              <GitBranch size={14} className="text-gx-accent" />
              <div>
                <div className="font-mono text-[13px] font-semibold">{info.branch}</div>
                {info.upstream && (
                  <div className="text-[11px] text-gx-muted">
                    Tracks {info.upstream}
                    {info.ahead > 0 && <span className="ml-2 text-gx-success">↑{info.ahead}</span>}
                    {info.behind > 0 && <span className="ml-1 text-gx-error">↓{info.behind}</span>}
                  </div>
                )}
                {!info.upstream && <div className="text-[11px] text-gx-muted">No upstream</div>}
              </div>
            </div>

            {/* Summary */}
            <div className="grid grid-cols-3 gap-2 text-center">
              <div className="rounded border border-gx-border bg-gx-surface p-2">
                <div className="text-lg font-mono text-gx-success">{info.staged}</div>
                <div className="text-[10px] text-gx-muted">Staged</div>
              </div>
              <div className="rounded border border-gx-border bg-gx-surface p-2">
                <div className="text-lg font-mono text-gx-warn">{info.modified}</div>
                <div className="text-[10px] text-gx-muted">Modified</div>
              </div>
              <div className="rounded border border-gx-border bg-gx-surface p-2">
                <div className="text-lg font-mono text-gx-muted">{info.untracked}</div>
                <div className="text-[10px] text-gx-muted">Untracked</div>
              </div>
            </div>

            {/* File list */}
            {info.entries.length === 0 ? (
              <div className="text-center text-xs text-gx-muted">Working tree is clean ✓</div>
            ) : (
              <div className="rounded border border-gx-border bg-gx-surface">
                <ul className="max-h-[300px] overflow-auto text-[11px]">
                  {info.entries.map((e) => (
                    <li key={e.path} className="flex items-center gap-2 border-b border-gx-border/40 px-3 py-1 last:border-b-0">
                      <span className={`w-5 shrink-0 text-center font-mono font-bold ${statusColor(e.code)}`}>
                        {e.code.trim() || "·"}
                      </span>
                      <span className="min-w-0 flex-1 truncate font-mono">{e.path}</span>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

function statusColor(code: string): string {
  const c = code.trim();
  if (c === "??") return "text-gx-muted";
  if (c[0] === "A" || c[0] === "M") return "text-gx-success";
  if (c[0] === "D") return "text-gx-error";
  if (c[1] === "M") return "text-gx-warn";
  return "text-gx-muted";
}
