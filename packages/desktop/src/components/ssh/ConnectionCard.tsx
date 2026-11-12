import { Button } from "@/components/ui/button";
import clsx from "clsx";
import {
  AlertTriangle,
  Circle,
  Loader2,
  Pencil,
  Pin,
  PinOff,
  Plug,
  Terminal,
  Trash2,
  Unplug,
  FolderOpen,
  ArrowRightLeft,
  GripVertical,
  Bot,
} from "lucide-react";
import { SshConnectionState } from "../../state";
import type { SshProfile } from "./ssh-profile-types";
import { SystemHealth, type SystemHealthData } from "./SystemHealth";

interface ConnectionCardProps {
  profile: SshProfile;
  connectionState?: SshConnectionState;
  focused?: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
  onEdit: () => void;
  onDelete: () => void;
  onTogglePin: () => void;
  onOpenTerminal: () => void;
}

function statusColor(status: SshConnectionState["status"] | undefined): string {
  switch (status) {
    case "connected":
      return "text-success text-success";
    case "connecting":
    case "disconnecting":
    case "deploying":
    case "reconnecting":
      return "text-warning text-warning";
    case "error":
      return "text-destructive text-destructive";
    default:
      return "text-muted-foreground text-muted-foreground";
  }
}

function statusLabel(status: SshConnectionState["status"] | undefined): string {
  switch (status) {
    case "connected":
      return "Connected";
    case "connecting":
      return "Connecting...";
    case "disconnecting":
      return "Disconnecting...";
    case "deploying":
      return "Deploying agent...";
    case "reconnecting":
      return "Reconnecting...";
    case "error":
      return "Error";
    default:
      return "Disconnected";
  }
}

function formatRelativeTime(timestamp: number | undefined): string {
  if (!timestamp) return "Never";
  const diff = Date.now() - timestamp;
  const minutes = Math.floor(diff / 60000);
  if (minutes < 1) return "Just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/** Convert the flat SshConnectionState.health into SystemHealthData for the SystemHealth component. */
function toSystemHealthData(health: NonNullable<SshConnectionState["health"]>): SystemHealthData {
  const diskUsedPercent = health.diskTotal > 0
    ? Math.round(((health.diskTotal - health.diskFree) / health.diskTotal) * 100)
    : 0;

  return {
    disk: [{ path: "/", total: health.diskTotal, free: health.diskFree, usedPercent: diskUsedPercent }],
    memory: {
      total: health.memTotal,
      free: health.memFree,
      cached: 0,
      usedPercent: health.memTotal > 0
        ? Math.round(((health.memTotal - health.memFree) / health.memTotal) * 100)
        : 0,
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

/**
 * Per-connection card showing label, host, status, health summary,
 * and contextual action buttons.
 *
 * Connected cards show server info, health metrics, and action buttons
 * for terminal, browse, forwards, and disconnect.
 *
 * Disconnected cards show last connected time and connect/edit/delete buttons.
 *
 * Accessibility: ARIA labels on status indicators, keyboard-focusable
 * action buttons, screen reader announcements for state changes.
 */
export function ConnectionCard({
  profile,
  connectionState,
  focused,
  onConnect,
  onDisconnect,
  onEdit,
  onDelete,
  onTogglePin,
  onOpenTerminal,
}: ConnectionCardProps) {
  const status = connectionState?.status ?? "disconnected";
  const isConnected = status === "connected";
  const isConnecting = status === "disconnecting" || status === "connecting" || status === "deploying" || status === "reconnecting";

  // Convert health data for the SystemHealth compact component
  const healthData: SystemHealthData | undefined =
    isConnected && connectionState?.health
      ? toSystemHealthData(connectionState.health)
      : undefined;

  return (
    <div
      className={clsx(
        "group relative rounded-lg border bg-card p-4 transition-all",
        focused && "ring-2 ring-primary ring-offset-1 ring-offset-background",
        isConnected && "border-success/30 border-success/20",
        status === "error" && "border-destructive/30 border-destructive/20",
        !isConnected && status !== "error" && "border-border",
        "hover:border-primary/40 hover:shadow-sm",
      )}
      role="article"
      aria-label={`${profile.label} connection - ${statusLabel(status)}`}
    >
      {/* Drag handle */}
      <div
        className="absolute left-1 top-1/2 -translate-y-1/2 cursor-grab opacity-0 transition-opacity group-hover:opacity-60"
        aria-hidden="true"
      >
        <GripVertical size={14} className="text-muted-foreground" />
      </div>

      {/* Pin indicator */}
      {profile.pinned && (
        <div className="absolute -top-1.5 right-3" aria-label="Pinned connection">
          <Pin size={10} className="text-primary" />
        </div>
      )}

      <div className="flex items-start gap-3 pl-3">
        {/* Status dot */}
        <div className="mt-1 shrink-0" aria-hidden="true">
          {isConnecting ? (
            <Loader2 size={14} className={clsx("animate-spin", statusColor(status))} />
          ) : (
            <Circle size={10} className={clsx("fill-current", statusColor(status))} />
          )}
        </div>

        {/* Content */}
        <div className="min-w-0 flex-1">
          {/* Header row */}
          <div className="flex items-center gap-2">
            <h3 className="truncate text-sm font-semibold text-foreground">
              {profile.label}
            </h3>
            <span className="truncate text-xs text-muted-foreground">
              ({profile.host})
            </span>
            {/* Tags */}
            {profile.tags.length > 0 && (
              <div className="hidden items-center gap-1 sm:flex" aria-label="Tags">
                {profile.tags.slice(0, 3).map((tag) => (
                  <span
                    key={tag}
                    className="rounded-full bg-accent px-1.5 py-0.5 text-[10px] text-muted-foreground"
                  >
                    {tag}
                  </span>
                ))}
              </div>
            )}
          </div>

          {/* Status line */}
          <div className="mt-0.5 flex items-center gap-2 text-xs text-muted-foreground">
            <span className={statusColor(status)} role="status" aria-live="polite">
              {statusLabel(status)}
            </span>
            {isConnected && connectionState?.serverInfo && (
              <>
                <span aria-hidden="true">·</span>
                <span>{connectionState.serverInfo.os} {connectionState.serverInfo.arch}</span>
                <span aria-hidden="true">·</span>
                <span>Agent v{connectionState.serverInfo.agentVersion}</span>
              </>
            )}
            {!isConnected && !isConnecting && (
              <>
                <span aria-hidden="true">·</span>
                <span>Last connected {formatRelativeTime(profile.lastConnectedAt)}</span>
              </>
            )}
            {connectionState?.error && (
              <>
                <span aria-hidden="true">·</span>
                <span className="text-destructive text-destructive">{connectionState.error}</span>
              </>
            )}
          </div>

          {/* Health summary (connected only) */}
          {isConnected && healthData && (
            <div className="mt-1">
              <SystemHealth health={healthData} compact />
            </div>
          )}

          {/* Health alerts badge (connected only) */}
          {isConnected && connectionState?.healthAlerts && connectionState.healthAlerts.length > 0 && (
            <div
              className="mt-1.5 flex items-center gap-1.5"
              role="status"
              aria-label={`${connectionState.healthAlerts.length} health alert${connectionState.healthAlerts.length > 1 ? "s" : ""}`}
            >
              <span
                className={clsx(
                  "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium",
                  connectionState.healthAlerts.some((a) => a.severity === "critical")
                    ? "bg-destructive/15 text-destructive bg-destructive/15 text-destructive"
                    : "bg-warning/15 text-warning bg-warning/15 text-warning",
                )}
                title={connectionState.healthAlerts.map((a) => a.message).join("\n")}
              >
                <AlertTriangle size={10} aria-hidden="true" />
                {connectionState.healthAlerts.length === 1
                  ? connectionState.healthAlerts[0].message
                  : `${connectionState.healthAlerts.length} alerts`}
              </span>
            </div>
          )}

          {/* Detected agent CLIs (connected only) */}
          {isConnected && connectionState?.detectedAgents && connectionState.detectedAgents.some(a => a.available) && (
            <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground" aria-label="Available agent CLIs">
              <Bot size={11} className="shrink-0 text-muted-foreground" aria-hidden="true" />
              {connectionState.detectedAgents
                .filter(a => a.available)
                .map((agent) => (
                  <span
                    key={agent.name}
                    className="inline-flex items-center rounded bg-accent px-1.5 py-0.5 text-[10px] font-medium text-foreground"
                    title={agent.version ? `${agent.name} (${agent.version}) at ${agent.path}` : `${agent.name} at ${agent.path}`}
                  >
                    {agent.name}
                    {agent.version && (
                      <span className="ml-0.5 text-muted-foreground">{agent.version}</span>
                    )}
                  </span>
                ))}
            </div>
          )}

          {/* Latency indicator */}
          {isConnected && connectionState?.latencyMs != null && (
            <div
              className="mt-1 flex items-center gap-1.5 text-[10px]"
              aria-label={`Connection latency: ${connectionState.latencyMs}ms`}
              role="status"
            >
              <span
                className={clsx(
                  "inline-block h-2 w-2 rounded-full",
                  connectionState.latencyMs < 50 && "bg-success",
                  connectionState.latencyMs >= 50 && connectionState.latencyMs <= 200 && "bg-warning",
                  connectionState.latencyMs > 200 && "bg-destructive",
                )}
                aria-hidden="true"
              />
              <span
                className={clsx(
                  "font-medium",
                  connectionState.latencyMs < 50 && "text-success text-success",
                  connectionState.latencyMs >= 50 && connectionState.latencyMs <= 200 && "text-warning text-warning",
                  connectionState.latencyMs > 200 && "text-destructive text-destructive",
                )}
              >
                {connectionState.latencyMs}ms
              </span>
              {connectionState.highLatencyMode && (
                <span
                  className="ml-1 inline-flex items-center rounded-full bg-destructive/15 px-1.5 py-0.5 text-[10px] font-medium text-destructive bg-destructive/15 text-destructive"
                  role="status"
                  aria-label="High latency mode active"
                >
                  High latency mode
                </span>
              )}
            </div>
          )}
        </div>

        {/* Action buttons */}
        <div className="flex shrink-0 items-center gap-1">
          {isConnected && (
            <>
              <Button size="icon" variant="ghost"
                type="button"

                onClick={onOpenTerminal}
                title="Open terminal"
                aria-label={`Open terminal on ${profile.label}`}
              >
                <Terminal size={14} />
              </Button>
              <Button size="icon" variant="ghost"
                type="button"

                onClick={() => {
                  window.dispatchEvent(new CustomEvent("crab:open-ssh-file-browser", { detail: { profileId: profile.id, hostname: profile.host } }));
                }}
                title="Browse files"
                aria-label={`Browse files on ${profile.label}`}
              >
                <FolderOpen size={14} />
              </Button>
              <Button size="icon" variant="ghost"
                type="button"

                onClick={() => {
                  window.dispatchEvent(new CustomEvent("crab:open-ssh-port-forwards", { detail: { profileId: profile.id, hostname: profile.host } }));
                }}
                title="Port forwards"
                aria-label={`Manage port forwards for ${profile.label}`}
              >
                <ArrowRightLeft size={14} />
              </Button>
              <Button size="icon" variant="ghost"
                type="button"
                className="text-destructive hover:text-destructive text-destructive hover:text-destructive"
                onClick={onDisconnect}
                title="Disconnect"
                aria-label={`Disconnect from ${profile.label}`}
              >
                <Unplug size={14} />
              </Button>
            </>
          )}
          {!isConnected && !isConnecting && (
            <>
              <Button size="icon" variant="ghost"
                type="button"
                className="text-success hover:text-success text-success hover:text-success"
                onClick={onConnect}
                title="Connect"
                aria-label={`Connect to ${profile.label}`}
              >
                <Plug size={14} />
              </Button>
              <Button size="icon" variant="ghost"
                type="button"

                onClick={onEdit}
                title="Edit"
                aria-label={`Edit ${profile.label}`}
              >
                <Pencil size={14} />
              </Button>
              <Button size="icon" variant="ghost"
                type="button"
                className="text-destructive hover:text-destructive text-destructive hover:text-destructive"
                onClick={onDelete}
                title="Delete"
                aria-label={`Delete ${profile.label}`}
              >
                <Trash2 size={14} />
              </Button>
            </>
          )}
          {isConnecting && (
            <Button size="icon" variant="ghost"
              type="button"

              onClick={onDisconnect}
              title="Cancel"
              aria-label={`Cancel connection to ${profile.label}`}
            >
              <Unplug size={14} />
            </Button>
          )}
          {/* Pin toggle (always visible on hover) */}
          <Button size="icon" variant="ghost"
            type="button"
            className="opacity-0 transition-opacity group-hover:opacity-100"
            onClick={onTogglePin}
            title={profile.pinned ? "Unpin" : "Pin to top"}
            aria-label={profile.pinned ? `Unpin ${profile.label}` : `Pin ${profile.label} to top`}
          >
            {profile.pinned ? <PinOff size={12} /> : <Pin size={12} />}
          </Button>
        </div>
      </div>
    </div>
  );
}
