import { Input } from "@/components/ui/input";
/**
 * Process Manager Panel — displays remote processes with filtering,
 * signal controls, memory warning highlights, and connection error state.
 */

import { useState, useMemo, useCallback } from "react";
import {
  Activity,
  Cpu,
  MemoryStick,
  User,
  Clock,
  AlertTriangle,
  XCircle,
  Skull,
  WifiOff,
  Search,
} from "lucide-react";
import clsx from "clsx";
import type { RemoteProcess } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

interface ProcessManagerPanelProps {
  processes: RemoteProcess[];
  /** Whether the panel is in a connection error state */
  connectionError?: boolean;
  /** Error message when connection fails */
  errorMessage?: string;
  onSignal: (pid: number, signal: "SIGTERM" | "SIGKILL") => void;
}

interface ProcessFilters {
  name: string;
  user: string;
  minCpu: string;
  minMemory: string;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatRuntime(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  const hours = Math.floor(seconds / 3600);
  const mins = Math.floor((seconds % 3600) / 60);
  return `${hours}h ${mins}m`;
}

function formatMemoryPercent(percent: number): string {
  return `${percent.toFixed(1)}%`;
}

function formatCpuPercent(percent: number): string {
  return `${percent.toFixed(1)}%`;
}

function filterProcesses(processes: RemoteProcess[], filters: ProcessFilters): RemoteProcess[] {
  return processes.filter((proc) => {
    if (filters.name && !proc.name.toLowerCase().includes(filters.name.toLowerCase())) {
      return false;
    }
    if (filters.user && !proc.user.toLowerCase().includes(filters.user.toLowerCase())) {
      return false;
    }
    const minCpu = parseFloat(filters.minCpu);
    if (!isNaN(minCpu) && proc.cpuPercent < minCpu) {
      return false;
    }
    const minMem = parseFloat(filters.minMemory);
    if (!isNaN(minMem) && proc.memoryPercent < minMem) {
      return false;
    }
    return true;
  });
}

// ── Component ────────────────────────────────────────────────────

export function ProcessManagerPanel({
  processes,
  connectionError = false,
  errorMessage,
  onSignal,
}: ProcessManagerPanelProps) {
  const [filters, setFilters] = useState<ProcessFilters>({
    name: "",
    user: "",
    minCpu: "",
    minMemory: "",
  });
  const [showFilters, setShowFilters] = useState(false);

  const filteredProcesses = useMemo(
    () => filterProcesses(processes, filters),
    [processes, filters],
  );

  const updateFilter = useCallback(
    (key: keyof ProcessFilters, value: string) => {
      setFilters((prev) => ({ ...prev, [key]: value }));
    },
    [],
  );

  // Connection error state
  if (connectionError) {
    return (
      <div
        className="flex flex-col items-center justify-center h-full py-12 px-4"
        role="region"
        aria-label="Process Manager"
      >
        <WifiOff className="w-10 h-10 text-destructive text-destructive mb-3" />
        <p className="text-sm text-destructive text-destructive text-center font-medium">
          Connection Error
        </p>
        <p className="text-xs text-muted-foreground text-muted-foreground text-center mt-1">
          {errorMessage || "Unable to reach the remote agent. Will retry on next refresh."}
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Process Manager">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-border border-border">
        <h3 className="text-xs font-semibold text-muted-foreground text-muted-foreground flex items-center gap-1.5">
          <Activity className="w-4 h-4" />
          Processes ({filteredProcesses.length})
        </h3>
        <button
          onClick={() => setShowFilters(!showFilters)}
          className={clsx(
            "flex items-center gap-1 px-2 py-1 text-xs rounded",
            showFilters
              ? "text-info text-info bg-info/10 bg-info/20"
              : "text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card",
          )}
          aria-label="Toggle filters"
        >
          <Search className="w-3 h-3" />
          Filter
        </button>
      </div>

      {/* Filters */}
      {showFilters && (
        <div className="px-3 py-2 border-b border-border border-border grid grid-cols-2 gap-2">
          <div>
            <label htmlFor="proc-filter-name" className="text-[10px] text-muted-foreground text-muted-foreground">
              Name
            </label>
            <Input
              id="proc-filter-name"
              type="text"
              value={filters.name}
              onChange={(e) => updateFilter("name", e.target.value)}
              placeholder="Filter by name"
              className="w-full px-2 py-0.5 text-xs border border-border border-border rounded bg-background bg-card"
            />
          </div>
          <div>
            <label htmlFor="proc-filter-user" className="text-[10px] text-muted-foreground text-muted-foreground">
              User
            </label>
            <Input
              id="proc-filter-user"
              type="text"
              value={filters.user}
              onChange={(e) => updateFilter("user", e.target.value)}
              placeholder="Filter by user"
              className="w-full px-2 py-0.5 text-xs border border-border border-border rounded bg-background bg-card"
            />
          </div>
          <div>
            <label htmlFor="proc-filter-cpu" className="text-[10px] text-muted-foreground text-muted-foreground">
              Min CPU%
            </label>
            <Input
              id="proc-filter-cpu"
              type="number"
              min={0}
              max={100}
              step={0.1}
              value={filters.minCpu}
              onChange={(e) => updateFilter("minCpu", e.target.value)}
              placeholder="0"
              className="w-full px-2 py-0.5 text-xs border border-border border-border rounded bg-background bg-card"
            />
          </div>
          <div>
            <label htmlFor="proc-filter-mem" className="text-[10px] text-muted-foreground text-muted-foreground">
              Min Memory%
            </label>
            <Input
              id="proc-filter-mem"
              type="number"
              min={0}
              max={100}
              step={0.1}
              value={filters.minMemory}
              onChange={(e) => updateFilter("minMemory", e.target.value)}
              placeholder="0"
              className="w-full px-2 py-0.5 text-xs border border-border border-border rounded bg-background bg-card"
            />
          </div>
        </div>
      )}

      {/* Process list */}
      <div className="flex-1 overflow-y-auto">
        {filteredProcesses.length === 0 ? (
          <p className="text-xs text-muted-foreground text-muted-foreground italic text-center py-8">
            {processes.length === 0 ? "No processes found." : "No processes match the current filters."}
          </p>
        ) : (
          <ul role="list" className="divide-y divide-border divide-border">
            {filteredProcesses.map((proc) => (
              <ProcessRow key={proc.pid} process={proc} onSignal={onSignal} />
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

// ── Process Row ──────────────────────────────────────────────────

function ProcessRow({
  process: proc,
  onSignal,
}: {
  process: RemoteProcess;
  onSignal: (pid: number, signal: "SIGTERM" | "SIGKILL") => void;
}) {
  const isHighMemory = proc.memoryPercent > 90;

  return (
    <li
      className={clsx(
        "flex items-center justify-between px-3 py-2 group",
        isHighMemory && "bg-destructive/10 bg-destructive/10",
      )}
    >
      <div className="flex-1 min-w-0 space-y-0.5">
        <div className="flex items-center gap-2">
          {isHighMemory && (
            <AlertTriangle className="w-3.5 h-3.5 text-destructive shrink-0" aria-label="High memory usage warning" />
          )}
          <span className="text-sm font-medium text-foreground text-foreground truncate">
            {proc.name}
          </span>
          <span className="text-[10px] text-muted-foreground text-muted-foreground">
            PID {proc.pid}
          </span>
        </div>
        <div className="flex items-center gap-3 text-[10px] text-muted-foreground text-muted-foreground">
          <span className="flex items-center gap-0.5" title="CPU usage">
            <Cpu className="w-3 h-3" />
            {formatCpuPercent(proc.cpuPercent)}
          </span>
          <span
            className={clsx(
              "flex items-center gap-0.5",
              isHighMemory && "text-destructive text-destructive font-medium",
            )}
            title="Memory usage"
          >
            <MemoryStick className="w-3 h-3" />
            {formatMemoryPercent(proc.memoryPercent)}
          </span>
          <span className="flex items-center gap-0.5" title="User">
            <User className="w-3 h-3" />
            {proc.user}
          </span>
          <span className="flex items-center gap-0.5" title="Runtime">
            <Clock className="w-3 h-3" />
            {formatRuntime(proc.runtime)}
          </span>
        </div>
      </div>

      {/* Signal buttons */}
      <div className="flex items-center gap-1 shrink-0 opacity-0 group-hover:opacity-100 transition-opacity">
        <button
          onClick={() => onSignal(proc.pid, "SIGTERM")}
          className="p-1 text-warning text-warning hover:bg-warning/10 hover:bg-warning/20 rounded"
          aria-label={`Send SIGTERM to ${proc.name}`}
          title="SIGTERM (graceful)"
        >
          <XCircle className="w-3.5 h-3.5" />
        </button>
        <button
          onClick={() => onSignal(proc.pid, "SIGKILL")}
          className="p-1 text-destructive text-destructive hover:bg-destructive/10 hover:bg-destructive/20 rounded"
          aria-label={`Send SIGKILL to ${proc.name}`}
          title="SIGKILL (force)"
        >
          <Skull className="w-3.5 h-3.5" />
        </button>
      </div>
    </li>
  );
}

// ── Exports for testing ──────────────────────────────────────────

export { filterProcesses, formatRuntime };
