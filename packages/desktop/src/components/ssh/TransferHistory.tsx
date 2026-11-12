import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
/**
 * TransferHistory — paginated list of past transfers with search, sort,
 * detail view, and aggregate statistics.
 *
 * 50 entries per page, searchable by file name/host, sortable by
 * date, size, duration, speed. Includes aggregate stats by day/week/month.
 */

import clsx from "clsx";
import {
  ArrowDownCircle,
  ArrowUpCircle,
  ArrowUpDown,
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  HardDrive,
  Inbox,
  Search,
  X,
  XCircle,
  Zap,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import type { TransferRecord, TransferStats } from "../../../electron/ssh-types";
import { formatBytes, formatSpeed } from "./TransferProgressBar";

// ── Types ────────────────────────────────────────────────────────

export interface TransferHistoryProps {
  /** SSH profile ID to query history for */
  profileId: string;
}

type SortField = "timestamp" | "fileSize" | "duration" | "averageSpeed";
type SortDir = "asc" | "desc";
type StatsPeriod = "day" | "week" | "month";

interface HistoryQuery {
  page: number;
  pageSize: number;
  search?: string;
  sortBy?: SortField;
  sortDir?: SortDir;
  profileId?: string;
}

// ── Constants ────────────────────────────────────────────────────

const PAGE_SIZE = 50;

const SORT_LABELS: Record<SortField, string> = {
  timestamp: "Date",
  fileSize: "Size",
  duration: "Duration",
  averageSpeed: "Speed",
};

// ── Component ────────────────────────────────────────────────────

export function TransferHistory({ profileId }: TransferHistoryProps) {
  const [entries, setEntries] = useState<TransferRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [search, setSearch] = useState("");
  const [sortBy, setSortBy] = useState<SortField>("timestamp");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [stats, setStats] = useState<TransferStats | null>(null);
  const [statsPeriod, setStatsPeriod] = useState<StatsPeriod>("week");
  const [selectedEntry, setSelectedEntry] = useState<TransferRecord | null>(null);
  const [isLoading, setIsLoading] = useState(false);

  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  // Fetch history entries
  const fetchHistory = useCallback(async () => {
    setIsLoading(true);
    try {
      const query: HistoryQuery = {
        page,
        pageSize: PAGE_SIZE,
        sortBy,
        sortDir,
        profileId,
      };
      if (search.trim()) query.search = search.trim();

      const result = await window.crab.ssh.transferHistory(query);
      setEntries(result.entries);
      setTotal(result.total);
    } catch {
      setEntries([]);
      setTotal(0);
    } finally {
      setIsLoading(false);
    }
  }, [page, search, sortBy, sortDir, profileId]);

  // Fetch aggregate stats
  const fetchStats = useCallback(async () => {
    try {
      const result = await window.crab.ssh.transferStats(profileId, statsPeriod);
      setStats(result);
    } catch {
      setStats(null);
    }
  }, [profileId, statsPeriod]);

  useEffect(() => { fetchHistory(); }, [fetchHistory]);
  useEffect(() => { fetchStats(); }, [fetchStats]);

  // Reset page when search or sort changes
  useEffect(() => { setPage(1); }, [search, sortBy, sortDir]);

  const handleSort = useCallback((field: SortField) => {
    setSortBy((prev) => {
      if (prev === field) {
        setSortDir((d) => (d === "asc" ? "desc" : "asc"));
      } else {
        setSortDir("desc");
      }
      return field;
    });
  }, []);

  const handleSearch = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    setSearch(e.target.value);
  }, []);

  // Empty state
  if (!isLoading && entries.length === 0 && !search) {
    return (
      <div className="flex flex-col items-center justify-center h-full py-12 px-4" role="region" aria-label="Transfer history">
        <Inbox className="w-10 h-10 text-muted-foreground text-muted-foreground mb-3" />
        <p className="text-sm text-muted-foreground text-muted-foreground text-center">
          No transfers have been recorded yet.
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Transfer history">
      {/* Stats summary */}
      {stats && (
        <StatsBar stats={stats} period={statsPeriod} onPeriodChange={setStatsPeriod} />
      )}

      {/* Search and sort controls */}
      <div className="flex items-center gap-2 px-3 py-2 border-b border-border border-border">
        <div className="relative flex-1">
          <Search className="absolute left-2 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-muted-foreground" />
          <Input
            type="text"
            value={search}
            onChange={handleSearch}
            placeholder="Search by file name or host…"
            className="w-full pl-7 pr-2 py-1.5 text-xs rounded-md border border-border border-border bg-background bg-background focus:outline-none focus:ring-1 focus:ring-info"
            aria-label="Search transfer history"
          />
        </div>
        <div className="flex items-center gap-1">
          {(Object.keys(SORT_LABELS) as SortField[]).map((field) => (
            <Button
              variant="ghost"
              size="compact"
              key={field}
              type="button"
              onClick={() => handleSort(field)}
              className={clsx(
                "px-2 py-1 text-xs rounded transition-colors",
                sortBy === field
                  ? "bg-info/10 bg-info/30 text-info text-info font-medium"
                  : "text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card",
              )}
              aria-label={`Sort by ${SORT_LABELS[field]}`}
              title={`Sort by ${SORT_LABELS[field]} ${sortBy === field ? (sortDir === "asc" ? "↑" : "↓") : ""}`}
            >
              {SORT_LABELS[field]}
              {sortBy === field && (
                <ArrowUpDown size={10} className="inline ml-0.5" />
              )}
            </Button>
          ))}
        </div>
      </div>

      {/* Entry list */}
      <div className="flex-1 overflow-y-auto">
        {isLoading ? (
          <div className="flex items-center justify-center py-8">
            <span className="text-xs text-muted-foreground text-muted-foreground">
              Loading…
            </span>
          </div>
        ) : entries.length === 0 ? (
          <div className="flex flex-col items-center justify-center py-8">
            <p className="text-xs text-muted-foreground text-muted-foreground">
              No results for &ldquo;{search}&rdquo;
            </p>
          </div>
        ) : (
          <ul role="list" className="divide-y divide-border divide-border">
            {entries.map((entry) => (
              <HistoryRow
                key={entry.id}
                entry={entry}
                onClick={() => setSelectedEntry(entry)}
              />
            ))}
          </ul>
        )}
      </div>

      {/* Pagination */}
      <div className="flex items-center justify-between px-3 py-2 border-t border-border border-border">
        <span className="text-xs text-muted-foreground text-muted-foreground">
          {total} total · Page {page} of {totalPages}
        </span>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            disabled={page <= 1}
            onClick={() => setPage((p) => Math.max(1, p - 1))}
            className="p-1 rounded hover:bg-card hover:bg-card disabled:opacity-30 disabled:cursor-not-allowed"
            aria-label="Previous page"
          >
            <ChevronLeft size={14} />
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            disabled={page >= totalPages}
            onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
            className="p-1 rounded hover:bg-card hover:bg-card disabled:opacity-30 disabled:cursor-not-allowed"
            aria-label="Next page"
          >
            <ChevronRight size={14} />
          </Button>
        </div>
      </div>

      {/* Detail view modal */}
      {selectedEntry && (
        <HistoryDetail entry={selectedEntry} onClose={() => setSelectedEntry(null)} />
      )}
    </div>
  );
}

// ── Stats Bar ────────────────────────────────────────────────────

interface StatsBarProps {
  stats: TransferStats;
  period: StatsPeriod;
  onPeriodChange: (period: StatsPeriod) => void;
}

function StatsBar({ stats, period, onPeriodChange }: StatsBarProps) {
  const failureRate = stats.successCount + stats.failureCount > 0
    ? ((stats.failureCount / (stats.successCount + stats.failureCount)) * 100).toFixed(1)
    : "0.0";

  return (
    <div className="px-3 py-2 border-b border-border border-border bg-background bg-card/50">
      <div className="flex items-center justify-between mb-1.5">
        <span className="text-xs font-semibold text-muted-foreground text-muted-foreground">
          Statistics
        </span>
        <div className="flex items-center gap-1">
          {(["day", "week", "month"] as StatsPeriod[]).map((p) => (
            <Button
              variant="ghost"
              size="compact"
              key={p}
              type="button"
              onClick={() => onPeriodChange(p)}
              className={clsx(
                "px-1.5 py-0.5 text-[10px] rounded capitalize",
                period === p
                  ? "bg-info/10 bg-info/30 text-info text-info font-medium"
                  : "text-muted-foreground text-muted-foreground hover:text-muted-foreground hover:text-muted-foreground",
              )}
            >
              {p}
            </Button>
          ))}
        </div>
      </div>
      <div className="grid grid-cols-4 gap-3">
        <StatItem icon={<HardDrive size={12} />} label="Uploaded" value={formatBytes(stats.totalUploaded)} />
        <StatItem icon={<HardDrive size={12} />} label="Downloaded" value={formatBytes(stats.totalDownloaded)} />
        <StatItem icon={<Zap size={12} />} label="Avg Speed" value={formatSpeed(stats.averageSpeed)} />
        <StatItem
          icon={
            stats.failureCount > 0 ? (
              <XCircle size={12} />
            ) : (
              <CheckCircle2 size={12} />
            )
          }
          label="Success/Fail"
          value={`${stats.successCount}/${stats.failureCount} (${failureRate}%)`}
        />
      </div>
    </div>
  );
}

function StatItem({ icon, label, value }: { icon: React.ReactNode; label: string; value: string }) {
  return (
    <div className="flex flex-col gap-0.5">
      <div className="flex items-center gap-1 text-muted-foreground text-muted-foreground">
        {icon}
        <span className="text-[10px]">{label}</span>
      </div>
      <span className="text-xs font-medium text-foreground text-foreground truncate">
        {value}
      </span>
    </div>
  );
}

// ── History Row ──────────────────────────────────────────────────

interface HistoryRowProps {
  entry: TransferRecord;
  onClick: () => void;
}

function HistoryRow({ entry, onClick }: HistoryRowProps) {
  const fileName = entry.localPath.split("/").pop() ?? entry.localPath;
  const date = new Date(entry.timestamp);
  const DirectionIcon = entry.direction === "upload" ? ArrowUpCircle : ArrowDownCircle;

  return (
    <li
      className="flex items-center gap-2 px-3 py-2 hover:bg-background hover:bg-card/50 cursor-pointer"
      onClick={onClick}
      role="button"
      tabIndex={0}
      onKeyDown={(e) => { if (e.key === "Enter") onClick(); }}
    >
      <DirectionIcon
        className={clsx(
          "w-4 h-4 shrink-0",
          entry.direction === "upload"
            ? "text-info text-info"
            : "text-success text-success",
        )}
      />
      <div className="flex-1 min-w-0">
        <p className="text-sm text-foreground text-foreground truncate">
          {fileName}
        </p>
        <p className="text-xs text-muted-foreground text-muted-foreground">
          {date.toLocaleDateString()}{" "}
          {date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
        </p>
      </div>
      <div className="flex items-center gap-3 text-xs text-muted-foreground text-muted-foreground shrink-0">
        <span>{formatBytes(entry.fileSize)}</span>
        <span>{formatSpeed(entry.averageSpeed)}</span>
        <span
          className={clsx(
            "px-1.5 py-0.5 rounded-full text-[10px] font-medium",
            entry.status === "completed"
              ? "bg-success/10 text-success bg-success/30 text-success"
              : "bg-destructive/10 text-destructive bg-destructive/30 text-destructive",
          )}
        >
          {entry.status}
        </span>
      </div>
    </li>
  );
}

// ── Detail View ──────────────────────────────────────────────────

interface HistoryDetailProps {
  entry: TransferRecord;
  onClose: () => void;
}

function HistoryDetail({ entry, onClose }: HistoryDetailProps) {
  const startTime = new Date(entry.timestamp);
  const endTime = new Date(entry.timestamp + entry.duration);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent
        aria-describedby={undefined}
        showCloseButton={false}
        className={
          "w-full max-w-md mx-4 bg-background bg-card rounded-xl shadow-xl border border-border border-border" +
          " p-0 gap-0"
        }
      >
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-3 border-b border-border border-border">
          <DialogTitle className="text-sm font-semibold text-foreground text-foreground">
            Transfer Details
          </DialogTitle>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={onClose}
            className="p-1 rounded hover:bg-card hover:bg-card text-muted-foreground"
            aria-label="Close"
          >
            <X size={16} />
          </Button>
        </div>

        {/* Body */}
        <div className="px-5 py-4 space-y-3">
          <DetailRow label="File" value={entry.localPath.split("/").pop() ?? entry.localPath} />
          <DetailRow label="Local Path" value={entry.localPath} mono />
          <DetailRow label="Remote Path" value={entry.remotePath} mono />
          <DetailRow label="Direction" value={entry.direction} />
          <DetailRow label="Status" value={entry.status} />
          <DetailRow label="Size" value={formatBytes(entry.fileSize)} />
          {entry.compressedSize !== undefined && (
            <DetailRow label="Compressed Size" value={formatBytes(entry.compressedSize)} />
          )}
          <DetailRow label="Duration" value={formatDuration(entry.duration)} />
          <DetailRow label="Average Speed" value={formatSpeed(entry.averageSpeed)} />
          <DetailRow label="Start Time" value={startTime.toLocaleString()} />
          <DetailRow label="End Time" value={endTime.toLocaleString()} />
          <DetailRow label="Retry Count" value={String(entry.retryCount)} />
          <DetailRow label="Cipher Suite" value={entry.cipherSuite} />
          {entry.deltaSavingsPercent !== undefined && (
            <DetailRow label="Delta Savings" value={`${entry.deltaSavingsPercent.toFixed(1)}%`} />
          )}
          {entry.error && <DetailRow label="Error" value={entry.error} error />}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function DetailRow({ label, value, mono, error }: { label: string; value: string; mono?: boolean; error?: boolean }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <span className="text-xs text-muted-foreground text-muted-foreground shrink-0">
        {label}
      </span>
      <span
        className={clsx(
          "text-xs text-right truncate max-w-[60%]",
          mono && "font-mono",
          error ? "text-destructive text-destructive" : "text-foreground text-foreground",
        )}
        title={value}
      >
        {value}
      </span>
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const secs = seconds % 60;
  if (minutes < 60) return secs > 0 ? `${minutes}m ${secs}s` : `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const mins = minutes % 60;
  return mins > 0 ? `${hours}h ${mins}m` : `${hours}h`;
}
