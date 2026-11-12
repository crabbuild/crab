import { Button } from "@/components/ui/button";
import { useEffect, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  ClipboardCopy,
  Database,
  FolderOpen,
  HardDrive,
  Loader2,
  RefreshCw,
  UploadCloud,
  XCircle,
} from "lucide-react";
import clsx from "clsx";
import { AgentHealth, AgentHealthWarning, IndexerStatus, mainProcess, rpc } from "../ipc";
import { useStore } from "../state";
import { sanitizeRemoteUrl } from "../lib/format";

function formatBytes(bytesStr: string): string {
  const bytes = Number(bytesStr);
  if (!Number.isFinite(bytes) || bytes === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const i = Math.min(Math.floor(Math.log2(bytes) / 10), units.length - 1);
  const value = bytes / Math.pow(1024, i);
  return `${value.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

export function HealthView() {
  const [health, setHealth] = useState<AgentHealth | null>(null);
  const [indexer, setIndexer] = useState<IndexerStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const { state } = useStore();

  // Daemon diagnostics state
  const [daemonDiag, setDaemonDiag] = useState<{
    pathMismatch: boolean;
    previousEntryPath: string | null;
  } | null>(null);
  const [upgradeInProgress, setUpgradeInProgress] = useState(false);
  const [upgradeResult, setUpgradeResult] = useState<{ success: boolean; error?: string } | null>(null);

  async function fetchHealth() {
    setLoading(true);
    setError(null);
    try {
      const [data, idx] = await Promise.all([
        rpc.agentHealth(),
        rpc.indexerStatus(),
      ]);
      setHealth(data);
      setIndexer(idx);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }

    // Fetch daemon diagnostics separately — non-fatal if unavailable.
    try {
      const diag = await mainProcess.daemonDiagnostics() as {
        pathMismatch?: boolean;
        previousEntryPath?: string | null;
      };
      setDaemonDiag({
        pathMismatch: diag.pathMismatch ?? false,
        previousEntryPath: diag.previousEntryPath ?? null,
      });
    } catch {
      // Daemon may not be running — ignore.
    }
  }

  useEffect(() => {
    void fetchHealth();
  }, []);

  async function applyDaemonUpgrade() {
    setUpgradeInProgress(true);
    setUpgradeResult(null);
    try {
      const result = await mainProcess.daemonPrepareUpgrade();
      setUpgradeResult(result);
      if (result.success) {
        // Refresh diagnostics after a short delay to reflect the new daemon.
        setTimeout(() => void fetchHealth(), 2000);
      }
    } catch (e) {
      setUpgradeResult({ success: false, error: e instanceof Error ? e.message : String(e) });
    } finally {
      setUpgradeInProgress(false);
    }
  }

  async function copyDiagnostics() {
    const lines: string[] = [];
    lines.push("=== Crab Desktop Diagnostics ===");
    lines.push(`Timestamp: ${new Date().toISOString()}`);
    lines.push("");

    if (health) {
      lines.push(`Agent version: ${health.agent_version}`);
      lines.push(`CLI version: ${health.crab_cli_version ?? "not installed"}`);
      lines.push(`Cache dir: ${health.cache_dir}`);
      lines.push(`Cache used: ${formatBytes(health.cache_used_bytes)}`);
      lines.push(`Cache free: ${formatBytes(health.cache_free_bytes)}`);
      lines.push(`Staging size: ${formatBytes(health.staging_size_bytes)}`);
      lines.push(`Last GC: ${health.last_gc_at ?? "never"}`);
      if (health.warnings.length > 0) {
        lines.push("");
        lines.push("Warnings:");
        for (const w of health.warnings) {
          lines.push(`  [${w.level}] ${w.code}: ${w.message}`);
        }
      }
    }

    // Append last 500 log lines from the app state.
    lines.push("");
    lines.push("=== Recent Logs (last 500 lines) ===");
    const recentLogs = state.logs.slice(-500);
    for (const log of recentLogs) {
      lines.push(`[${log.stream}] ${log.op}/${log.job_id}: ${log.line}`);
    }

    const text = lines.join("\n");
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Fallback: no-op, clipboard may be unavailable.
    }
  }

  return (
    <section aria-label="Health & Diagnostics" className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h3 className="text-sm font-semibold text-foreground">Health &amp; Diagnostics</h3>
        <div className="flex items-center gap-2">
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => void mainProcess.openLogsFolder()}
            aria-label="Open logs folder in file manager"
          >
            <FolderOpen size={12} />
            Open logs
          </Button>
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => void fetchHealth()}
            disabled={loading}
            aria-label="Refresh health data"
          >
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
            Refresh
          </Button>
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => void copyDiagnostics()}
            aria-label="Copy diagnostics to clipboard"
          >
            <ClipboardCopy size={12} />
            {copied ? "Copied!" : "Copy diagnostics"}
          </Button>
        </div>
      </div>

      {error && (
        <div className="mb-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
          {error}
        </div>
      )}

      {/* Daemon path-mismatch banner — shown when a stale daemon from a
          different app location is detected. The upgrade button triggers
          a zero-downtime handoff to the current binary. */}
      {daemonDiag?.pathMismatch && (
        <div className="mb-4 rounded border border-warning/40 bg-warning/10 p-3">
          <div className="mb-2 flex items-center gap-2 text-sm font-medium text-warning">
            <AlertTriangle size={14} />
            Daemon binary mismatch detected
          </div>
          {daemonDiag.previousEntryPath && (
            <p className="mb-2 text-xs text-muted-foreground">
              Running daemon was started from a different location:{" "}
              <code className="rounded bg-background px-1 py-0.5 font-mono text-[11px]">
                {daemonDiag.previousEntryPath}
              </code>
            </p>
          )}
          {upgradeResult && (
            <div
              className={clsx(
                "mb-2 rounded px-2 py-1 text-xs",
                upgradeResult.success
                  ? "bg-success/10 text-success"
                  : "bg-destructive/10 text-destructive",
              )}
            >
              {upgradeResult.success
                ? "Daemon upgrade initiated successfully."
                : `Upgrade failed: ${upgradeResult.error ?? "unknown error"}`}
            </div>
          )}
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => void applyDaemonUpgrade()}
            disabled={upgradeInProgress}
            aria-label="Apply daemon upgrade"
          >
            {upgradeInProgress ? (
              <Loader2 size={12} className="animate-spin" />
            ) : (
              <UploadCloud size={12} />
            )}
            {upgradeInProgress ? "Applying…" : "Apply daemon upgrade"}
          </Button>
        </div>
      )}

      {loading && !health && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 size={14} className="animate-spin" />
          Loading health data…
        </div>
      )}

      {health && (
        <div className="space-y-4">
          {/* Warning chips */}
          {health.warnings.length > 0 && (
            <div className="flex flex-wrap gap-2">
              {health.warnings.map((w, i) => (
                <WarningChip key={i} warning={w} />
              ))}
            </div>
          )}

          {/* Version & path cards */}
          <div className="grid grid-cols-[repeat(auto-fit,minmax(160px,1fr))] gap-3">
            <Card
              title="Agent Version"
              value={health.agent_version}
              icon={<CheckCircle2 size={14} className="text-success" />}
            />
            <Card
              title="CLI Version"
              value={health.crab_cli_version ?? "Not installed"}
              icon={
                health.crab_cli_version ? (
                  <CheckCircle2 size={14} className="text-success" />
                ) : (
                  <XCircle size={14} className="text-warning" />
                )
              }
            />
            {!health.crab_cli_version && (
              <div className="col-span-full rounded-lg border border-warning/30 bg-warning/5 p-4">
                <div className="mb-2 flex items-center gap-2 text-sm font-medium text-warning">
                  <AlertTriangle size={14} />
                  Crab CLI not detected
                </div>
                <p className="mb-3 text-xs text-muted-foreground">
                  The CLI may be installed but not in the agent&apos;s PATH. If you installed via{" "}
                  <code className="rounded bg-background px-1 py-0.5 font-mono text-[11px]">make install</code>{" "}
                  it lives at <code className="rounded bg-background px-1 py-0.5 font-mono text-[11px]">~/.cargo/bin/crab</code>.
                </p>
                <div className="rounded border border-border bg-background p-3 font-mono text-[11px] text-foreground">
                  <div className="mb-1 text-muted-foreground"># Install from source (recommended):</div>
                  <div>cd crab && make install</div>
                  <div className="mt-2 mb-1 text-muted-foreground"># Verify installation:</div>
                  <div>which crab</div>
                  <div>crab --version</div>
                  <div className="mt-2 mb-1 text-muted-foreground"># Ensure ~/.cargo/bin is in your PATH:</div>
                  <div>export PATH=&quot;$HOME/.cargo/bin:$PATH&quot;</div>
                </div>
                <p className="mt-2 text-[10px] text-muted-foreground">
                  Note: The desktop agent may not inherit your shell&apos;s PATH. Restart the app after installing.
                </p>
              </div>
            )}
            <Card
              title="Cache Directory"
              value={health.cache_dir}
              icon={<HardDrive size={14} className="text-muted-foreground" />}
              mono
            />
          </div>

          {/* Storage metrics */}
          <div className="grid grid-cols-[repeat(auto-fit,minmax(160px,1fr))] gap-3">
            <Card
              title="Cache Used"
              value={formatBytes(health.cache_used_bytes)}
              icon={<HardDrive size={14} className="text-primary" />}
            />
            <Card
              title="Disk Free"
              value={formatBytes(health.cache_free_bytes)}
              icon={<HardDrive size={14} className="text-muted-foreground" />}
            />
            <Card
              title="Staging Size"
              value={formatBytes(health.staging_size_bytes)}
              icon={<HardDrive size={14} className="text-muted-foreground" />}
            />
            <Card
              title="Last GC"
              value={health.last_gc_at ?? "Never"}
              icon={<RefreshCw size={14} className="text-muted-foreground" />}
            />
          </div>

          {/* Indexer status */}
          <IndexerCard status={indexer} />

          {/* Repository info section */}
          <RepoInfoSection />
        </div>
      )}
    </section>
  );
}

function Card({
  title,
  value,
  icon,
  mono,
}: {
  title: string;
  value: string;
  icon: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-2 flex items-center gap-2 text-[11px] text-muted-foreground">
        {icon}
        {title}
      </div>
      <div
        className={clsx(
          "text-foreground",
          mono ? "truncate font-mono text-xs" : "font-mono text-lg font-semibold",
        )}
        title={value}
      >
        {value}
      </div>
    </div>
  );
}

function WarningChip({ warning }: { warning: AgentHealthWarning }) {
  return (
    <div
      className={clsx(
        "flex items-center gap-1.5 rounded-full px-3 py-1 text-xs",
        warning.level === "error"
          ? "bg-destructive/15 text-destructive"
          : "bg-warning/15 text-warning",
      )}
    >
      <AlertTriangle size={11} />
      <span>{warning.message}</span>
    </div>
  );
}

function IndexerCard({ status }: { status: IndexerStatus | null }) {
  if (!status) return null;

  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 flex items-center gap-2 text-xs text-muted-foreground">
        <Database size={14} className="text-muted-foreground" />
        Indexer
      </div>
      <div className="space-y-2 text-sm">
        <div className="flex items-center gap-2">
          <span
            className={clsx(
              "inline-block h-2 w-2 rounded-full",
              status.running ? "bg-success" : "bg-accent",
            )}
            aria-label={status.running ? "Running" : "Disabled"}
          />
          <span className="text-foreground">
            {status.running ? "Running" : "Indexer disabled"}
          </span>
        </div>
        {status.db_path && (
          <div className="flex items-center justify-between">
            <span className="text-muted-foreground">Database</span>
            <span className="truncate pl-2 font-mono text-xs text-foreground" title={status.db_path}>
              {status.db_path}
            </span>
          </div>
        )}
        <div className="flex items-center justify-between">
          <span className="text-muted-foreground">Entries cached</span>
          <span className="text-foreground">{status.entries_cached.toLocaleString()}</span>
        </div>
        <div className="flex items-center justify-between">
          <span className="text-muted-foreground">Repos indexed</span>
          <span className="text-foreground">{status.repos_indexed.toLocaleString()}</span>
        </div>
      </div>
    </div>
  );
}

function RepoInfoSection() {
  const [info, setInfo] = useState<Awaited<ReturnType<typeof rpc.gitRepoInfo>> | null>(null);
  const { state } = useStore();
  const activeRepo = state.repos.find(r => r.repo_id === state.activeRepoId);

  useEffect(() => {
    if (!activeRepo) return;
    rpc.gitRepoInfo(activeRepo.repo_id).then(setInfo).catch(() => {});
  }, [activeRepo?.repo_id]);

  if (!info) return null;

  return (
    <div className="space-y-4">
      <h4 className="text-sm font-semibold text-foreground">Repository Info</h4>

      {/* HEAD + working dir */}
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <Card title="HEAD" value={`${info.head_branch || "detached"} (${info.head_sha.slice(0, 12)})`} icon={<CheckCircle2 size={14} className="text-primary" />} mono />
        <Card title=".git size" value={formatBytes(info.git_dir_size)} icon={<HardDrive size={14} className="text-muted-foreground" />} />
      </div>

      {/* Remotes */}
      {info.remotes.length > 0 && (
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="mb-2 text-[10px] uppercase tracking-wider text-muted-foreground">Remotes ({info.remotes.length})</div>
          <ul className="space-y-1 text-[12px]">
            {info.remotes.map((r, i) => (
              <li key={i} className="flex items-center gap-2">
                <span className="shrink-0 font-semibold text-foreground">{r.name}</span>
                <span className="min-w-0 flex-1 truncate font-mono text-muted-foreground">{sanitizeRemoteUrl(r.url)}</span>
                <span className="chip text-[9px]">{r.type}</span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* Git config */}
      {info.config.length > 0 && (
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="mb-2 text-[10px] uppercase tracking-wider text-muted-foreground">Git Config</div>
          <ul className="space-y-1 text-[11px]">
            {info.config.map((c, i) => (
              <li key={i} className="flex items-center gap-2 font-mono">
                <span className="shrink-0 text-muted-foreground">{c.key}</span>
                <span className="text-foreground">= {c.value}</span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* Hooks */}
      {info.hooks.length > 0 && (
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="mb-2 text-[10px] uppercase tracking-wider text-muted-foreground">Active Hooks ({info.hooks.length})</div>
          <div className="flex flex-wrap gap-1">
            {info.hooks.map(h => (
              <span key={h} className="chip font-mono text-[10px]">{h}</span>
            ))}
          </div>
        </div>
      )}

      {/* Submodules */}
      {info.submodules.length > 0 && (
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="mb-2 text-[10px] uppercase tracking-wider text-muted-foreground">Submodules ({info.submodules.length})</div>
          <ul className="space-y-1 text-[11px]">
            {info.submodules.map((s, i) => (
              <li key={i} className="flex items-center gap-2 font-mono">
                <span className="text-foreground">{s.path}</span>
                <span className="text-muted-foreground">{s.sha.slice(0, 7)}</span>
                {s.describe && <span className="text-muted-foreground">({s.describe})</span>}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
