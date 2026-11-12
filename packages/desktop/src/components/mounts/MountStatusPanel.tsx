import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
/**
 * Mount status panel showing detailed information about a specific mount.
 *
 * Displays:
 * - HEAD OID, ref, mode (read-only / read-write)
 * - Overlay dirty count, cache size, last refresh
 * - "Refresh" button to trigger immediate re-fetch
 * - "Unmount" button
 * - "Switch Branch" dropdown
 */

import {
  GitBranch,
  HardDrive,
  Loader2,
  RefreshCw,
  X,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";

/** Detailed mount status from the coordinator. */
interface MountStatusData {
  mountpoint: string;
  remote: string;
  ref: string;
  read_only: boolean;
  head_oid: string | null;
}

/** IPC result shape from the main process. */
interface MountIpcResult {
  ok: boolean;
  error?: string;
  errorCode?: string;
  data?: unknown;
}

interface MountStatusPanelProps {
  /** The mountpoint to show status for. */
  mountpoint: string;
  /** Callback to close the panel. */
  onClose: () => void;
  /** Callback after successful unmount. */
  onUnmounted?: () => void;
}

export function MountStatusPanel({ mountpoint, onClose, onUnmounted }: MountStatusPanelProps) {
  const [status, setStatus] = useState<MountStatusData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [unmounting, setUnmounting] = useState(false);
  const [switchRef, setSwitchRef] = useState("");
  const [switching, setSwitching] = useState(false);
  const [showSwitchInput, setShowSwitchInput] = useState(false);

  const fetchStatus = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = (await window.crab.mount.status(mountpoint)) as MountIpcResult;
      if (result.ok && result.data) {
        setStatus(result.data as MountStatusData);
      } else if (!result.ok) {
        setError(result.error ?? "Failed to get mount status");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [mountpoint]);

  useEffect(() => {
    fetchStatus();
  }, [fetchStatus]);

  async function handleRefresh() {
    setRefreshing(true);
    setError(null);
    try {
      const result = (await window.crab.mount.refresh(mountpoint)) as MountIpcResult;
      if (result.ok) {
        // Re-fetch status to show updated HEAD
        await fetchStatus();
      } else {
        setError(result.error ?? "Refresh failed");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setRefreshing(false);
    }
  }

  async function handleUnmount() {
    setUnmounting(true);
    setError(null);
    try {
      const result = (await window.crab.mount.unmount(mountpoint)) as MountIpcResult;
      if (result.ok) {
        onUnmounted?.();
        onClose();
      } else {
        setError(result.error ?? "Unmount failed");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setUnmounting(false);
    }
  }

  async function handleSwitchRef() {
    if (!switchRef.trim()) return;
    setSwitching(true);
    setError(null);
    try {
      const result = (await window.crab.mount.switch(mountpoint, switchRef.trim())) as MountIpcResult;
      if (result.ok) {
        setShowSwitchInput(false);
        setSwitchRef("");
        await fetchStatus();
      } else {
        setError(result.error ?? "Switch ref failed");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSwitching(false);
    }
  }

  const displayName = mountpoint.split("/").pop() || mountpoint;

  return (
    <div className="flex flex-col border-l border-border bg-card w-80">
      {/* Header */}
      <div className="flex items-center gap-2 border-b border-border px-4 py-3">
        <HardDrive size={14} className="text-primary shrink-0" />
        <h3 className="flex-1 truncate text-sm font-semibold text-foreground">
          {displayName}
        </h3>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground transition"
          onClick={onClose}
          aria-label="Close panel"
        >
          <X size={14} />
        </Button>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-auto px-4 py-3 space-y-4">
        {loading && (
          <div className="flex items-center justify-center py-8">
            <Loader2 size={16} className="animate-spin text-muted-foreground" />
          </div>
        )}

        {error && (
          <div className="rounded border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {error}
          </div>
        )}

        {status && !loading && (
          <>
            {/* Status fields */}
            <div className="space-y-2">
              <StatusField label="Mountpoint" value={status.mountpoint} />
              <StatusField label="Remote" value={status.remote} />
              <StatusField label="Ref" value={status.ref} />
              <StatusField label="Mode" value={status.read_only ? "Read-only" : "Read-write"} />
              {status.head_oid && (
                <StatusField label="HEAD" value={status.head_oid.slice(0, 12)} mono />
              )}
            </div>

            {/* Actions */}
            <div className="space-y-2 pt-2 border-t border-border">
              {/* Refresh */}
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="flex w-full items-center gap-2 rounded border border-border px-3 py-2 text-xs text-foreground hover:bg-accent transition disabled:opacity-50"
                onClick={handleRefresh}
                disabled={refreshing}
              >
                <RefreshCw size={12} className={refreshing ? "animate-spin" : ""} />
                <span>{refreshing ? "Refreshing…" : "Refresh"}</span>
              </Button>

              {/* Switch branch */}
              {!showSwitchInput ? (
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="flex w-full items-center gap-2 rounded border border-border px-3 py-2 text-xs text-foreground hover:bg-accent transition"
                  onClick={() => setShowSwitchInput(true)}
                >
                  <GitBranch size={12} />
                  <span>Switch Branch</span>
                </Button>
              ) : (
                <div className="flex items-center gap-1">
                  <Input
                    type="text"
                    className="flex-1 rounded border border-border bg-background px-2 py-1.5 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
                    placeholder="branch name"
                    value={switchRef}
                    onChange={(e) => setSwitchRef(e.target.value)}
                    onKeyDown={(e) => { if (e.key === "Enter") handleSwitchRef(); if (e.key === "Escape") setShowSwitchInput(false); }}
                    disabled={switching}
                    autoFocus
                  />
                  <Button
                    variant="default"
                    size="compact"
                    type="button"
                    className="rounded bg-primary px-2 py-1.5 text-xs text-primary-foreground hover:bg-primary/90 transition disabled:opacity-50"
                    onClick={handleSwitchRef}
                    disabled={switching || !switchRef.trim()}
                  >
                    {switching ? (
                      <Loader2 size={10} className="animate-spin" />
                    ) : (
                      "Go"
                    )}
                  </Button>
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    className="rounded p-1.5 text-muted-foreground hover:text-foreground hover:bg-accent transition"
                    onClick={() => { setShowSwitchInput(false); setSwitchRef(""); }}
                    aria-label="Cancel"
                  >
                    <X size={10} />
                  </Button>
                </div>
              )}

              {/* Unmount */}
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="flex w-full items-center gap-2 rounded border border-destructive/40 px-3 py-2 text-xs text-destructive hover:bg-destructive/10 transition disabled:opacity-50"
                onClick={handleUnmount}
                disabled={unmounting}
              >
                <X size={12} />
                <span>{unmounting ? "Unmounting…" : "Unmount"}</span>
              </Button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function StatusField({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div>
      <dt className="text-[10px] font-medium text-muted-foreground uppercase tracking-wider">
        {label}
      </dt>
      <dd className={`mt-0.5 text-xs text-foreground truncate ${mono ? "font-mono" : ""}`}>
        {value}
      </dd>
    </div>
  );
}
