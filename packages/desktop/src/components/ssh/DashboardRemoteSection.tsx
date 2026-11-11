/**
 * Dashboard section showing remote server metrics for connected SSH
 * connections. Displays per-server: compact health, hydration stats,
 * connection latency, and disk usage. Collapsible, only rendered when
 * at least one SSH connection exists.
 *
 * Embedded in DashboardView as a "Remote Servers" section.
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  Cloud,
  Clock,
  Droplets,
  Loader2,
  Monitor,
  Signal,
} from "lucide-react";
import { useStore, SshConnectionState } from "../../state";
import { SystemHealth, type SystemHealthData } from "./SystemHealth";
import { rpc } from "../../ipc";

// ── Types ────────────────────────────────────────────────────────

interface RemoteHydrationStats {
  totalFiles: number;
  hydrated: number;
  dehydrated: number;
  partial: number;
  loading: boolean;
  error: string | null;
}

interface ServerCardProps {
  profileId: string;
  connection: SshConnectionState;
}

// ── Helpers ──────────────────────────────────────────────────────

/** Convert SshConnectionState.health to SystemHealthData for the SystemHealth component. */
function toSystemHealthData(
  health: NonNullable<SshConnectionState["health"]>,
): SystemHealthData {
  const diskUsedPercent =
    health.diskTotal > 0
      ? Math.round(((health.diskTotal - health.diskFree) / health.diskTotal) * 100)
      : 0;
  const memUsedPercent =
    health.memTotal > 0
      ? Math.round(((health.memTotal - health.memFree) / health.memTotal) * 100)
      : 0;

  return {
    disk: [
      {
        path: "/",
        total: health.diskTotal,
        free: health.diskFree,
        usedPercent: diskUsedPercent,
      },
    ],
    memory: {
      total: health.memTotal,
      free: health.memFree,
      cached: 0,
      usedPercent: memUsedPercent,
    },
    gpus: health.gpus?.map((g, i) => ({
      index: i,
      name: g.name,
      utilization: g.utilization,
      memoryUsed: g.memUsed,
      memoryTotal: g.memTotal,
      temperature: g.temp,
    })) ?? null,
  };
}

function latencyColor(ms: number | undefined): string {
  if (ms === undefined) return "text-muted-foreground";
  if (ms < 50) return "text-success text-success";
  if (ms < 200) return "text-warning text-warning";
  return "text-destructive text-destructive";
}

function latencyLabel(ms: number | undefined): string {
  if (ms === undefined) return "—";
  return `${ms}ms`;
}

/** Format uptime seconds into a human-readable string. */
function formatUptime(seconds: number | undefined): string | null {
  if (seconds === undefined || seconds <= 0) return null;
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);

  if (days > 0) {
    return `${days}d ${hours}h`;
  }
  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }
  return `${minutes}m`;
}

/** Format bytes into a human-readable string (e.g., "12.5 MB"). */
function formatBytes(bytes: number): string {
  if (bytes >= 1_073_741_824) {
    return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  }
  if (bytes >= 1_048_576) {
    return `${(bytes / 1_048_576).toFixed(1)} MB`;
  }
  if (bytes >= 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${bytes} B`;
}

// ── Server Card ──────────────────────────────────────────────────

function ServerCard({ profileId, connection }: ServerCardProps) {
  const [expanded, setExpanded] = useState(false);
  const [hydration, setHydration] = useState<RemoteHydrationStats>({
    totalFiles: 0,
    hydrated: 0,
    dehydrated: 0,
    partial: 0,
    loading: false,
    error: null,
  });

  // Fetch hydration stats for the first open repo on this connection
  useEffect(() => {
    if (connection.status !== "connected" || connection.openRepos.length === 0) {
      return;
    }

    let cancelled = false;
    setHydration((prev) => ({ ...prev, loading: true, error: null }));

    // Fetch hydration for the first open remote repo
    const repoId = connection.openRepos[0];
    rpc
      .hydrationStatus(repoId)
      .then((result) => {
        if (cancelled) return;
        const summary = result.summary;
        setHydration({
          totalFiles: summary.total_tracked,
          hydrated: summary.hydrated,
          dehydrated: summary.pointer,
          partial: summary.partial,
          loading: false,
          error: null,
        });
      })
      .catch((err: Error) => {
        if (cancelled) return;
        setHydration((prev) => ({
          ...prev,
          loading: false,
          error: err.message,
        }));
      });

    return () => {
      cancelled = true;
    };
  }, [connection.status, connection.openRepos]);

  const healthData = connection.health
    ? toSystemHealthData(connection.health)
    : undefined;

  const hostname = connection.serverInfo?.hostname ?? profileId;
  const os = connection.serverInfo?.os ?? "";
  const arch = connection.serverInfo?.arch ?? "";
  const isConnected = connection.status === "connected";

  return (
    <div
      className="rounded-lg border border-border bg-card overflow-hidden"
      aria-label={`Remote server: ${hostname}`}
    >
      {/* Header — always visible */}
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-3 p-3 text-left hover:bg-accent transition-colors cursor-pointer"
        aria-expanded={expanded}
        aria-controls={`server-detail-${profileId}`}
      >
        {/* Status indicator */}
        <div
          className={clsx(
            "h-2 w-2 shrink-0 rounded-full",
            isConnected ? "bg-success" : "bg-accent",
          )}
          aria-label={isConnected ? "Connected" : "Disconnected"}
        />

        {/* Server name + info */}
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <Monitor size={13} className="shrink-0 text-muted-foreground" aria-hidden="true" />
            <span className="truncate text-sm font-medium text-foreground">
              {hostname}
            </span>
            {os && arch && (
              <span className="hidden sm:inline text-[10px] text-muted-foreground">
                {os} {arch}
              </span>
            )}
          </div>
          {/* Compact health summary */}
          {isConnected && healthData && (
            <div className="mt-0.5">
              <SystemHealth health={healthData} compact />
            </div>
          )}
        </div>

        {/* Latency badge */}
        <div
          className={clsx(
            "flex items-center gap-1 text-[11px] font-mono shrink-0",
            latencyColor(connection.latencyMs),
          )}
          aria-label={`Latency: ${latencyLabel(connection.latencyMs)}`}
        >
          <Signal size={11} aria-hidden="true" />
          {latencyLabel(connection.latencyMs)}
        </div>

        {/* Expand/collapse chevron */}
        {expanded ? (
          <ChevronDown size={14} className="shrink-0 text-muted-foreground" aria-hidden="true" />
        ) : (
          <ChevronRight size={14} className="shrink-0 text-muted-foreground" aria-hidden="true" />
        )}
      </button>

      {/* Expanded detail */}
      {expanded && (
        <div
          id={`server-detail-${profileId}`}
          className="border-t border-border px-3 pb-3 pt-2 space-y-3"
        >
          {/* Full system health */}
          {isConnected && healthData ? (
            <SystemHealth health={healthData} />
          ) : (
            <div className="text-xs text-muted-foreground">
              {isConnected ? "Loading health data…" : "Connect to view system health."}
            </div>
          )}

          {/* Hydration stats */}
          {isConnected && connection.openRepos.length > 0 && (
            <div className="space-y-1.5">
              <div className="flex items-center gap-1.5 text-[11px] font-medium text-muted-foreground">
                <Droplets size={11} aria-hidden="true" />
                Hydration
              </div>
              {hydration.loading ? (
                <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
                  <Loader2 size={11} className="animate-spin" aria-hidden="true" />
                  Loading…
                </div>
              ) : hydration.error ? (
                <div className="text-[11px] text-muted-foreground italic">
                  Unable to fetch hydration data
                </div>
              ) : (
                <div className="grid grid-cols-3 gap-2">
                  <HydrationStat
                    label="Total files"
                    value={hydration.totalFiles}
                  />
                  <HydrationStat
                    label="Hydrated"
                    value={hydration.hydrated}
                    color="text-success text-success"
                  />
                  <HydrationStat
                    label="Dehydrated"
                    value={hydration.dehydrated}
                    color="text-warning text-warning"
                  />
                </div>
              )}
            </div>
          )}

          {/* Connection info */}
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[11px] text-muted-foreground">
            {connection.health?.uptime != null && formatUptime(connection.health.uptime) && (
              <span className="flex items-center gap-1">
                <Clock size={10} aria-hidden="true" />
                Uptime: {formatUptime(connection.health.uptime)}
              </span>
            )}
            {connection.bandwidth && (connection.bandwidth.bytesIn > 0 || connection.bandwidth.bytesOut > 0) && (
              <span className="font-mono" aria-label={`Bandwidth: ${formatBytes(connection.bandwidth.bytesOut)} sent, ${formatBytes(connection.bandwidth.bytesIn)} received`}>
                ↑ {formatBytes(connection.bandwidth.bytesOut)} ↓ {formatBytes(connection.bandwidth.bytesIn)}
              </span>
            )}
            {connection.serverInfo?.agentVersion && (
              <span>Agent v{connection.serverInfo.agentVersion}</span>
            )}
            {connection.openRepos.length > 0 && (
              <span>{connection.openRepos.length} repo{connection.openRepos.length !== 1 ? "s" : ""} open</span>
            )}
            {connection.terminals.length > 0 && (
              <span>{connection.terminals.length} terminal{connection.terminals.length !== 1 ? "s" : ""}</span>
            )}
            {connection.portForwards.filter((f) => f.status === "active").length > 0 && (
              <span>
                {connection.portForwards.filter((f) => f.status === "active").length} port forward{connection.portForwards.filter((f) => f.status === "active").length !== 1 ? "s" : ""}
              </span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function HydrationStat({
  label,
  value,
  color,
}: {
  label: string;
  value: number;
  color?: string;
}) {
  return (
    <div className="text-center">
      <div className={clsx("font-mono text-sm", color ?? "text-foreground")}>
        {value.toLocaleString()}
      </div>
      <div className="text-[10px] text-muted-foreground">{label}</div>
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────

export interface DashboardRemoteSectionProps {
  /** Override collapse state persistence key */
  storageKey?: string;
}

/**
 * Remote Servers section for the Dashboard. Lists all connected SSH
 * servers with compact health, hydration stats, and latency. Hidden
 * when no SSH connections exist. Collapsible with state persisted in
 * localStorage.
 */
export function DashboardRemoteSection({
  storageKey = "crab.dashboard.section.remoteServersExpanded",
}: DashboardRemoteSectionProps) {
  const { state } = useStore();

  const [expanded, setExpanded] = useState(() => {
    try {
      const stored = localStorage.getItem(storageKey);
      if (stored !== null) return stored === "true";
    } catch {
      /* localStorage unavailable */
    }
    return true;
  });

  const toggle = useCallback(() => {
    setExpanded((prev) => {
      const next = !prev;
      try {
        localStorage.setItem(storageKey, String(next));
      } catch {
        /* ignore */
      }
      return next;
    });
  }, [storageKey]);

  // Only show connected (or connecting/reconnecting) servers
  const connections = useMemo(() => {
    const entries: Array<{ profileId: string; connection: SshConnectionState }> = [];
    for (const [profileId, conn] of state.sshConnections) {
      entries.push({ profileId, connection: conn });
    }
    // Sort: connected first, then by hostname
    entries.sort((a, b) => {
      const aConnected = a.connection.status === "connected" ? 0 : 1;
      const bConnected = b.connection.status === "connected" ? 0 : 1;
      if (aConnected !== bConnected) return aConnected - bConnected;
      const aName = a.connection.serverInfo?.hostname ?? a.profileId;
      const bName = b.connection.serverInfo?.hostname ?? b.profileId;
      return aName.localeCompare(bName);
    });
    return entries;
  }, [state.sshConnections]);

  // Don't render anything if there are no SSH connections
  if (connections.length === 0) {
    return null;
  }

  return (
    <div className="pt-4 pb-2">
      <button
        type="button"
        onClick={toggle}
        className="flex w-full items-center gap-2 mb-3 cursor-pointer group"
        aria-expanded={expanded}
        aria-controls="section-remote-servers"
      >
        <span
          className="text-[10px] text-muted-foreground transition-transform duration-200"
          style={{
            display: "inline-block",
            transform: expanded ? "rotate(90deg)" : "rotate(0deg)",
          }}
        >
          ▶
        </span>
        <Cloud size={12} className="text-muted-foreground" aria-hidden="true" />
        <span className="text-[10px] uppercase tracking-wider text-muted-foreground group-hover:text-foreground transition-colors">
          Remote Servers
        </span>
        <span className="text-[10px] text-muted-foreground">
          ({connections.length})
        </span>
        <div className="h-px flex-1 bg-border/50" />
      </button>
      <div
        id="section-remote-servers"
        className="overflow-hidden transition-[max-height,opacity] duration-200 ease-in-out"
        style={{
          maxHeight: expanded ? "5000px" : "0px",
          opacity: expanded ? 1 : 0,
        }}
      >
        {expanded && (
          <div className="space-y-2" role="list" aria-label="Remote servers">
            {connections.map(({ profileId, connection }) => (
              <ServerCard
                key={profileId}
                profileId={profileId}
                connection={connection}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
