import { Button } from "@/components/ui/button";
/**
 * Sidebar section showing active VFS mounts with status indicators.
 *
 * Lists all mounts managed by the coordinator with:
 * - Green dot: running (healthy)
 * - Yellow dot: stale (coordinator not responding)
 * - Mount name, mountpoint path, and ref
 * - Click to view status details
 * - One-click unmount button
 */

import {
  ChevronDown,
  ChevronRight,
  HardDrive,
  RefreshCw,
  X,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";

/** Mount info as returned by the coordinator list operation. */
interface MountEntry {
  mountpoint: string;
  remote: string;
  ref: string;
  read_only: boolean;
}

/** IPC result shape from the main process. */
interface MountIpcResult {
  ok: boolean;
  error?: string;
  errorCode?: string;
  data?: unknown;
}

interface MountsSidebarProps {
  /** Callback when a mount is selected for detail view. */
  onSelectMount?: (mountpoint: string) => void;
  /** Currently selected mountpoint (for highlighting). */
  selectedMountpoint?: string | null;
}

const LS_MOUNTS_COLLAPSED = "crab.section.mounts.collapsed";

export function MountsSidebar({ onSelectMount, selectedMountpoint }: MountsSidebarProps) {
  const [mounts, setMounts] = useState<MountEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState(() => {
    try { return localStorage.getItem(LS_MOUNTS_COLLAPSED) === "1"; } catch { return false; }
  });

  useEffect(() => {
    try { localStorage.setItem(LS_MOUNTS_COLLAPSED, collapsed ? "1" : "0"); } catch { /* best-effort */ }
  }, [collapsed]);

  const fetchMounts = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = (await window.crab.mount.list()) as MountIpcResult;
      if (result.ok && Array.isArray(result.data)) {
        setMounts(result.data as MountEntry[]);
      } else if (!result.ok) {
        // Connection refused means coordinator isn't running — show empty
        if (result.errorCode === "CONNECTION_REFUSED") {
          setMounts([]);
        } else {
          setError(result.error ?? "Failed to list mounts");
        }
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  // Fetch mounts on mount and periodically
  useEffect(() => {
    fetchMounts();
    const interval = setInterval(fetchMounts, 10_000);
    return () => clearInterval(interval);
  }, [fetchMounts]);

  async function handleUnmount(mountpoint: string) {
    try {
      const result = (await window.crab.mount.unmount(mountpoint)) as MountIpcResult;
      if (result.ok) {
        setMounts((prev) => prev.filter((m) => m.mountpoint !== mountpoint));
      } else {
        setError(result.error ?? "Unmount failed");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  // Don't render the section if there are no mounts and no error
  if (mounts.length === 0 && !error && !loading) return null;

  return (
    <div>
      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="flex w-full items-center gap-1 px-3 pt-2 pb-1 text-[11px] uppercase tracking-wider text-muted-foreground hover:text-foreground transition"
        onClick={() => setCollapsed((v) => !v)}
        aria-expanded={!collapsed}
        aria-label="Toggle Mounts section"
      >
        {collapsed ? <ChevronRight size={11} /> : <ChevronDown size={11} />}
        <HardDrive size={11} className="mr-0.5" />
        <span>Mounts</span>
        {mounts.length > 0 && (
          <span className="ml-auto text-[10px] text-muted-foreground">
            {mounts.length}
          </span>
        )}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="ml-1 rounded p-0.5 text-muted-foreground hover:text-foreground hover:bg-accent transition"
          onClick={(e) => { e.stopPropagation(); fetchMounts(); }}
          title="Refresh mount list"
          aria-label="Refresh mounts"
        >
          <RefreshCw size={10} className={loading ? "animate-spin" : ""} />
        </Button>
      </Button>

      {!collapsed && (
        <div className="mt-1 pl-3 pr-2">
          {error && (
            <div className="mb-1 rounded border border-destructive/40 bg-destructive/10 px-2 py-1 text-[10px] text-destructive">
              {error}
            </div>
          )}

          {mounts.length === 0 && !error && (
            <div className="px-2 py-2 text-[11px] text-muted-foreground">
              No active mounts
            </div>
          )}

          <ul className="space-y-0.5">
            {mounts.map((mount) => (
              <MountEntryRow
                key={mount.mountpoint}
                mount={mount}
                isSelected={selectedMountpoint === mount.mountpoint}
                onSelect={() => onSelectMount?.(mount.mountpoint)}
                onUnmount={() => handleUnmount(mount.mountpoint)}
              />
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Mount entry row
// ---------------------------------------------------------------------------

interface MountEntryRowProps {
  mount: MountEntry;
  isSelected: boolean;
  onSelect: () => void;
  onUnmount: () => void;
}

function MountEntryRow({ mount, isSelected, onSelect, onUnmount }: MountEntryRowProps) {
  const [hovered, setHovered] = useState(false);

  // Derive a short display name from the mountpoint
  const displayName = mount.mountpoint.split("/").pop() || mount.mountpoint;

  return (
    <li
      className={`group flex items-center gap-1.5 rounded px-2 py-1 cursor-pointer transition ${
        isSelected
          ? "bg-primary/15 text-foreground"
          : "text-foreground hover:bg-accent"
      }`}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      onClick={onSelect}
      role="button"
      tabIndex={0}
      onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") onSelect(); }}
      aria-label={`Mount: ${displayName}`}
    >
      {/* Status indicator */}
      <span
        className="inline-block h-2 w-2 shrink-0 rounded-full bg-success"
        title="Running"
        aria-label="Running"
      />

      {/* Mount info */}
      <div className="min-w-0 flex-1">
        <div className="truncate text-xs font-medium">{displayName}</div>
        <div className="truncate text-[10px] text-muted-foreground">
          {mount.ref !== "HEAD" ? mount.ref : ""}
          {mount.ref !== "HEAD" && mount.read_only ? " · " : ""}
          {mount.read_only ? "read-only" : ""}
        </div>
      </div>

      {/* Unmount button (visible on hover) */}
      {hovered && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="shrink-0 rounded p-0.5 text-muted-foreground hover:text-destructive hover:bg-destructive/10 transition"
          onClick={(e) => { e.stopPropagation(); onUnmount(); }}
          title="Unmount"
          aria-label={`Unmount ${displayName}`}
        >
          <X size={12} />
        </Button>
      )}
    </li>
  );
}
