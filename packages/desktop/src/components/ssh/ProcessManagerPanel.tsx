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
        <WifiOff className="w-10 h-10 text-red-300 dark:text-red-600 mb-3" />
        <p className="text-sm text-red-600 dark:text-red-400 text-center font-medium">
          Connection Error
        </p>
        <p className="text-xs text-gray-500 dark:text-gray-400 text-center mt-1">
          {errorMessage || "Unable to reach the remote agent. Will retry on next refresh."}
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Process Manager">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-gray-100 dark:border-gray-700">
        <h3 className="text-xs font-semibold text-gray-600 dark:text-gray-300 flex items-center gap-1.5">
          <Activity className="w-4 h-4" />
          Processes ({filteredProcesses.length})
        </h3>
        <button
          onClick={() => setShowFilters(!showFilters)}
          className={clsx(
            "flex items-center gap-1 px-2 py-1 text-xs rounded",
            showFilters
              ? "text-blue-600 dark:text-blue-400 bg-blue-50 dark:bg-blue-900/20"
              : "text-gray-500 dark:text-gray-400 hover:bg-gray-100 dark:hover:bg-gray-700",
          )}
          aria-label="Toggle filters"
        >
          <Search className="w-3 h-3" />
          Filter
        </button>
      </div>

      {/* Filters */}
      {showFilters && (
        <div className="px-3 py-2 border-b border-gray-100 dark:border-gray-700 grid grid-cols-2 gap-2">
          <div>
            <label htmlFor="proc-filter-name" className="text-[10px] text-gray-500 dark:text-gray-400">
              Name
            </label>
            <input
              id="proc-filter-name"
              type="text"
              value={filters.name}
              onChange={(e) => updateFilter("name", e.target.value)}
              placeholder="Filter by name"
              className="w-full px-2 py-0.5 text-xs border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800"
            />
          </div>
          <div>
            <label htmlFor="proc-filter-user" className="text-[10px] text-gray-500 dark:text-gray-400">
              User
            </label>
            <input
              id="proc-filter-user"
              type="text"
              value={filters.user}
              onChange={(e) => updateFilter("user", e.target.value)}
              placeholder="Filter by user"
              className="w-full px-2 py-0.5 text-xs border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800"
            />
          </div>
          <div>
            <label htmlFor="proc-filter-cpu" className="text-[10px] text-gray-500 dark:text-gray-400">
              Min CPU%
            </label>
            <input
              id="proc-filter-cpu"
              type="number"
              min={0}
              max={100}
              step={0.1}
              value={filters.minCpu}
              onChange={(e) => updateFilter("minCpu", e.target.value)}
              placeholder="0"
              className="w-full px-2 py-0.5 text-xs border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800"
            />
          </div>
          <div>
            <label htmlFor="proc-filter-mem" className="text-[10px] text-gray-500 dark:text-gray-400">
              Min Memory%
            </label>
            <input
              id="proc-filter-mem"
              type="number"
              min={0}
              max={100}
              step={0.1}
              value={filters.minMemory}
              onChange={(e) => updateFilter("minMemory", e.target.value)}
              placeholder="0"
              className="w-full px-2 py-0.5 text-xs border border-gray-300 dark:border-gray-600 rounded bg-white dark:bg-gray-800"
            />
          </div>
        </div>
      )}

      {/* Process list */}
      <div className="flex-1 overflow-y-auto">
        {filteredProcesses.length === 0 ? (
          <p className="text-xs text-gray-500 dark:text-gray-400 italic text-center py-8">
            {processes.length === 0 ? "No processes found." : "No processes match the current filters."}
          </p>
        ) : (
          <ul role="list" className="divide-y divide-gray-100 dark:divide-gray-800">
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
        isHighMemory && "bg-red-50 dark:bg-red-900/10",
      )}
    >
      <div className="flex-1 min-w-0 space-y-0.5">
        <div className="flex items-center gap-2">
          {isHighMemory && (
            <AlertTriangle className="w-3.5 h-3.5 text-red-500 shrink-0" aria-label="High memory usage warning" />
          )}
          <span className="text-sm font-medium text-gray-800 dark:text-gray-200 truncate">
            {proc.name}
          </span>
          <span className="text-[10px] text-gray-400 dark:text-gray-500">
            PID {proc.pid}
          </span>
        </div>
        <div className="flex items-center gap-3 text-[10px] text-gray-500 dark:text-gray-400">
          <span className="flex items-center gap-0.5" title="CPU usage">
            <Cpu className="w-3 h-3" />
            {formatCpuPercent(proc.cpuPercent)}
          </span>
          <span
            className={clsx(
              "flex items-center gap-0.5",
              isHighMemory && "text-red-600 dark:text-red-400 font-medium",
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
          className="p-1 text-yellow-600 dark:text-yellow-400 hover:bg-yellow-50 dark:hover:bg-yellow-900/20 rounded"
          aria-label={`Send SIGTERM to ${proc.name}`}
          title="SIGTERM (graceful)"
        >
          <XCircle className="w-3.5 h-3.5" />
        </button>
        <button
          onClick={() => onSignal(proc.pid, "SIGKILL")}
          className="p-1 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/20 rounded"
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
