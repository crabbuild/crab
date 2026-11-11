/**
 * Remote repo sidebar extension.
 *
 * Extends the sidebar repo list to show remote repos with visual
 * distinction: coloured SSH badge, connection status dot, hostname
 * label. Supports quick-connect, persistence, and connection-aware
 * grouping.
 */

import { useCallback, useMemo, useState } from "react";
import clsx from "clsx";
import {
  FolderGit2,
  Loader2,
  Plug,
  Terminal,
  X,
} from "lucide-react";
import { RemoteBadge } from "./RemoteBadge";

// ── Types ────────────────────────────────────────────────────────

export interface RemoteRepoEntry {
  repoId: string;
  profileId: string;
  profileLabel: string;
  profileColour?: string;
  remotePath: string;
  name: string;
  connected: boolean;
  connecting?: boolean;
}

interface RemoteRepoSidebarProps {
  remoteRepos: RemoteRepoEntry[];
  activeRepoId?: string;
  groupByConnection?: boolean;
  onSelectRepo: (repoId: string) => void;
  onConnect: (profileId: string) => void;
  onCloseRepo: (repoId: string) => void;
  onOpenTerminal: (profileId: string, path: string) => void;
}

// ── Persistence ──────────────────────────────────────────────────

const STORAGE_KEY = "crab.remoteRepos";

export function loadPersistedRemoteRepos(): RemoteRepoEntry[] {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (!stored) return [];
    return JSON.parse(stored) as RemoteRepoEntry[];
  } catch {
    return [];
  }
}

export function persistRemoteRepos(repos: RemoteRepoEntry[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(repos));
  } catch {
    // Storage full or unavailable — non-fatal
  }
}

// ── Component ────────────────────────────────────────────────────

export function RemoteRepoSidebar({
  remoteRepos,
  activeRepoId,
  groupByConnection = false,
  onSelectRepo,
  onConnect,
  onCloseRepo,
  onOpenTerminal,
}: RemoteRepoSidebarProps) {
  const grouped = useMemo(() => {
    if (!groupByConnection) return null;

    const groups = new Map<string, RemoteRepoEntry[]>();
    for (const repo of remoteRepos) {
      const key = repo.profileId;
      const existing = groups.get(key) ?? [];
      existing.push(repo);
      groups.set(key, existing);
    }
    return groups;
  }, [remoteRepos, groupByConnection]);

  if (remoteRepos.length === 0) return null;

  return (
    <div className="border-t border-border border-border pt-1">
      <div className="px-3 py-1 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground text-muted-foreground">
        Remote
      </div>

      {groupByConnection && grouped ? (
        Array.from(grouped.entries()).map(([profileId, repos]) => (
          <div key={profileId}>
            <div className="px-3 py-0.5 text-[10px] text-muted-foreground text-muted-foreground">
              {repos[0]?.profileLabel ?? profileId}
            </div>
            {repos.map((repo) => (
              <RemoteRepoRow
                key={repo.repoId}
                repo={repo}
                isActive={repo.repoId === activeRepoId}
                onSelect={() => onSelectRepo(repo.repoId)}
                onConnect={() => onConnect(repo.profileId)}
                onClose={() => onCloseRepo(repo.repoId)}
                onOpenTerminal={() => onOpenTerminal(repo.profileId, repo.remotePath)}
              />
            ))}
          </div>
        ))
      ) : (
        remoteRepos.map((repo) => (
          <RemoteRepoRow
            key={repo.repoId}
            repo={repo}
            isActive={repo.repoId === activeRepoId}
            onSelect={() => onSelectRepo(repo.repoId)}
            onConnect={() => onConnect(repo.profileId)}
            onClose={() => onCloseRepo(repo.repoId)}
            onOpenTerminal={() => onOpenTerminal(repo.profileId, repo.remotePath)}
          />
        ))
      )}
    </div>
  );
}

// ── Repo Row ─────────────────────────────────────────────────────

interface RemoteRepoRowProps {
  repo: RemoteRepoEntry;
  isActive: boolean;
  onSelect: () => void;
  onConnect: () => void;
  onClose: () => void;
  onOpenTerminal: () => void;
}

function RemoteRepoRow({
  repo,
  isActive,
  onSelect,
  onConnect,
  onClose,
  onOpenTerminal,
}: RemoteRepoRowProps) {
  const [hovered, setHovered] = useState(false);

  const handleClick = useCallback(() => {
    if (!repo.connected && !repo.connecting) {
      // Quick-connect: auto-connect before opening
      onConnect();
      return;
    }
    onSelect();
  }, [repo.connected, repo.connecting, onConnect, onSelect]);

  return (
    <div
      className={clsx(
        "group flex items-center gap-1.5 px-3 py-1 cursor-pointer",
        "hover:bg-background hover:bg-card/50",
        isActive && "bg-info/10 bg-info/20 border-l-2 border-info",
      )}
      onClick={handleClick}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      role="treeitem"
      aria-selected={isActive}
      aria-label={`Remote repository: ${repo.name} on ${repo.profileLabel}`}
    >
      {/* Status indicator */}
      {repo.connecting ? (
        <Loader2 className="w-3 h-3 animate-spin text-warning shrink-0" />
      ) : (
        <FolderGit2
          className={clsx(
            "w-3.5 h-3.5 shrink-0",
            repo.connected ? "text-info" : "text-muted-foreground",
          )}
        />
      )}

      {/* Repo name */}
      <span className="text-sm truncate flex-1">{repo.name}</span>

      {/* SSH badge */}
      <RemoteBadge
        colour={repo.profileColour}
        label={repo.profileLabel}
        connected={repo.connected}
        compact
      />

      {/* Hover actions */}
      {hovered && repo.connected && (
        <div className="flex items-center gap-0.5">
          <button
            onClick={(e) => { e.stopPropagation(); onOpenTerminal(); }}
            className="p-0.5 rounded hover:bg-card hover:bg-card"
            title="Open Terminal"
            aria-label="Open terminal at repo root"
          >
            <Terminal className="w-3 h-3" />
          </button>
          <button
            onClick={(e) => { e.stopPropagation(); onClose(); }}
            className="p-0.5 rounded hover:bg-card hover:bg-card"
            title="Close"
            aria-label="Close remote repo"
          >
            <X className="w-3 h-3" />
          </button>
        </div>
      )}

      {/* Disconnected overlay */}
      {!repo.connected && !repo.connecting && hovered && (
        <button
          onClick={(e) => { e.stopPropagation(); onConnect(); }}
          className="p-0.5 rounded hover:bg-card hover:bg-card"
          title="Connect"
          aria-label="Connect to remote server"
        >
          <Plug className="w-3 h-3 text-muted-foreground" />
        </button>
      )}
    </div>
  );
}
