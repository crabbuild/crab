import { Activity, Cloud, HardDrive, Loader2, RefreshCw, Wifi, WifiOff } from "lucide-react";
import { HydrationSummary, rpc } from "../ipc";
import { formatBytes } from "../lib/format";
import { useActiveRepo, useStore } from "../state";
import { SidecarChips } from "./SidecarChips";
import { TaskListPopover } from "./TaskListPopover";
import { RemoteManagerPopover } from "./RemoteManagerPopover";
import { StatusBarToolbar } from "./StatusBarToolbar";
import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";

interface Props {
  summary: HydrationSummary | null;
  loading: boolean;
  onRefresh: () => void;
  /** Navigate to the storage config section in Settings. */
  onOpenStorageConfig?: () => void;
  /** Navigate to a file at a specific line (for task anchors). */
  onNavigateFile?: (path: string, line: number) => void;
  /** Navigate to a commit detail (for task anchors). */
  onNavigateCommit?: (sha: string) => void;
  /** Git sync toolbar props — when provided, renders the StatusBarToolbar. */
  syncToolbar?: {
    repoId: string;
    currentBranch: string;
    ahead: number;
    behind: number;
    upstream: string | null;
  } | null;
  /** Called after a toolbar command completes (push/pull/fetch). */
  onCommandExecuted?: (commandId: string) => void;
  /** Whether the activity drawer is currently open. */
  activityDrawerOpen?: boolean;
  /** Toggle the activity drawer. */
  onToggleActivityDrawer?: () => void;
}

/**
 * Thin strip along the bottom of the main pane — VS Code-style.
 * Shows at-a-glance hydration progress (how many pointer files are
 * materialised, how much disk they take, how much would be used if
 * everything were hydrated) and a live count of running background
 * jobs.
 *
 * Kept to a single row with icon-first info so non-technical users
 * can see the shape of their checkout without squinting. For very
 * large repos the `total_tracked` can be misleading if the user only
 * wants a subset hydrated — that's addressed by the per-subtree
 * Hydrate action and the dataset preset we ship in Pass B.
 */
export function StatusBar({ summary, loading, onRefresh, onOpenStorageConfig, onNavigateFile, onNavigateCommit, syncToolbar, onCommandExecuted, activityDrawerOpen, onToggleActivityDrawer }: Props) {
  const { state } = useStore();
  const running = Object.values(state.jobs).filter((j) => j.status === "running").length;

  return (
    <div className="flex h-7 shrink-0 items-center justify-between border-t border-gx-border bg-gx-surface px-3 text-[11px] text-gx-muted">
      <div className="flex items-center gap-4">
        {summary ? (
          <>
            <HydrationProgress summary={summary} />
            <span className="flex items-center gap-1">
              <HardDrive size={11} /> {formatBytes(Number(summary.on_disk_bytes))} on disk
            </span>
            <span className="flex items-center gap-1">
              <Cloud size={11} /> {formatBytes(Number(summary.logical_bytes))} logical
            </span>
          </>
        ) : loading ? (
          <span className="flex items-center gap-1">
            <Loader2 size={11} className="animate-spin" /> Checking hydration…
          </span>
        ) : (
          <span>No hydration data</span>
        )}
      </div>
      <div className="flex items-center gap-3">
        {syncToolbar && (
          <StatusBarToolbar
            repoId={syncToolbar.repoId}
            currentBranch={syncToolbar.currentBranch}
            ahead={syncToolbar.ahead}
            behind={syncToolbar.behind}
            upstream={syncToolbar.upstream}
            onCommandExecuted={onCommandExecuted}
          />
        )}
        <TaskListPopover
          onNavigateFile={onNavigateFile}
          onNavigateCommit={onNavigateCommit}
        />
        <RemoteStatusChip onClick={onOpenStorageConfig} />
        <SidecarChips />
        {onToggleActivityDrawer && (
          <button
            type="button"
            onClick={onToggleActivityDrawer}
            className={clsx(
              "flex items-center gap-1 rounded px-1.5 py-0.5 transition hover:bg-gx-surface-hover",
              activityDrawerOpen && "bg-gx-accent/15 text-gx-accent",
              running > 0 && !activityDrawerOpen && "text-gx-warn",
            )}
            title="Toggle Activity (background jobs)"
          >
            <Activity size={11} />
            {running > 0 && (
              <span className="ml-0.5 flex items-center gap-0.5 rounded bg-gx-warn/20 px-1 text-[9px] text-gx-warn">
                <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-gx-warn" />
                {running}
              </span>
            )}
          </button>
        )}
        <button
          type="button"
          onClick={onRefresh}
          className="flex items-center gap-1 hover:text-gx-text"
          title="Refresh hydration status"
        >
          <RefreshCw size={11} />
        </button>
      </div>
    </div>
  );
}

function HydrationProgress({ summary }: { summary: HydrationSummary }) {
  const total = summary.total_tracked;
  if (total === 0) {
    return <span>No pointer-tracked files</span>;
  }
  const hydratedPct = (summary.hydrated / total) * 100;
  return (
    <span className="flex items-center gap-2">
      <span className="relative h-2 w-24 overflow-hidden rounded-full bg-gx-bg-sub">
        <span
          className="absolute inset-y-0 left-0 bg-gx-accent"
          style={{ width: `${hydratedPct}%` }}
        />
      </span>
      <span className="tabular-nums">
        <strong className="text-gx-text">{summary.hydrated.toLocaleString()}</strong>
        <span className="text-gx-muted"> / {total.toLocaleString()} hydrated</span>
      </span>
      {summary.partial > 0 && (
        <span className="text-gx-warn">· {summary.partial} partial</span>
      )}
      {summary.missing > 0 && (
        <span className="text-gx-error">· {summary.missing} missing</span>
      )}
    </span>
  );
}

type RemoteStatus = "unchecked" | "verifying" | "connected" | "failed";

/**
 * Remote connectivity indicator chip. Shows a coloured dot:
 * - Green: verified connection to the backend store.
 * - Yellow: credentials configured but not yet verified.
 * - Red: verification failed.
 * Clicking opens the Remote Manager popover for managing remotes.
 * When the connection is red/failed, the popover lets users fix
 * configuration or add new remotes.
 */
function RemoteStatusChip({ onClick: _onClick }: { onClick?: () => void }) {
  const activeRepo = useActiveRepo();
  const [status, setStatus] = useState<RemoteStatus>("unchecked");
  const [latency, setLatency] = useState<number | null>(null);
  const [popoverOpen, setPopoverOpen] = useState(false);

  const generation = useRef(0);

  const verify = useCallback(async () => {
    if (!activeRepo) return;
    const request = ++generation.current;
    setStatus("verifying");
    try {
      const result = await rpc.storageConfigVerify(activeRepo.repo_id);
      if (request !== generation.current) return;
      if (result.ok) {
        setStatus("connected");
        setLatency(result.latency_ms);
      } else {
        setStatus("failed");
      }
    } catch {
      if (request === generation.current) setStatus("failed");
    }
  }, [activeRepo]);

  useEffect(() => {
    generation.current++;
    setStatus("unchecked"); setLatency(null);
    if (!activeRepo) return;
    let cancelled = false;
    const timer = setTimeout(() => {
      rpc.storageConfigGet(activeRepo.repo_id).then(config => { if (!cancelled && config.configured) void verify(); }).catch(() => undefined);
    }, 1500);
    const changed = (event: Event) => {
      const target = (event as CustomEvent<{ repo_id?: string }>).detail?.repo_id;
      if (!target || target === activeRepo.repo_id) { setLatency(null); void verify(); }
    };
    window.addEventListener("crab:storage-connection-changed", changed);
    return () => { cancelled = true; generation.current++; clearTimeout(timer); window.removeEventListener("crab:storage-connection-changed", changed); };
  }, [activeRepo, verify]);

  if (!activeRepo) return null;

  const dotColor =
    status === "connected"
      ? "bg-gx-success"
      : status === "failed"
        ? "bg-gx-error"
        : status === "verifying"
          ? "bg-gx-warn animate-pulse"
          : "bg-gx-muted/50";

  const label =
    status === "connected"
      ? `Remote connected${latency ? ` (${latency}ms)` : ""}`
      : status === "failed"
        ? "Remote connection failed — click to manage remotes"
        : status === "verifying"
          ? "Verifying remote..."
          : "Remote unchecked — click to manage remotes";

  return (
    <div className="relative">
      <button
        type="button"
        className="flex items-center gap-1 hover:text-gx-text"
        onClick={() => setPopoverOpen(!popoverOpen)}
        title={label}
      >
        <span className={clsx("inline-block h-2 w-2 rounded-full", dotColor)} />
        {status === "connected" ? (
          <Wifi size={11} className="text-gx-success" />
        ) : status === "failed" ? (
          <WifiOff size={11} className="text-gx-error" />
        ) : (
          <Cloud size={11} />
        )}
      </button>
      <RemoteManagerPopover
        open={popoverOpen}
        onClose={() => setPopoverOpen(false)}
        onPushTo={(remoteName) => {
          setPopoverOpen(false);
          // Navigate to push flow with the selected remote.
          // For now, dispatch a custom event that the parent can handle.
          window.dispatchEvent(
            new CustomEvent("crab:push-to-remote", { detail: { remote: remoteName } }),
          );
        }}
      />
    </div>
  );
}
