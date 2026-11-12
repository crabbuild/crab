import { Button } from "@/components/ui/button";
import clsx from "clsx";
import {
  AlertCircle,
  ArrowDown,
  ArrowUp,
  ArrowUpDown,
  CheckCircle2,
  Cloud,
  Download,
  GitBranch,
  Loader2,
  RefreshCw,
  WifiOff,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import type { ConnectionQuality } from "../../electron/ssh-types";
import { ConnectionQualityBar } from "../components/ssh/ConnectionQualityBar";
import { useWorkspaceRepoSelect } from "../hooks/useWorkspaceRepoSelect";
import type { Workspace } from "../state";

interface DashboardRow {
  name: string;
  relativePath: string;
  branch: string | null;
  dirtyCount: number | null;
  ahead: number | null;
  behind: number | null;
  level: 0 | 1 | 2;
  error: string | null;
  /** True when the repo lives on a remote host (repo_id starts with ssh://) */
  isRemote: boolean;
  /** SSH profile ID extracted from repo_id (e.g. "gpu-server") */
  sshHost: string | null;
}

type SortColumn = "name" | "branch" | "dirty" | "ahead" | "behind";
type SortDirection = "asc" | "desc";

interface Props {
  workspace: Workspace;
}

/**
 * Hook that tracks connection quality for all SSH profiles present in the workspace.
 * Fetches initial quality on mount and subscribes to quality change events.
 */
function useConnectionQuality(profileIds: string[]): Map<string, ConnectionQuality> {
  const [qualityMap, setQualityMap] = useState<Map<string, ConnectionQuality>>(new Map());

  useEffect(() => {
    if (profileIds.length === 0) return;

    const ssh = (window as any).crab?.ssh;
    if (!ssh) return;

    // Fetch initial quality for each profile
    for (const profileId of profileIds) {
      ssh.getQuality(profileId).then((q: ConnectionQuality | null) => {
        if (q) {
          setQualityMap((prev) => {
            const next = new Map(prev);
            next.set(profileId, q);
            return next;
          });
        }
      }).catch(() => {
        // Quality unavailable — ignore
      });
    }

    // Subscribe to quality change events
    const unsubscribe = ssh.onQualityChanged?.((payload: { profileId: string; quality: ConnectionQuality }) => {
      if (profileIds.includes(payload.profileId)) {
        setQualityMap((prev) => {
          const next = new Map(prev);
          next.set(payload.profileId, payload.quality);
          return next;
        });
      }
    });

    return () => {
      unsubscribe?.();
    };
  }, [profileIds.join(",")]); // eslint-disable-line react-hooks/exhaustive-deps

  return qualityMap;
}

/**
 * Aggregated workspace dashboard showing all repos in a sortable table
 * with color-coded status, bulk Fetch All / Pull All actions, and
 * per-repo error display.
 */
export function WorkspaceDashboardView({ workspace }: Props) {
  const selectRepo = useWorkspaceRepoSelect();

  const [sortColumn, setSortColumn] = useState<SortColumn>("name");
  const [sortDirection, setSortDirection] = useState<SortDirection>("asc");
  const [fetchingAll, setFetchingAll] = useState(false);
  const [pullingAll, setPullingAll] = useState(false);
  const [bulkErrors, setBulkErrors] = useState<Map<string, string>>(new Map());

  const rows: DashboardRow[] = useMemo(
    () =>
      workspace.repos.map((entry) => {
        const isRemote = workspace.kind === "remote";
        const sshHost = isRemote
          ? (workspace.profileLabel ?? workspace.profileId ?? null)
          : null;
        return {
          name: entry.name,
          relativePath: entry.relativePath,
          branch: entry.branch,
          dirtyCount: entry.dirtyCount,
          ahead: entry.ahead,
          behind: entry.behind,
          level: entry.level,
          error: entry.error,
          isRemote,
          sshHost,
        };
      }),
    [workspace.repos, workspace.kind, workspace.profileLabel, workspace.profileId],
  );

  // Extract unique SSH profile IDs for quality monitoring
  const remoteProfileIds = useMemo(() => {
    if (workspace.kind !== "remote" || !workspace.profileId) return [];
    return [workspace.profileId];
  }, [workspace.kind, workspace.profileId]);

  const hasRemoteRepos = remoteProfileIds.length > 0;
  const qualityMap = useConnectionQuality(remoteProfileIds);

  const sortedRows = useMemo(() => {
    const sorted = [...rows];
    sorted.sort((a, b) => {
      let cmp = 0;
      switch (sortColumn) {
        case "name":
          cmp = a.name.localeCompare(b.name);
          break;
        case "branch":
          cmp = (a.branch ?? "").localeCompare(b.branch ?? "");
          break;
        case "dirty":
          cmp = (a.dirtyCount ?? -1) - (b.dirtyCount ?? -1);
          break;
        case "ahead":
          cmp = (a.ahead ?? -1) - (b.ahead ?? -1);
          break;
        case "behind":
          cmp = (a.behind ?? -1) - (b.behind ?? -1);
          break;
      }
      return sortDirection === "asc" ? cmp : -cmp;
    });
    return sorted;
  }, [rows, sortColumn, sortDirection]);

  const handleSort = useCallback(
    (column: SortColumn) => {
      if (sortColumn === column) {
        setSortDirection((d) => (d === "asc" ? "desc" : "asc"));
      } else {
        setSortColumn(column);
        setSortDirection("asc");
      }
    },
    [sortColumn],
  );

  const handleFetchAll = useCallback(async () => {
    setFetchingAll(true);
    setBulkErrors(new Map());
    try {
      const response = await window.crab.workspace.fetchAll();
      const results = response.results as Array<{ relativePath: string; ok: boolean; error?: string }>;
      const errors = new Map<string, string>();
      for (const result of results) {
        if (!result.ok && result.error && result.error !== "disconnected") {
          errors.set(result.relativePath, result.error);
        }
      }
      if (errors.size > 0) {
        setBulkErrors(errors);
      }
    } catch (err) {
      setBulkErrors(
        new Map([["__global__", err instanceof Error ? err.message : String(err)]]),
      );
    } finally {
      setFetchingAll(false);
    }
  }, []);

  const handlePullAll = useCallback(async () => {
    setPullingAll(true);
    setBulkErrors(new Map());
    try {
      const response = await window.crab.workspace.pullAll();
      const results = response.results as Array<{ relativePath: string; ok: boolean; error?: string }>;
      const errors = new Map<string, string>();
      for (const result of results) {
        if (!result.ok && result.error && result.error !== "disconnected") {
          errors.set(result.relativePath, result.error);
        }
      }
      if (errors.size > 0) {
        setBulkErrors(errors);
      }
    } catch (err) {
      setBulkErrors(
        new Map([["__global__", err instanceof Error ? err.message : String(err)]]),
      );
    } finally {
      setPullingAll(false);
    }
  }, []);

  const handleRowClick = useCallback(
    (relativePath: string) => {
      selectRepo(relativePath);
    },
    [selectRepo],
  );

  // Summary stats
  const dirtyCount = rows.filter((r) => r.dirtyCount !== null && r.dirtyCount > 0).length;
  const behindCount = rows.filter((r) => r.behind !== null && r.behind > 0).length;
  const cleanCount = rows.filter(
    (r) => r.dirtyCount === 0 && (r.behind === null || r.behind === 0),
  ).length;
  const pullableCount = rows.filter(
    (r) => r.dirtyCount === 0 && r.behind !== null && r.behind > 0,
  ).length;

  return (
    <div className="h-full overflow-auto bg-background p-6">
      <div className="mx-auto max-w-5xl space-y-5">
        {/* Header */}
        <div>
          <div className="text-[10px] uppercase tracking-wider text-muted-foreground">
            Workspace
          </div>
          <h1 className="mt-1 text-xl font-semibold">{workspace.name}</h1>
          <div className="mt-0.5 flex items-center gap-3">
            <span className="truncate font-mono text-[11px] text-muted-foreground">
              {workspace.rootPath}
            </span>
            {hasRemoteRepos && (
              <div className="flex items-center gap-1">
                {remoteProfileIds.map((profileId) => {
                  const quality = qualityMap.get(profileId) ?? null;
                  const label = workspace.profileLabel ?? profileId;
                  return (
                    <ConnectionQualityBar
                      key={profileId}
                      quality={quality}
                      profileLabel={label}
                    />
                  );
                })}
              </div>
            )}
          </div>
        </div>

        {/* Summary stats */}
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <StatCard
            label="Repositories"
            value={String(rows.length)}
          />
          <StatCard
            label="Clean"
            value={String(cleanCount)}
            color="green"
          />
          <StatCard
            label="Dirty"
            value={String(dirtyCount)}
            color={dirtyCount > 0 ? "red" : undefined}
          />
          <StatCard
            label="Behind"
            value={String(behindCount)}
            color={behindCount > 0 ? "yellow" : undefined}
          />
        </div>

        {/* Actions */}
        <div className="flex items-center gap-3">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex items-center gap-1.5 rounded-md border border-border bg-card px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition disabled:opacity-50 disabled:pointer-events-none"
            onClick={handleFetchAll}
            disabled={fetchingAll || pullingAll}
          >
            {fetchingAll ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <RefreshCw size={13} />
            )}
            Fetch All
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex items-center gap-1.5 rounded-md border border-border bg-card px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition disabled:opacity-50 disabled:pointer-events-none"
            onClick={handlePullAll}
            disabled={pullingAll || fetchingAll || pullableCount === 0}
            title={
              pullableCount === 0
                ? "No repos eligible for pull (must be clean and behind upstream)"
                : `Pull ${pullableCount} repo${pullableCount === 1 ? "" : "s"}`
            }
          >
            {pullingAll ? (
              <Loader2 size={13} className="animate-spin" />
            ) : (
              <Download size={13} />
            )}
            Pull All
            {pullableCount > 0 && (
              <span className="rounded bg-primary/20 px-1.5 py-0.5 text-[10px] font-medium text-primary">
                {pullableCount}
              </span>
            )}
          </Button>
        </div>

        {/* Global error */}
        {bulkErrors.has("__global__") && (
          <div className="flex items-center gap-2 rounded border border-destructive/30 bg-destructive/5 px-3 py-2 text-[11px] text-destructive">
            <AlertCircle size={13} />
            <span className="min-w-0 flex-1">
              {bulkErrors.get("__global__")}
            </span>
          </div>
        )}

        {/* Table */}
        <div className="overflow-hidden rounded-lg border border-border bg-card">
          <table className="w-full text-xs" role="grid">
            <thead>
              <tr className="border-b border-border bg-background/50">
                <SortableHeader
                  label="Repository"
                  column="name"
                  active={sortColumn}
                  direction={sortDirection}
                  onSort={handleSort}
                  className="pl-3"
                />
                <SortableHeader
                  label="Branch"
                  column="branch"
                  active={sortColumn}
                  direction={sortDirection}
                  onSort={handleSort}
                />
                <SortableHeader
                  label="Dirty"
                  column="dirty"
                  active={sortColumn}
                  direction={sortDirection}
                  onSort={handleSort}
                  align="right"
                />
                <SortableHeader
                  label="Ahead"
                  column="ahead"
                  active={sortColumn}
                  direction={sortDirection}
                  onSort={handleSort}
                  align="right"
                />
                <SortableHeader
                  label="Behind"
                  column="behind"
                  active={sortColumn}
                  direction={sortDirection}
                  onSort={handleSort}
                  align="right"
                />
                <th className="w-8 px-2 py-2" />
              </tr>
            </thead>
            <tbody>
              {sortedRows.map((row) => (
                <DashboardTableRow
                  key={row.relativePath}
                  row={row}
                  bulkError={bulkErrors.get(row.relativePath) ?? null}
                  onClick={handleRowClick}
                />
              ))}
            </tbody>
          </table>
          {rows.length === 0 && (
            <div className="px-4 py-8 text-center text-xs text-muted-foreground">
              No repositories in this workspace.
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

// ── Sub-components ──────────────────────────────────────────────────

function StatCard({
  label,
  value,
  color,
}: {
  label: string;
  value: string;
  color?: "green" | "red" | "yellow";
}) {
  return (
    <div className="rounded-lg border border-border bg-card p-3">
      <div className="text-[10px] text-muted-foreground">{label}</div>
      <div
        className={clsx(
          "mt-1 font-mono text-lg font-semibold",
          color === "green" && "text-success",
          color === "red" && "text-destructive",
          color === "yellow" && "text-warning",
          !color && "text-foreground",
        )}
      >
        {value}
      </div>
    </div>
  );
}

function SortableHeader({
  label,
  column,
  active,
  direction,
  onSort,
  align,
  className,
}: {
  label: string;
  column: SortColumn;
  active: SortColumn;
  direction: SortDirection;
  onSort: (col: SortColumn) => void;
  align?: "left" | "right";
  className?: string;
}) {
  const isActive = active === column;
  return (
    <th
      className={clsx(
        "cursor-pointer select-none px-2 py-2 font-medium text-muted-foreground hover:text-foreground transition",
        align === "right" && "text-right",
        className,
      )}
      onClick={() => onSort(column)}
      aria-sort={isActive ? (direction === "asc" ? "ascending" : "descending") : "none"}
    >
      <span className="inline-flex items-center gap-1">
        {label}
        {isActive ? (
          direction === "asc" ? (
            <ArrowUp size={11} />
          ) : (
            <ArrowDown size={11} />
          )
        ) : (
          <ArrowUpDown size={11} className="opacity-30" />
        )}
      </span>
    </th>
  );
}

function DashboardTableRow({
  row,
  bulkError,
  onClick,
}: {
  row: DashboardRow;
  bulkError: string | null;
  onClick: (relativePath: string) => void;
}) {
  const status = getRowStatus(row);

  return (
    <>
      <tr
        className={clsx(
          "cursor-pointer border-b border-border/50 last:border-b-0 transition hover:bg-accent",
          row.level === 0 && "opacity-50",
        )}
        onClick={() => onClick(row.relativePath)}
        role="row"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onClick(row.relativePath);
          }
        }}
      >
        {/* Name */}
        <td className="px-3 py-2.5">
          <div className="flex items-center gap-2">
            <StatusDot status={status} />
            <span className="font-medium text-foreground">{row.name}</span>
            {row.level === 0 && (
              <span className="rounded bg-border/50 px-1 py-0.5 text-[9px] text-muted-foreground">
                L0
              </span>
            )}
            {row.isRemote && (
              <TransportBadge sshHost={row.sshHost} disconnected={row.error === "disconnected"} />
            )}
          </div>
        </td>

        {/* Branch */}
        <td className="px-2 py-2.5">
          {row.branch ? (
            <span className="inline-flex items-center gap-1 text-muted-foreground">
              <GitBranch size={11} />
              <span className="max-w-[120px] truncate">{row.branch}</span>
            </span>
          ) : (
            <span className="text-muted-foreground/50">—</span>
          )}
        </td>

        {/* Dirty */}
        <td className="px-2 py-2.5 text-right">
          {row.dirtyCount !== null ? (
            <span
              className={clsx(
                "font-mono",
                row.dirtyCount > 0 ? "text-destructive font-medium" : "text-muted-foreground",
              )}
            >
              {row.dirtyCount}
            </span>
          ) : (
            <span className="text-muted-foreground/50">—</span>
          )}
        </td>

        {/* Ahead */}
        <td className="px-2 py-2.5 text-right">
          {row.ahead !== null ? (
            <span
              className={clsx(
                "font-mono",
                row.ahead > 0 ? "text-primary font-medium" : "text-muted-foreground",
              )}
            >
              {row.ahead}
            </span>
          ) : (
            <span className="text-muted-foreground/50">—</span>
          )}
        </td>

        {/* Behind */}
        <td className="px-2 py-2.5 text-right">
          {row.behind !== null ? (
            <span
              className={clsx(
                "font-mono",
                row.behind > 0 ? "text-warning font-medium" : "text-muted-foreground",
              )}
            >
              {row.behind}
            </span>
          ) : (
            <span className="text-muted-foreground/50">—</span>
          )}
        </td>

        {/* Status icon */}
        <td className="px-2 py-2.5 text-center">
          {row.error ? (
            <span title={row.error}>
              <AlertCircle size={13} className="text-destructive" />
            </span>
          ) : status === "clean" ? (
            <CheckCircle2 size={13} className="text-success/60" />
          ) : null}
        </td>
      </tr>

      {/* Per-repo bulk operation error */}
      {bulkError && (
        <tr className="border-b border-border/50">
          <td colSpan={6} className="px-3 py-1.5">
            <div className="flex items-center gap-1.5 text-[10px] text-destructive">
              <AlertCircle size={11} />
              {bulkError}
            </div>
          </td>
        </tr>
      )}
    </>
  );
}

function StatusDot({ status }: { status: "clean" | "dirty" | "behind" | "unknown" }) {
  return (
    <span
      className={clsx(
        "inline-block h-2 w-2 shrink-0 rounded-full",
        status === "clean" && "bg-success",
        status === "dirty" && "bg-destructive",
        status === "behind" && "bg-warning",
        status === "unknown" && "bg-border",
      )}
      aria-label={status}
    />
  );
}

function TransportBadge({
  sshHost,
  disconnected,
}: {
  sshHost: string | null;
  disconnected: boolean;
}) {
  if (disconnected) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded bg-destructive/10 px-1.5 py-0.5 text-[9px] font-medium text-destructive"
        title="Remote connection lost"
      >
        <WifiOff size={10} />
        disconnected
      </span>
    );
  }

  return (
    <span
      className="inline-flex items-center gap-1 rounded bg-primary/10 px-1.5 py-0.5 text-[9px] text-muted-foreground"
      title={sshHost ? `Remote: ${sshHost}` : "Remote repository"}
    >
      <Cloud size={10} />
      {sshHost && <span className="max-w-[80px] truncate">{sshHost}</span>}
    </span>
  );
}

function getRowStatus(row: DashboardRow): "clean" | "dirty" | "behind" | "unknown" {
  if (row.dirtyCount === null && row.behind === null) return "unknown";
  if (row.dirtyCount !== null && row.dirtyCount > 0) return "dirty";
  if (row.behind !== null && row.behind > 0) return "behind";
  return "clean";
}
