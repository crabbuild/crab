import { Button } from "@/components/ui/button";
import { NativeSelect } from "@/components/ui/native-select";
import { Input } from "@/components/ui/input";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  Monitor,
  Plus,
  FileDown,
  Search,
  Filter,
} from "lucide-react";
import { useStore, SshConnectionState } from "../state";
import { ConnectionCard } from "../components/ssh/ConnectionCard";
import { ConnectionForm, SshProfileFormData } from "../components/ssh/ConnectionForm";
import { SshConfigBanner } from "../components/ssh/SshConfigBanner";
import { SshConfigImport, ParsedSshHost } from "../components/ssh/SshConfigImport";
import { RemoteFileBrowser } from "../components/ssh/RemoteFileBrowser";
import { PortForwardPanel } from "../components/ssh/PortForwardPanel";
import { SyncPairDialog, SyncPairConfig } from "../components/ssh/SyncPairDialog";
import { ConflictResolutionDialog } from "../components/ssh/ConflictResolutionDialog";
import { SyncTransferProgress } from "../components/ssh/SyncTransferProgress";
import { DeployProgressIndicator } from "../components/ssh/DeployProgressIndicator";
import { SyncManagementPanel } from "../components/ssh/SyncManagementPanel";
import { publishToast } from "../lib/toasts";
import { useSyncConflictNotifications } from "../hooks/useSyncConflictNotifications";

// Minimal SshProfile type for the renderer (mirrors electron/ssh-manager.ts)
// Extracted to components/ssh/ssh-profile-types.ts to break circular deps.
export type { SshProfile } from "../components/ssh/ssh-profile-types";
import type { SshProfile } from "../components/ssh/ssh-profile-types";

/**
 * SSH Connections manager view. Lists all saved profiles with live
 * status indicators, health summaries, and action buttons.
 *
 * Features:
 * - Connected/disconnected/error state cards
 * - Drag-and-drop reordering
 * - Profile pinning
 * - Tag filtering
 * - Empty state with CTA
 * - Responsive layout
 * - Full accessibility (ARIA, keyboard nav, screen reader)
 */
export function SshView() {
  const { state, dispatch } = useStore();
  const [openError, setOpenError] = useState<string | null>(null);
  const [profiles, setProfiles] = useState<SshProfile[]>([]);
  const [formOpen, setFormOpen] = useState(false);
  const [editingProfile, setEditingProfile] = useState<SshProfile | null>(null);
  const [importDialogOpen, setImportDialogOpen] = useState(false);
  const [filterTag, setFilterTag] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [draggedId, setDraggedId] = useState<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [focusedIndex, setFocusedIndex] = useState(-1);
  const [subView, setSubView] = useState<{ type: "files" | "ports" | "sync"; profileId: string } | null>(null);
  const [syncDialogState, setSyncDialogState] = useState<{ profileId: string; remotePath: string } | null>(null);
  const [fileBrowserKey, setFileBrowserKey] = useState(0);

  // Sync conflict notifications — shows toast on new conflicts, manages dialog state
  const { activeConflict, dismissConflict, resolveConflict } = useSyncConflictNotifications();

  // Load profiles from IPC
  useEffect(() => {
    loadProfiles();
  }, []);

  // Subscribe to SSH connection status changes from the main process
  useEffect(() => {
    const ssh = (window as any).crab?.ssh;
    if (!ssh?.onStatusChanged) return;
    const unsubscribe = ssh.onStatusChanged((evt: { profileId: string; status: string; error?: string }) => {
      dispatch({ type: "ssh-status-changed", profileId: evt.profileId, status: evt.status as SshConnectionState["status"], error: evt.error });
    });
    return unsubscribe;
  }, [dispatch]);

  // Listen for file browser and port forward open events from ConnectionCard
  useEffect(() => {
    const handleFileBrowser = (e: Event) => {
      const detail = (e as CustomEvent).detail;
      setSubView({ type: "files", profileId: detail.profileId });
    };
    const handlePortForwards = (e: Event) => {
      const detail = (e as CustomEvent).detail;
      setSubView({ type: "ports", profileId: detail.profileId });
    };
    window.addEventListener("crab:open-ssh-file-browser", handleFileBrowser);
    window.addEventListener("crab:open-ssh-port-forwards", handlePortForwards);
    return () => {
      window.removeEventListener("crab:open-ssh-file-browser", handleFileBrowser);
      window.removeEventListener("crab:open-ssh-port-forwards", handlePortForwards);
    };
  }, []);

  async function loadProfiles() {
    try {
      const result = await (window as any).crab.ssh.listProfiles();
      setProfiles(result ?? []);
    } catch {
      // IPC not available yet or SSH not initialized
      setProfiles([]);
    }
  }

  // Get connection state for a profile
  function getConnectionState(profileId: string): SshConnectionState | undefined {
    return state.sshConnections.get(profileId);
  }

  // Collect all unique tags
  const allTags = useMemo(() => {
    const tags = new Set<string>();
    for (const p of profiles) {
      for (const t of p.tags) tags.add(t);
    }
    return Array.from(tags).sort();
  }, [profiles]);

  // Filter and sort profiles
  const displayProfiles = useMemo(() => {
    let filtered = profiles;

    // Tag filter
    if (filterTag) {
      filtered = filtered.filter((p) => p.tags.includes(filterTag));
    }

    // Search filter
    if (searchQuery.trim()) {
      const q = searchQuery.toLowerCase();
      filtered = filtered.filter(
        (p) =>
          p.label.toLowerCase().includes(q) ||
          p.host.toLowerCase().includes(q) ||
          p.username.toLowerCase().includes(q) ||
          p.tags.some((t) => t.toLowerCase().includes(q)),
      );
    }

    // Sort: pinned first, then by last connected (most recent first)
    return [...filtered].sort((a, b) => {
      if (a.pinned && !b.pinned) return -1;
      if (!a.pinned && b.pinned) return 1;
      const aTime = a.lastConnectedAt ?? a.createdAt;
      const bTime = b.lastConnectedAt ?? b.createdAt;
      return bTime - aTime;
    });
  }, [profiles, filterTag, searchQuery]);

  // Actions
  async function handleConnect(profileId: string) {
    try {
      await (window as any).crab.ssh.connect(profileId);
    } catch {
      // Error handled via IPC events
    }
  }

  async function handleOpenRepository(profileId: string, remotePath: string) {
    setOpenError(null);
    try {
      if (!window.crab) throw new Error("SSH requires the desktop application");
      const handle = await window.crab.ssh.openAsRepo(profileId, remotePath);
      window.dispatchEvent(new CustomEvent("crab:repo-opened", { detail: { handle, path: handle.repo_id } }));
    } catch (error) { setOpenError(error instanceof Error ? error.message : String(error)); }
  }

  async function handleDisconnect(profileId: string) {
    try {
      await (window as any).crab.ssh.disconnect(profileId);
    } catch {
      // Error handled via IPC events
    }
  }

  async function handleDelete(profileId: string) {
    try {
      await (window as any).crab.ssh.deleteProfile(profileId);
      setProfiles((prev) => prev.filter((p) => p.id !== profileId));
      dispatch({ type: "ssh-connection-remove", profileId });
    } catch {
      // Ignore
    }
  }

  function handleEdit(profile: SshProfile) {
    setEditingProfile(profile);
    setFormOpen(true);
  }

  async function handleSave(data: SshProfileFormData) {
    try {
      await (window as any).crab.ssh.saveProfile(data);
      setFormOpen(false);
      setEditingProfile(null);
      await loadProfiles();
    } catch {
      // Error shown in form
    }
  }

  async function handleTogglePin(profileId: string) {
    const profile = profiles.find((p) => p.id === profileId);
    if (!profile) return;
    const updated = { ...profile, pinned: !profile.pinned };
    try {
      await (window as any).crab.ssh.saveProfile(updated);
      setProfiles((prev) => prev.map((p) => (p.id === profileId ? updated : p)));
    } catch {
      // Ignore
    }
  }

  async function handleOpenTerminal(profileId: string) {
    const profile = profiles.find((p) => p.id === profileId);
    window.dispatchEvent(new CustomEvent("crab:open-ssh-terminal-here", {
      detail: { profileId, hostname: profile?.host ?? "remote", cwd: profile?.remotePath, colour: profile?.colour },
    }));
  }

  async function handleImportConfig() {
    setImportDialogOpen(true);
  }

  async function handleImportHosts(hosts: ParsedSshHost[]) {
    try {
      for (const host of hosts) {
        await (window as any).crab.ssh.saveProfile({
          label: host.alias,
          host: host.hostname,
          port: host.port,
          username: host.user || "root",
          authMethod: host.identityFile
            ? { type: "key", keyPath: host.identityFile }
            : { type: "agent" },
          jumpHost: host.proxyJump,
          forwardAgent: host.forwardAgent,
          keepAliveInterval: host.serverAliveInterval ?? 15,
          tags: [],
        });
      }
      await loadProfiles();
    } catch {
      // Errors handled by the import dialog
    }
  }

  function handleSetupSync(profileId: string, remotePath: string) {
    setSyncDialogState({ profileId, remotePath });
  }

  async function handleSyncConfirm(config: SyncPairConfig) {
    const profileId = syncDialogState?.profileId;
    setSyncDialogState(null);
    if (!profileId) return;

    try {
      const { syncPairId } = await (window as any).crab.ssh.syncAddPair({
        profileId,
        localDir: config.localDir,
        remoteDir: config.remoteDir,
        ignorePatterns: config.ignorePatterns,
      });
      await (window as any).crab.ssh.syncStart(syncPairId);
      publishToast({
        kind: "success",
        title: "Sync pair created",
        body: `Syncing ${config.remoteDir} ↔ ${config.localDir}`,
      });
      setFileBrowserKey((k) => k + 1);
    } catch (err) {
      publishToast({
        kind: "error",
        title: "Failed to create sync pair",
        body: err instanceof Error
          ? err.message
          : (err as any)?.message ?? String(err),
      });
    }
  }

  // Drag and drop reordering
  function handleDragStart(profileId: string) {
    setDraggedId(profileId);
  }

  function handleDragOver(e: React.DragEvent, profileId: string) {
    e.preventDefault();
    setDragOverId(profileId);
  }

  function handleDrop(targetId: string) {
    if (!draggedId || draggedId === targetId) {
      setDraggedId(null);
      setDragOverId(null);
      return;
    }
    setProfiles((prev) => {
      const items = [...prev];
      const fromIdx = items.findIndex((p) => p.id === draggedId);
      const toIdx = items.findIndex((p) => p.id === targetId);
      if (fromIdx === -1 || toIdx === -1) return prev;
      const [moved] = items.splice(fromIdx, 1);
      items.splice(toIdx, 0, moved);
      return items;
    });
    setDraggedId(null);
    setDragOverId(null);
  }

  // Keyboard navigation
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (displayProfiles.length === 0) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setFocusedIndex((i) => Math.min(i + 1, displayProfiles.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setFocusedIndex((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter" && focusedIndex >= 0) {
        const profile = displayProfiles[focusedIndex];
        const connState = getConnectionState(profile.id);
        if (connState?.status === "connected") {
          handleOpenTerminal(profile.id);
        } else {
          handleConnect(profile.id);
        }
      }
    },
    [displayProfiles, focusedIndex],
  );

  // Empty state
  if (profiles.length === 0) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-6 p-8">
        <SshConfigBanner onImport={handleImportConfig} />
        <div className="flex flex-col items-center gap-3 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-primary/10">
            <Monitor size={32} className="text-primary" aria-hidden="true" />
          </div>
          <h2 className="text-lg font-semibold text-foreground">SSH Remote Connections</h2>
          <p className="max-w-sm text-sm text-muted-foreground">
            Connect to remote servers over SSH to manage repositories, run terminals, and deploy
            agents — all from within Crab Desktop.
          </p>
        </div>
        <div className="flex flex-col items-center gap-2">
          <Button variant="default"
            type="button"
            className="flex items-center gap-2 px-4 py-2 text-sm"
            onClick={() => { setEditingProfile(null); setFormOpen(true); }}
            aria-label="Add your first SSH connection"
          >
            <Plus size={14} />
            Add your first connection
          </Button>
          <Button variant="outline"
            type="button"
            className="flex items-center gap-2 px-3 py-1.5 text-xs text-muted-foreground hover:text-foreground"
            onClick={handleImportConfig}
            aria-label="Import connections from SSH config"
          >
            <FileDown size={12} />
            Import from SSH Config
          </Button>
        </div>
        <a
          href="https://docs.crab.dev/desktop/ssh-remote"
          target="_blank"
          rel="noopener noreferrer"
          className="text-xs text-primary hover:underline"
        >
          Learn more about SSH remote connections →
        </a>

        {/* Import dialog */}
        <SshConfigImport
          isOpen={importDialogOpen}
          onClose={() => setImportDialogOpen(false)}
          onImport={handleImportHosts}
          existingProfiles={profiles.map((p) => ({ host: p.host, port: p.port, username: p.username }))}
        />

        {/* Connection form dialog */}
        {formOpen && (
          <ConnectionForm
            profile={editingProfile}
            onSave={handleSave}
            onCancel={() => { setFormOpen(false); setEditingProfile(null); }}
          />
        )}
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Sub-view: File Browser or Port Forwards */}
      {subView && (() => {
        const profile = profiles.find((p) => p.id === subView.profileId);
        if (!profile) { setSubView(null); return null; }

        return (
          <div className="flex h-full flex-col overflow-hidden">
            <header className="flex shrink-0 items-center gap-2 border-b border-border px-3 py-2">
              <button
                type="button"
                className="text-xs text-primary hover:underline"
                onClick={() => setSubView(null)}
              >
                ← Back to connections
              </button>
              <span className="text-xs text-muted-foreground">
                {profile.label}
              </span>
              <div className="flex-1" />
              {/* Sub-view tabs */}
              <div className="flex items-center gap-1">
                <button
                  type="button"
                  className={clsx(
                    "px-2 py-1 text-[11px] rounded transition",
                    subView.type === "files"
                      ? "bg-primary/15 text-primary font-medium"
                      : "text-muted-foreground hover:text-foreground hover:bg-accent",
                  )}
                  onClick={() => setSubView({ ...subView, type: "files" })}
                >
                  Files
                </button>
                <button
                  type="button"
                  className={clsx(
                    "px-2 py-1 text-[11px] rounded transition",
                    subView.type === "sync"
                      ? "bg-primary/15 text-primary font-medium"
                      : "text-muted-foreground hover:text-foreground hover:bg-accent",
                  )}
                  onClick={() => setSubView({ ...subView, type: "sync" })}
                >
                  Sync
                </button>
                <button
                  type="button"
                  className={clsx(
                    "px-2 py-1 text-[11px] rounded transition",
                    subView.type === "ports"
                      ? "bg-primary/15 text-primary font-medium"
                      : "text-muted-foreground hover:text-foreground hover:bg-accent",
                  )}
                  onClick={() => setSubView({ ...subView, type: "ports" })}
                >
                  Ports
                </button>
              </div>
            </header>
            {openError && <p role="alert" className="px-3 py-2 text-sm text-destructive">{openError}</p>}
            {/* Deploy progress + sync transfer progress */}
            <div className="shrink-0 px-3">
              <DeployProgressIndicator profileId={profile.id} />
              <SyncTransferProgress />
            </div>
            <div className="flex-1 overflow-auto">
              {subView.type === "files" ? (
                <RemoteFileBrowser
                  key={fileBrowserKey}
                  profileId={profile.id}
                  profileLabel={profile.label}
                  profileColour={profile.colour}
                  initialPath={profile.remotePath || "/"}
                  onOpenRepo={handleOpenRepository}
                  onOpenTerminal={(pid, path) => {
                    window.dispatchEvent(new CustomEvent("crab:open-ssh-terminal-here", {
                      detail: { profileId: pid, hostname: profile.host, cwd: path, colour: profile.colour },
                    }));
                  }}
                  onSetupSync={(pid, remotePath) => handleSetupSync(pid, remotePath)}
                />
              ) : subView.type === "sync" ? (
                <SyncManagementPanel profileId={profile.id} />
              ) : (
                <PortForwardPanel
                  profileId={profile.id}
                  forwards={(getConnectionState(profile.id)?.portForwards ?? []).map((f) => ({
                    id: f.id,
                    profileId: profile.id,
                    localPort: f.localPort,
                    actualLocalPort: f.actualLocalPort ?? f.localPort,
                    remoteHost: f.remoteHost ?? "localhost",
                    remotePort: f.remotePort,
                    label: f.label ?? "",
                    autoStart: f.autoStart ?? false,
                    status: (f.status ?? "stopped") as "active" | "stopped" | "error",
                    error: f.error,
                  }))}
                  onAdd={async (localPort, remoteHost, remotePort, label) => {
                    await (window as any).crab.ssh.addForward(profile.id, localPort, remoteHost, remotePort, label);
                  }}
                  onRemove={async (forwardId) => {
                    await (window as any).crab.ssh.removeForward(forwardId);
                  }}
                  onOpenBrowser={async (forwardId) => {
                    await (window as any).crab.ssh.openForwardBrowser(forwardId);
                  }}
                />
              )}
            </div>
          </div>
        );
      })()}

      {/* Main connection list (hidden when sub-view is active) */}
      {!subView && <>
      {/* Header */}
      <header className="flex shrink-0 items-center gap-2 border-b border-border px-4 py-3">
        <Monitor size={16} className="text-muted-foreground" aria-hidden="true" />
        <h1 className="text-sm font-semibold text-foreground">SSH Connections</h1>
        <span className="ml-1 rounded-full bg-accent px-2 py-0.5 text-[10px] text-muted-foreground" aria-label={`${displayProfiles.length} connections`}>
          {displayProfiles.length}
        </span>
        <div className="flex-1" />

        {/* Tag filter */}
        {allTags.length > 0 && (
          <div className="relative">
            <NativeSelect
              className="appearance-none rounded border border-border bg-card px-2 py-1 pr-6 text-xs text-foreground"
              value={filterTag ?? ""}
              onChange={(e) => setFilterTag(e.target.value || null)}
              aria-label="Filter by tag"
            >
              <option value="">All tags</option>
              {allTags.map((tag) => (
                <option key={tag} value={tag}>{tag}</option>
              ))}
            </NativeSelect>
            <Filter size={10} className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground" />
          </div>
        )}

        {/* Search */}
        <div className="relative">
          <Search size={12} className="absolute left-2 top-1/2 -translate-y-1/2 text-muted-foreground" aria-hidden="true" />
          <Input
            type="search"
            placeholder="Search..."
            className="w-36 rounded border border-border bg-card py-1 pl-7 pr-2 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus:outline-none"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            aria-label="Search connections"
          />
        </div>
      </header>

      {/* Connection list */}
      <div
        ref={listRef}
        className="flex-1 overflow-auto p-4"
        role="list"
        aria-label="SSH connection profiles"
        onKeyDown={handleKeyDown}
        tabIndex={0}
      >
        <SshConfigBanner onImport={handleImportConfig} />
        <div className="mx-auto flex max-w-3xl flex-col gap-3">
          {displayProfiles.map((profile, index) => {
            const connState = getConnectionState(profile.id);
            return (
              <div
                key={profile.id}
                role="listitem"
                draggable
                onDragStart={() => handleDragStart(profile.id)}
                onDragOver={(e) => handleDragOver(e, profile.id)}
                onDrop={() => handleDrop(profile.id)}
                onDragEnd={() => { setDraggedId(null); setDragOverId(null); }}
                className={clsx(
                  "transition-all",
                  draggedId === profile.id && "opacity-50",
                  dragOverId === profile.id && draggedId !== profile.id && "border-t-2 border-primary",
                )}
                aria-label={`${profile.label} - ${connState?.status ?? "disconnected"}`}
              >
                <ConnectionCard
                  profile={profile}
                  connectionState={connState}
                  focused={focusedIndex === index}
                  onConnect={() => handleConnect(profile.id)}
                  onDisconnect={() => handleDisconnect(profile.id)}
                  onEdit={() => handleEdit(profile)}
                  onDelete={() => handleDelete(profile.id)}
                  onTogglePin={() => handleTogglePin(profile.id)}
                  onOpenTerminal={() => handleOpenTerminal(profile.id)}
                />
              </div>
            );
          })}
        </div>
      </div>

      {/* Footer actions */}
      <footer className="flex shrink-0 items-center gap-3 border-t border-border px-4 py-3">
        <Button variant="default"
          type="button"
          className="flex items-center gap-1.5 px-3 py-1.5 text-xs"
          onClick={() => { setEditingProfile(null); setFormOpen(true); }}
          aria-label="Add new SSH connection"
        >
          <Plus size={12} />
          Add Connection
        </Button>
        <Button variant="outline"
          type="button"
          className="flex items-center gap-1.5 px-3 py-1.5 text-xs"
          onClick={handleImportConfig}
          aria-label="Import from SSH config file"
        >
          <FileDown size={12} />
          Import from SSH Config
        </Button>
      </footer>

      {/* Connection form dialog */}
      {formOpen && (
        <ConnectionForm
          profile={editingProfile}
          onSave={handleSave}
          onCancel={() => { setFormOpen(false); setEditingProfile(null); }}
        />
      )}

      {/* SSH Config Import dialog */}
      <SshConfigImport
        isOpen={importDialogOpen}
        onClose={() => setImportDialogOpen(false)}
        onImport={handleImportHosts}
        existingProfiles={profiles.map((p) => ({ host: p.host, port: p.port, username: p.username }))}
      />
      </>}

      {/* Sync Pair Dialog — rendered outside sub-view guard so it overlays everything */}
      {syncDialogState && (
        <SyncPairDialog
          profileId={syncDialogState.profileId}
          remotePath={syncDialogState.remotePath}
          onConfirm={handleSyncConfirm}
          onCancel={() => setSyncDialogState(null)}
        />
      )}

      {/* Conflict Resolution Dialog — opened from toast notification action */}
      {activeConflict && (
        <ConflictResolutionDialog
          conflict={activeConflict}
          onResolve={resolveConflict}
          onDismiss={dismissConflict}
        />
      )}
    </div>
  );
}
