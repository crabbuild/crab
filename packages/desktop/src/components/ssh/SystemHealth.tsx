/**
 * Compact system health display for SSH connection cards and panels.
 *
 * Shows disk usage bars per mount point, memory usage bar, and GPU
 * cards (if available) with utilization, temperature, memory, and
 * running processes. Designed to fit within a ConnectionCard or
 * standalone panel without taking excessive vertical space.
 *
 * Uses Tailwind CSS with dark: variants for theme support.
 * Accessible: ARIA roles on progress bars, screen reader text.
 */

import clsx from "clsx";
import { Cpu, HardDrive, MemoryStick, Loader2 } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────

export interface DiskInfo {
  path: string;
  total: number;
  free: number;
  usedPercent: number;
}

export interface MemoryInfo {
  total: number;
  free: number;
  cached: number;
  usedPercent: number;
}

export interface GpuProcess {
  pid: number;
  name: string;
  memUsed: number;
}

export interface GpuInfo {
  index: number;
  name: string;
  utilization: number;
  memoryUsed: number;
  memoryTotal: number;
  temperature: number;
  processes?: GpuProcess[];
}

export interface SystemHealthData {
  disk: DiskInfo[];
  memory: MemoryInfo;
  gpus?: GpuInfo[] | null;
  loadAvg?: [number, number, number];
  collectedAt?: number;
}

export interface SystemHealthProps {
  /** Health data from the monitor. Pass undefined for loading state. */
  health: SystemHealthData | undefined;
  /** Compact mode renders a one-liner summary for connection cards. */
  compact?: boolean;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatBytes(bytes: number): string {
  if (bytes >= 1_099_511_627_776) {
    return `${(bytes / 1_099_511_627_776).toFixed(1)} TB`;
  }
  if (bytes >= 1_073_741_824) {
    return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  }
  if (bytes >= 1_048_576) {
    return `${(bytes / 1_048_576).toFixed(0)} MB`;
  }
  return `${(bytes / 1024).toFixed(0)} KB`;
}

/** Returns Tailwind classes for the progress bar fill based on usage percent. */
function barColorClass(percent: number): string {
  if (percent >= 90) return "bg-destructive bg-destructive";
  if (percent >= 80) return "bg-warning bg-warning";
  return "bg-success bg-success";
}

/** Returns Tailwind text color classes for temperature. */
function tempColorClass(temp: number): string {
  if (temp > 85) return "text-destructive text-destructive";
  if (temp >= 70) return "text-warning text-warning";
  return "text-success text-success";
}

/** Returns a severity label for screen readers. */
function severityLabel(percent: number): string {
  if (percent >= 90) return "critical";
  if (percent >= 80) return "warning";
  return "healthy";
}

// ── Sub-components ───────────────────────────────────────────────

function UsageBar({
  percent,
  label,
  detail,
  icon: Icon,
}: {
  percent: number;
  label: string;
  detail: string;
  icon?: typeof HardDrive;
}) {
  const clampedPercent = Math.min(Math.max(percent, 0), 100);

  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between text-xs">
        <span className="flex items-center gap-1.5 font-medium text-foreground">
          {Icon && <Icon size={12} className="text-muted-foreground" aria-hidden="true" />}
          {label}
        </span>
        <span className="text-muted-foreground">{detail}</span>
      </div>
      <div
        className="h-2 w-full overflow-hidden rounded-full bg-background bg-card"
        role="progressbar"
        aria-valuenow={clampedPercent}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={`${label}: ${clampedPercent}% used, ${severityLabel(clampedPercent)}`}
      >
        <div
          className={clsx(
            "h-full rounded-full transition-all duration-300 ease-out",
            barColorClass(clampedPercent),
          )}
          style={{ width: `${clampedPercent}%` }}
        />
      </div>
      {/* Screen reader text for percentage */}
      <span className="sr-only">
        {label} usage: {clampedPercent}% ({severityLabel(clampedPercent)})
      </span>
    </div>
  );
}

function GpuCard({ gpu }: { gpu: GpuInfo }) {
  const shortName = gpu.name
    .replace(/NVIDIA\s*/i, "")
    .replace(/GeForce\s*/i, "")
    .trim();

  const memPercent =
    gpu.memoryTotal > 0
      ? Math.round((gpu.memoryUsed / gpu.memoryTotal) * 100)
      : 0;

  return (
    <div
      className="rounded-md border border-border bg-background/50 bg-card/50 p-2.5 space-y-2"
      aria-label={`GPU ${gpu.index}: ${gpu.name}`}
    >
      {/* GPU header: name + temperature */}
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium text-foreground truncate">
          {shortName}
        </span>
        <span
          className={clsx("text-xs font-medium", tempColorClass(gpu.temperature))}
          aria-label={`Temperature: ${gpu.temperature}°C`}
        >
          {gpu.temperature}°C
        </span>
      </div>

      {/* Utilization bar */}
      <div className="space-y-0.5">
        <div className="flex items-center justify-between text-[11px] text-muted-foreground">
          <span>Utilization</span>
          <span>{gpu.utilization}%</span>
        </div>
        <div
          className="h-1.5 w-full overflow-hidden rounded-full bg-background bg-card"
          role="progressbar"
          aria-valuenow={gpu.utilization}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-label={`GPU ${gpu.index} utilization: ${gpu.utilization}%`}
        >
          <div
            className={clsx(
              "h-full rounded-full transition-all duration-300 ease-out",
              barColorClass(gpu.utilization),
            )}
            style={{ width: `${gpu.utilization}%` }}
          />
        </div>
      </div>

      {/* Memory usage */}
      <div className="flex items-center justify-between text-[11px] text-muted-foreground">
        <span>Memory</span>
        <span>
          {formatBytes(gpu.memoryUsed)} / {formatBytes(gpu.memoryTotal)}{" "}
          ({memPercent}%)
        </span>
      </div>

      {/* Running processes */}
      {gpu.processes && gpu.processes.length > 0 && (
        <div className="border-t border-border pt-1.5">
          <span className="text-[10px] font-medium text-muted-foreground uppercase tracking-wide">
            Processes
          </span>
          <ul className="mt-0.5 space-y-0.5" aria-label={`GPU ${gpu.index} processes`}>
            {gpu.processes.slice(0, 5).map((proc) => (
              <li
                key={proc.pid}
                className="flex items-center justify-between text-[11px] text-muted-foreground"
              >
                <span className="truncate max-w-[60%]" title={proc.name}>
                  {proc.name}
                </span>
                <span className="shrink-0 ml-2">{formatBytes(proc.memUsed)}</span>
              </li>
            ))}
            {gpu.processes.length > 5 && (
              <li className="text-[10px] text-muted-foreground italic">
                +{gpu.processes.length - 5} more
              </li>
            )}
          </ul>
        </div>
      )}
    </div>
  );
}

// ── Loading State ────────────────────────────────────────────────

function LoadingState() {
  return (
    <div
      className="flex items-center gap-2 py-3 text-xs text-muted-foreground"
      role="status"
      aria-label="Loading system health data"
    >
      <Loader2 size={14} className="animate-spin" aria-hidden="true" />
      <span>Loading health data…</span>
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────

export function SystemHealth({ health, compact = false }: SystemHealthProps) {
  // Loading state: health data not yet available
  if (!health) {
    return <LoadingState />;
  }

  const memUsed = health.memory.total - health.memory.free;

  // Compact mode: one-liner summary for connection cards
  if (compact) {
    const parts: string[] = [];
    if (health.gpus && health.gpus.length > 0) {
      const avgUtil = Math.round(
        health.gpus.reduce((sum, g) => sum + g.utilization, 0) / health.gpus.length,
      );
      parts.push(`${health.gpus.length}x GPU ${avgUtil}%`);
    }
    if (health.disk.length > 0) {
      const mainDisk = health.disk.reduce((a, b) => (a.total > b.total ? a : b));
      parts.push(`${formatBytes(mainDisk.free)} free`);
    }
    parts.push(`${formatBytes(memUsed)}/${formatBytes(health.memory.total)} RAM`);

    return (
      <div className="text-xs text-muted-foreground" aria-label="System health summary">
        {parts.join(" · ")}
      </div>
    );
  }

  // Full display
  return (
    <div className="space-y-3" aria-label="System health details">
      {/* Disk usage — one bar per mount point */}
      {health.disk.length > 0 && (
        <div className="space-y-2">
          {health.disk.map((disk) => (
            <UsageBar
              key={disk.path}
              percent={disk.usedPercent}
              label={`Disk (${disk.path})`}
              detail={`${formatBytes(disk.free)} free of ${formatBytes(disk.total)}`}
              icon={HardDrive}
            />
          ))}
        </div>
      )}

      {/* Memory usage */}
      <UsageBar
        percent={health.memory.usedPercent}
        label="Memory"
        detail={`${formatBytes(memUsed)} / ${formatBytes(health.memory.total)}`}
        icon={MemoryStick}
      />

      {/* GPU section */}
      {health.gpus && health.gpus.length > 0 ? (
        <div className="space-y-2">
          <div className="flex items-center gap-1.5 text-xs font-medium text-foreground">
            <Cpu size={12} className="text-muted-foreground" aria-hidden="true" />
            <span>GPUs ({health.gpus.length})</span>
          </div>
          <div className="grid gap-2 sm:grid-cols-2" aria-label="GPU status">
            {health.gpus.map((gpu) => (
              <GpuCard key={gpu.index} gpu={gpu} />
            ))}
          </div>
        </div>
      ) : (
        <div className="flex items-center gap-1.5 text-xs text-muted-foreground py-1">
          <Cpu size={12} aria-hidden="true" />
          <span>No GPU detected</span>
        </div>
      )}

      {/* Load average (if available) */}
      {health.loadAvg && (
        <div className="flex items-center gap-2 text-[11px] text-muted-foreground" aria-label="Load average">
          <span className="font-medium">Load:</span>
          <span>{health.loadAvg.map((v) => v.toFixed(2)).join(" / ")}</span>
        </div>
      )}
    </div>
  );
}
