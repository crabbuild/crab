import { Button } from "@/components/ui/button";
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
      <div className="flex items-center gap-2 border-b border-border bg-background px-3 py-2">
        <Button variant="outline" type="button" className="text-xs" onClick={() => void refresh()} disabled={loading}>
          {loading ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />} Refresh
        </Button>
        <Button variant="default"
          type="button"
          className="text-xs"
          onClick={async () => {
            try { await rpc.gitPushBranch(repo.repo_id, info?.branch ?? "HEAD"); void refresh(); }
            catch (e) { setError(e instanceof Error ? e.message : String(e)); }
          }}
        >
          <Upload size={12} /> Push
        </Button>
      </div>

      {error && (
        <div className="m-3 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{error}</div>
      )}

      <div className="min-h-0 flex-1 overflow-auto p-3">
        {!info && !error && (
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Loading status…
          </div>
        )}
        {info && (
          <div className="space-y-3">
            {/* Branch info */}
            <div className="flex items-center gap-3 rounded border border-border bg-card p-3">
              <GitBranch size={14} className="text-primary" />
              <div>
                <div className="font-mono text-[13px] font-semibold">{info.branch}</div>
                {info.upstream && (
                  <div className="text-[11px] text-muted-foreground">
                    Tracks {info.upstream}
                    {info.ahead > 0 && <span className="ml-2 text-success">↑{info.ahead}</span>}
                    {info.behind > 0 && <span className="ml-1 text-destructive">↓{info.behind}</span>}
                  </div>
                )}
                {!info.upstream && <div className="text-[11px] text-muted-foreground">No upstream</div>}
              </div>
            </div>

            {/* Summary */}
            <div className="grid grid-cols-3 gap-2 text-center">
              <div className="rounded border border-border bg-card p-2">
                <div className="text-lg font-mono text-success">{info.staged}</div>
                <div className="text-[10px] text-muted-foreground">Staged</div>
              </div>
              <div className="rounded border border-border bg-card p-2">
                <div className="text-lg font-mono text-warning">{info.modified}</div>
                <div className="text-[10px] text-muted-foreground">Modified</div>
              </div>
              <div className="rounded border border-border bg-card p-2">
                <div className="text-lg font-mono text-muted-foreground">{info.untracked}</div>
                <div className="text-[10px] text-muted-foreground">Untracked</div>
              </div>
            </div>

            {/* File list */}
            {info.entries.length === 0 ? (
              <div className="text-center text-xs text-muted-foreground">Working tree is clean ✓</div>
            ) : (
              <div className="rounded border border-border bg-card">
                <ul className="max-h-[300px] overflow-auto text-[11px]">
                  {info.entries.map((e) => (
                    <li key={e.path} className="flex items-center gap-2 border-b border-border/40 px-3 py-1 last:border-b-0">
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
  if (c === "??") return "text-muted-foreground";
  if (c[0] === "A" || c[0] === "M") return "text-success";
  if (c[0] === "D") return "text-destructive";
  if (c[1] === "M") return "text-warning";
  return "text-muted-foreground";
}
