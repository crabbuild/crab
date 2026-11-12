import { Button } from "@/components/ui/button";
/**
 * PTY Daemon diagnostics panel.
 *
 * Displays daemon health metrics: uptime, active/peak sessions, memory,
 * fd count, protocol version, circuit breaker state, respawn count,
 * audit log path, and a path-mismatch banner.
 *
 * Refreshes on open and on a 10 s timer. Pauses when the tab is not visible.
 */

import { useEffect, useRef, useState } from "react";
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  FileText,
  Loader2,
  RefreshCw,
  RotateCcw,
  Shield,
  UploadCloud,
  XCircle,
  Zap,
} from "lucide-react";
import clsx from "clsx";
import { mainProcess } from "../ipc";

// ---------------------------------------------------------------------------
// Types (mirrors DiagnosticsSnapshot from diagnostics-ui-ipc.ts)
// ---------------------------------------------------------------------------

interface CircuitBreakerSnapshot {
  state: "closed" | "open" | "half-open";
  failureCount: number;
  openedAt: number | null;
  cooldownRemainingMs: number | null;
}

interface DiagnosticsSnapshot {
  mode: "daemon" | "in-process";
  uptimeMs: number | null;
  activeSessionCount: number | null;
  memoryUsageBytes: number | null;
  fdCount: number | null;
  protocolVersion: number;
  lastClientConnectedAt: number | null;
  circuitBreaker: CircuitBreakerSnapshot;
  totalSessionsSpawned: number | null;
  peakSessionCount: number | null;
  respawnCount: number | null;
  memoryPressure: "ok" | "warn" | "critical" | null;
  highThroughputSessionCount: number | null;
  pathMismatch: boolean;
  previousEntryPath: string | null;
  auditLogPath: string | null;
  quotasPath: string | null;
  historyBasePath: string | null;
  resolvedSessionTypeBreakdown: { daemon: number; inProcess: number };
  daemonEntryFromEnv: boolean;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatUptime(ms: number): string {
  const totalSec = Math.floor(ms / 1000);
  const days = Math.floor(totalSec / 86400);
  const hours = Math.floor((totalSec % 86400) / 3600);
  const mins = Math.floor((totalSec % 3600) / 60);
  const secs = totalSec % 60;
  if (days > 0) return `${days}d ${hours}h ${mins}m`;
  if (hours > 0) return `${hours}h ${mins}m ${secs}s`;
  if (mins > 0) return `${mins}m ${secs}s`;
  return `${secs}s`;
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB"];
  const i = Math.min(Math.floor(Math.log2(bytes) / 10), units.length - 1);
  const value = bytes / Math.pow(1024, i);
  return `${value.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatCooldown(ms: number): string {
  const secs = Math.ceil(ms / 1000);
  if (secs < 60) return `${secs}s`;
  return `${Math.ceil(secs / 60)}m`;
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

function MetricCard({
  label,
  value,
  icon,
  mono,
  muted,
}: {
  label: string;
  value: string;
  icon: React.ReactNode;
  mono?: boolean;
  muted?: boolean;
}) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-2 flex items-center gap-2 text-[11px] text-muted-foreground">
        {icon}
        {label}
      </div>
      <div
        className={clsx(
          muted ? "text-muted-foreground" : "text-foreground",
          mono ? "truncate font-mono text-xs" : "font-mono text-lg font-semibold",
        )}
        title={value}
      >
        {value}
      </div>
    </div>
  );
}

function BreakerBadge({ state }: { state: CircuitBreakerSnapshot["state"] }) {
  if (state === "closed") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-success/15 px-2 py-0.5 text-[11px] font-medium text-success">
        <CheckCircle2 size={10} />
        closed
      </span>
    );
  }
  if (state === "open") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-destructive/15 px-2 py-0.5 text-[11px] font-medium text-destructive">
        <XCircle size={10} />
        open
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-warning/15 px-2 py-0.5 text-[11px] font-medium text-warning">
      <AlertTriangle size={10} />
      half-open
    </span>
  );
}

function MemoryPressureBadge({ pressure }: { pressure: "ok" | "warn" | "critical" | null }) {
  if (!pressure || pressure === "ok") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-success/15 px-2 py-0.5 text-[11px] font-medium text-success">
        <CheckCircle2 size={10} />
        ok
      </span>
    );
  }
  if (pressure === "warn") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-warning/15 px-2 py-0.5 text-[11px] font-medium text-warning">
        <AlertTriangle size={10} />
        warn
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-destructive/15 px-2 py-0.5 text-[11px] font-medium text-destructive">
      <XCircle size={10} />
      critical
    </span>
  );
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

export function DiagnosticsView() {
  const [diag, setDiag] = useState<DiagnosticsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [restartInProgress, setRestartInProgress] = useState(false);
  const [restartResult, setRestartResult] = useState<{
    success: boolean;
    killedSessionCount?: number;
    error?: string;
  } | null>(null);

  const [upgradeInProgress, setUpgradeInProgress] = useState(false);
  const [upgradeResult, setUpgradeResult] = useState<{
    success: boolean;
    error?: string;
  } | null>(null);

  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  async function fetchDiagnostics() {
    setLoading(true);
    setError(null);
    try {
      const raw = await mainProcess.daemonDiagnostics() as DiagnosticsSnapshot;
      setDiag(raw);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  // Refresh on open and on a 10 s timer; pause when the tab is not visible.
  useEffect(() => {
    void fetchDiagnostics();

    function startTimer() {
      timerRef.current = setInterval(() => {
        if (!document.hidden) {
          void fetchDiagnostics();
        }
      }, 10_000);
    }

    function handleVisibilityChange() {
      if (!document.hidden) {
        // Tab became visible — refresh immediately and restart timer.
        void fetchDiagnostics();
        if (timerRef.current) clearInterval(timerRef.current);
        startTimer();
      }
    }

    startTimer();
    document.addEventListener("visibilitychange", handleVisibilityChange);

    return () => {
      if (timerRef.current) clearInterval(timerRef.current);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
    };
  }, []);

  async function handleRestart() {
    setRestartInProgress(true);
    setRestartResult(null);
    try {
      const result = await mainProcess.daemonRestart();
      setRestartResult(result);
      if (result.success) {
        setTimeout(() => void fetchDiagnostics(), 1500);
      }
    } catch (e) {
      setRestartResult({
        success: false,
        error: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setRestartInProgress(false);
    }
  }

  async function handleUpgrade() {
    setUpgradeInProgress(true);
    setUpgradeResult(null);
    try {
      const result = await mainProcess.daemonPrepareUpgrade();
      setUpgradeResult(result);
      if (result.success) {
        setTimeout(() => void fetchDiagnostics(), 2000);
      }
    } catch (e) {
      setUpgradeResult({
        success: false,
        error: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setUpgradeInProgress(false);
    }
  }

  return (
    <div className="space-y-5">
      {/* Header row */}
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-base font-semibold text-foreground">PTY Daemon</h2>
          <p className="mt-0.5 text-[11px] text-muted-foreground">
            Persistent terminal daemon — keeps sessions alive across app restarts.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => void fetchDiagnostics()}
            disabled={loading}
            aria-label="Refresh daemon diagnostics"
          >
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
            Refresh
          </Button>
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => void handleRestart()}
            disabled={restartInProgress}
            aria-label="Restart PTY daemon"
          >
            {restartInProgress ? (
              <Loader2 size={12} className="animate-spin" />
            ) : (
              <RotateCcw size={12} />
            )}
            {restartInProgress ? "Restarting…" : "Restart Daemon"}
          </Button>
        </div>
      </div>

      {/* Restart result feedback */}
      {restartResult && (
        <div
          className={clsx(
            "rounded border px-3 py-2 text-xs",
            restartResult.success
              ? "border-success/40 bg-success/10 text-success"
              : "border-destructive/40 bg-destructive/10 text-destructive",
          )}
        >
          {restartResult.success
            ? `Daemon restarted. ${restartResult.killedSessionCount ?? 0} session(s) were closed.`
            : `Restart failed: ${restartResult.error ?? "unknown error"}`}
        </div>
      )}

      {/* Error state */}
      {error && (
        <div className="rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
          {error}
        </div>
      )}

      {/* Loading placeholder */}
      {loading && !diag && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 size={14} className="animate-spin" />
          Loading daemon diagnostics…
        </div>
      )}

      {diag && (
        <>
          {/* Path-mismatch banner */}
          {diag.pathMismatch && (
            <div className="rounded border border-warning/40 bg-warning/10 p-3">
              <div className="mb-2 flex items-center gap-2 text-sm font-medium text-warning">
                <AlertTriangle size={14} />
                Daemon binary mismatch detected
              </div>
              {diag.previousEntryPath && (
                <p className="mb-2 text-xs text-muted-foreground">
                  Running daemon was started from a different location:{" "}
                  <code className="rounded bg-background px-1 py-0.5 font-mono text-[11px]">
                    {diag.previousEntryPath}
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
                onClick={() => void handleUpgrade()}
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

          {/* Mode + protocol version */}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <MetricCard
              label="Mode"
              value={diag.mode}
              icon={<Activity size={12} />}
            />
            <MetricCard
              label="Protocol version"
              value={`v${diag.protocolVersion}`}
              icon={<Zap size={12} />}
            />
            <MetricCard
              label="Uptime"
              value={diag.uptimeMs != null ? formatUptime(diag.uptimeMs) : "—"}
              icon={<Activity size={12} />}
              muted={diag.uptimeMs == null}
            />
            <MetricCard
              label="Respawn count"
              value={diag.respawnCount != null ? String(diag.respawnCount) : "—"}
              icon={<RotateCcw size={12} />}
              muted={diag.respawnCount == null}
            />
          </div>

          {/* Session metrics */}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <MetricCard
              label="Active sessions"
              value={diag.activeSessionCount != null ? String(diag.activeSessionCount) : "—"}
              icon={<Activity size={12} />}
              muted={diag.activeSessionCount == null}
            />
            <MetricCard
              label="Peak sessions"
              value={diag.peakSessionCount != null ? String(diag.peakSessionCount) : "—"}
              icon={<Activity size={12} />}
              muted={diag.peakSessionCount == null}
            />
            <MetricCard
              label="Total spawned"
              value={diag.totalSessionsSpawned != null ? String(diag.totalSessionsSpawned) : "—"}
              icon={<Activity size={12} />}
              muted={diag.totalSessionsSpawned == null}
            />
            <MetricCard
              label="High-throughput"
              value={diag.highThroughputSessionCount != null ? String(diag.highThroughputSessionCount) : "—"}
              icon={<Zap size={12} />}
              muted={diag.highThroughputSessionCount == null}
            />
          </div>

          {/* Memory + fd */}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
            <MetricCard
              label="Heap memory"
              value={diag.memoryUsageBytes != null ? formatBytes(diag.memoryUsageBytes) : "—"}
              icon={<Activity size={12} />}
              muted={diag.memoryUsageBytes == null}
            />
            <MetricCard
              label="Open file descriptors"
              value={diag.fdCount != null ? String(diag.fdCount) : "—"}
              icon={<FileText size={12} />}
              muted={diag.fdCount == null}
            />
            <div className="rounded-lg border border-border bg-card p-4">
              <div className="mb-2 flex items-center gap-2 text-[11px] text-muted-foreground">
                <Activity size={12} />
                Memory pressure
              </div>
              <MemoryPressureBadge pressure={diag.memoryPressure} />
            </div>
          </div>

          {/* Circuit breaker */}
          <div className="rounded-lg border border-border bg-card p-4">
            <div className="mb-3 flex items-center gap-2 text-xs font-medium text-muted-foreground">
              <Shield size={13} />
              Circuit Breaker
            </div>
            <div className="flex flex-wrap items-center gap-4 text-sm">
              <div className="flex items-center gap-2">
                <span className="text-muted-foreground text-xs">State</span>
                <BreakerBadge state={diag.circuitBreaker.state} />
              </div>
              <div className="flex items-center gap-2">
                <span className="text-muted-foreground text-xs">Failures</span>
                <span className="font-mono text-sm text-foreground">
                  {diag.circuitBreaker.failureCount}
                </span>
              </div>
              {diag.circuitBreaker.state === "open" &&
                diag.circuitBreaker.cooldownRemainingMs != null && (
                  <div className="flex items-center gap-2">
                    <span className="text-muted-foreground text-xs">Cooldown remaining</span>
                    <span className="font-mono text-sm text-warning">
                      {formatCooldown(diag.circuitBreaker.cooldownRemainingMs)}
                    </span>
                  </div>
                )}
              {diag.circuitBreaker.openedAt != null && (
                <div className="flex items-center gap-2">
                  <span className="text-muted-foreground text-xs">Opened at</span>
                  <span className="font-mono text-xs text-foreground">
                    {new Date(diag.circuitBreaker.openedAt).toLocaleTimeString()}
                  </span>
                </div>
              )}
            </div>
          </div>

          {/* Paths */}
          <div className="rounded-lg border border-border bg-card p-4">
            <div className="mb-3 text-xs font-medium text-muted-foreground">Paths</div>
            <div className="space-y-2 text-xs">
              <PathRow label="Audit log" value={diag.auditLogPath} />
              <PathRow label="Quotas config" value={diag.quotasPath} />
              <PathRow label="Terminal history" value={diag.historyBasePath} />
              {diag.daemonEntryFromEnv && (
                <div className="flex items-center gap-1.5 rounded bg-primary/10 px-2 py-1 text-[11px] text-primary">
                  <Zap size={10} />
                  Daemon entry path overridden via <code className="font-mono">CRAB_DAEMON_ENTRY</code>
                </div>
              )}
            </div>
          </div>

          {/* Session type breakdown */}
          <div className="rounded-lg border border-border bg-card p-4">
            <div className="mb-3 text-xs font-medium text-muted-foreground">Active session breakdown</div>
            <div className="flex gap-6 text-sm">
              <div>
                <div className="text-[10px] uppercase tracking-wider text-muted-foreground">Daemon</div>
                <div className="mt-0.5 font-mono text-lg font-semibold text-foreground">
                  {diag.resolvedSessionTypeBreakdown.daemon}
                </div>
              </div>
              <div>
                <div className="text-[10px] uppercase tracking-wider text-muted-foreground">In-process</div>
                <div className="mt-0.5 font-mono text-lg font-semibold text-foreground">
                  {diag.resolvedSessionTypeBreakdown.inProcess}
                </div>
              </div>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

function PathRow({ label, value }: { label: string; value: string | null }) {
  return (
    <div className="flex items-start gap-3">
      <span className="w-28 shrink-0 text-muted-foreground">{label}</span>
      {value ? (
        <code className="min-w-0 flex-1 truncate rounded bg-background px-1 py-0.5 font-mono text-[11px] text-foreground" title={value}>
          {value}
        </code>
      ) : (
        <span className="text-muted-foreground italic">not configured</span>
      )}
    </div>
  );
}
