import { useEffect, useState } from "react";
import { ShieldCheck, ShieldAlert, RefreshCw } from "lucide-react";
import { OpenRepo } from "../state";
import { AuthStatus, rpc } from "../ipc";

export function AuthPage({ repo }: { repo: OpenRepo }) {
  const [status, setStatus] = useState<AuthStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  async function refresh() {
    setLoading(true);
    setError(null);
    try {
      const s = await rpc.authStatus(repo.repo_id);
      setStatus(s);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repo.repo_id]);

  const expiresAt = status?.token_expiry ? new Date(status.token_expiry) : null;
  const expired = status?.token_expired ?? false;
  const signedIn = status?.identity != null && !expired;

  return (
    <div className="h-full overflow-auto p-4">
      <div className="mb-4 flex items-center gap-3">
        <div
          className={`flex h-10 w-10 items-center justify-center rounded-full border ${
            signedIn ? "border-gx-success/40 bg-gx-success/10 text-gx-success" : "border-gx-warn/40 bg-gx-warn/10 text-gx-warn"
          }`}
        >
          {signedIn ? <ShieldCheck size={18} /> : <ShieldAlert size={18} />}
        </div>
        <div className="min-w-0 flex-1">
          <div className="text-sm font-semibold">
            {signedIn ? "Signed in" : expired ? "Token expired" : "Not signed in"}
          </div>
          <div className="text-xs text-gx-muted">
            Provider: <code className="font-mono">{status?.provider ?? "–"}</code>
          </div>
        </div>
        <button type="button" className="btn text-xs" onClick={refresh} disabled={loading}>
          <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
          Refresh
        </button>
      </div>

      {error && (
        <div className="mb-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
          {error}
        </div>
      )}

      {status && (
        <div className="space-y-3 rounded border border-gx-border bg-gx-surface p-4 text-sm">
          <KV label="Identity" value={status.identity ?? "–"} />
          <KV label="Expires" value={expiresAt ? expiresAt.toLocaleString() : "–"} />
          <KV label="Refresh token" value={status.refresh ? "Available" : "Unavailable"} />
        </div>
      )}

      <div className="mt-4 text-xs text-gx-muted">
        To sign in, run <code className="kbd">crab login</code> from a terminal, then click Refresh.
        A future desktop update will launch the OAuth flow in your browser directly.
      </div>
    </div>
  );
}

function KV({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-3 border-b border-gx-border/60 pb-2 last:border-b-0 last:pb-0">
      <div className="text-[11px] uppercase tracking-wider text-gx-muted">{label}</div>
      <div className="truncate font-mono text-sm">{value}</div>
    </div>
  );
}
