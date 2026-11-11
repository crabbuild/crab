import { fileBuffers } from "./lib/file-buffer-store";
import type { FileTransferActivity, FileTransferChange, FileTransferJob } from "../electron/file-transfers";
import { retainNotebookRepositories } from "./lib/jupyter-ipc";
import { invalidateFileQuerySources, retainFileQueryRepositories } from "./lib/file-query";
import { workspaceDocuments } from "./lib/workspace-settings-store";
import { workspaceReducer, type WorkspaceAction } from "./lib/workspace-reducer";
import { noCapabilities, type CapabilitySet } from "../electron/repo-capabilities";
import { parseRepoId } from "../electron/ssh-types";
import React, { createContext, useCallback, useContext, useEffect, useMemo, useReducer, useRef } from "react";
import { RepoHandle, ProgressLine, type FileTransfer, type ProgressComplete, onAgentEvent, mainProcess, rpc, nextJobId, isStorageRepositoryUrl } from "./ipc";

// ── Workspace types ─────────────────────────────────────────────────

/** Loading level for a workspace repo entry. */
export type LoadingLevel = 0 | 1 | 2;

/**
 * Workspace kind: a workspace is either entirely local or entirely remote.
 * There is no hybrid workspace that mixes local and remote repos.
 */
export type WorkspaceKind = "local" | "remote";

/** A repo entry within a workspace (extends beyond OpenRepo for L0/L1). */
export interface WorkspaceRepoEntry {
  /** Relative path from workspace root */
  relativePath: string;
  /** Absolute path */
  absolutePath: string;
  /** Display name */
  name: string;
  /** Current loading level */
  level: LoadingLevel;
  /** Branch name (available at L1+) */
  branch: string | null;
  /** Dirty file count (available at L1+) */
  dirtyCount: number | null;
  /** Ahead count (available at L1+) */
  ahead: number | null;
  /** Behind count (available at L1+) */
  behind: number | null;
  /** Error from last status poll */
  error: string | null;
  /** repo_id when promoted to L2 (matches OpenRepo.repo_id) */
  openRepoId: string | null;
  /** Timestamp of last activation */
  lastAccessedAt: number;
}

export interface WorkspaceConfig {
  version: 1;
  discovery: {
    maxDepth: number;
    excludePatterns: string[];
  };
  repos: {
    /** Manually removed repos (excluded from discovery, relative paths) */
    removed: string[];
  };
  display: {
    tabBarState: string[];
  };
}

export interface Workspace {
  /** Workspace kind: "local" or "remote". No hybrid. */
  kind: WorkspaceKind;
  /** Absolute path to workspace root (local path or remote path) */
  rootPath: string;
  /** Display name (basename of rootPath) */
  name: string;
  /** All discovered repos under rootPath */
  repos: WorkspaceRepoEntry[];
  /** Config as read from .crab/workspace.json */
  config: WorkspaceConfig;
  /** SSH profile ID (only set for remote workspaces) */
  profileId?: string;
  /** SSH host display name (only set for remote workspaces) */
  profileLabel?: string;
}

/** Status result from the poller for a single repo. */
export interface RepoStatus {
  relativePath: string;
  branch: string | null;
  dirtyCount: number;
  ahead: number | null;
  behind: number | null;
  error: string | null;
}

// ── Open repo types ─────────────────────────────────────────────────

// ── SSH connection state ────────────────────────────────────────────

export interface DetectedAgentCli {
  /** CLI name (e.g., "claude", "codex") */
  name: string;
  /** Absolute path on the remote machine, empty if not found */
  path: string;
  /** Whether the binary was found and is executable */
  available: boolean;
  /** Version string if detectable */
  version?: string;
}

export interface SshConnectionState {
  profileId: string;
  status: "disconnected" | "disconnecting" | "connecting" | "deploying" | "connected" | "reconnecting" | "error";
  error?: string;
  serverInfo?: {
    os: string;
    arch: string;
    agentVersion: string;
    hostname: string;
    uptime?: number;
  };
  health?: {
    diskTotal: number;
    diskFree: number;
    memTotal: number;
    memFree: number;
    uptime?: number;
    gpus?: Array<{ name: string; utilization: number; memUsed: number; memTotal: number; temp: number }>;
  };
  /** Cumulative bandwidth usage since connection (bytes in/out). */
  bandwidth?: {
    bytesIn: number;
    bytesOut: number;
  };
  /** Active health alerts (disk >90%, memory >90%, GPU temp >85°C). */
  healthAlerts?: Array<{
    type: "disk" | "memory" | "gpu_temp";
    severity: "warning" | "critical";
    message: string;
    value: number;
    threshold: number;
  }>;
  /** Agent CLIs detected on the remote PATH (claude, codex, etc.) */
  detectedAgents?: DetectedAgentCli[];
  openRepos: string[];
  terminals: string[];
  portForwards: Array<{
    autoStart: boolean;
    id: string;
    direction: "local" | "remote";
    actualLocalPort: number;
    localPort: number;
    remoteHost: string;
    remotePort: number;
    label?: string;
    status: "active" | "stopped" | "error";
    error: any;
  }>;
  latencyMs?: number;
  /** Whether high-latency mode is active (>200ms RTT detected). */
  highLatencyMode?: boolean;
}

// ── Open repo types ─────────────────────────────────────────────────

// Persistent slice of state: the recently opened repos. Everything
// else (active tab, transient dialogs) stays in component state.
export interface OpenRepo extends RepoHandle {
  native_error?: string;
  label: string;
  path?: string;
  openedAt: number;
  /** When true, the repo was restored from localStorage but hasn't
   *  been reopened yet. The sidebar triggers the
   *  actual open on first selection. */
  deferred?: boolean;
  /** When set, RepoDetail switches to this view on first render for
   *  this repo. Used to show the Dashboard immediately after opening
   *  a deferred repo so the workspace state can initialize. */
  initialView?: string;
  /** Capability flags for this repo. Populated when the repo is opened
   *  and updated reactively via `repo:capabilities-changed` events.
   *  Undefined while capabilities are being fetched. */
  capabilities?: CapabilitySet;
}

export interface AppState {
  repos: OpenRepo[];
  activeRepoId: string | null;
  // Ring-buffer of progress lines across all jobs. Trimmed to
  // MAX_LOG_LINES so the renderer stays responsive during a long
  // clone / hydrate.
  logs: ProgressLine[];
  jobs: Record<string, JobState>;
  activityVersion: number;
  activityError: string | null;
  // Ordering and pinning use physical paths or stable SDK snapshot identities.
  // Repos with an explicit sortIndex are shown in that order; repos
  // without one fall back to alphabetic order.
  pinnedPaths: Set<string>;
  repoOrder: string[];
  /** All open workspaces (empty array when none are open) */
  workspaces: Workspace[];
  /** The workspace currently shown in the dashboard (by rootPath).
   *  When null, defaults to the first workspace. */
  activeWorkspaceRootPath: string | null;
  /** Tab bar: ordered list of workspace repo relative paths */
  tabBar: string[];
  /** SSH connection states keyed by profileId */
  sshConnections: Map<string, SshConnectionState>;
}

/** Returns the workspace currently selected for the dashboard view.
 *  Falls back to the first workspace if none is explicitly selected. */
export function getActiveWorkspace(state: AppState): Workspace | null {
  if (state.activeWorkspaceRootPath) {
    return state.workspaces.find((w) => w.rootPath === state.activeWorkspaceRootPath) ?? state.workspaces[0] ?? null;
  }
  return state.workspaces[0] ?? null;
}

export type JobState = Omit<FileTransferJob, "job_id" | "op" | "intent" | "revision" | "repoId"> & {
  op: string;
  intent?: FileTransferJob["intent"];
  lines: number;
  revision?: number;
  repoId?: string;
};

// Logical SDK identities persist; native_session_id never does. A URL alone
// cannot distinguish connections to identical bucket names on different stores.
const LS_RECENT_REPOS = "crab.recentRepos.v1";
const LS_ACTIVE_PATH = "crab.activeRepoPath.v1";
const LS_PINNED_REPOS = "crab.pinnedRepoPaths.v1";
const LS_REPO_ORDER = "crab.repoOrder.v1";
const LS_WORKSPACE = "crab.workspace.v1";
const LS_WORKSPACE_TAB_BAR = "crab.workspaceTabBar.v1";
const LS_WORKSPACE_ACTIVE_REPO = "crab.workspaceActiveRepo.v1";

interface PersistedRepo {
  path: string;
  label: string;
  openedAt: number;
  repo_id?: string;
}

/** Identify UI state without mixing SDK connections at the same URL. */
export function repositoryKey(repo: Pick<OpenRepo, "repo_id" | "path">): string {
  return repo.path && !isStorageRepositoryUrl(repo.path) ? repo.path : repo.repo_id;
}

function loadPersistedRepos(): OpenRepo[] {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(LS_RECENT_REPOS) ?? "[]");
    if (!Array.isArray(value)) return [];
    const paths = new Set<string>();
    const repos: OpenRepo[] = [];
    for (const entry of value) {
      if (!entry || typeof entry !== "object") continue;
      const { path, label, openedAt, repo_id } = entry as Record<string, unknown>;
      if (typeof path !== "string" || !path || typeof label !== "string" || typeof openedAt !== "number" || !Number.isFinite(openedAt)) continue;
      const snapshot = isStorageRepositoryUrl(path);
      const id = snapshot && typeof repo_id === "string" && repo_id ? repo_id : `deferred-${path}`;
      const key = repositoryKey({ repo_id: id, path });
      if (paths.has(key)) continue;
      paths.add(key);
      const remote = snapshot || !!parseRepoId(path);
      repos.push({ repo_id: id, url: remote ? path : "", is_remote: remote, cache_dir: "", native_session_id: null, path, label: label.replace(/^(↳\s*)wt:/, "$1worktree:"), openedAt, deferred: true });
    }
    return repos;
  } catch { return []; }
}

function savePersistedRepos(repos: OpenRepo[]) {
  const persisted: PersistedRepo[] = repos
    .filter((r): r is OpenRepo & { path: string } => typeof r.path === "string" && r.path.length > 0)
    .map((r) => ({ path: r.path, label: r.label, openedAt: r.openedAt, ...(isStorageRepositoryUrl(r.path) ? { repo_id: r.repo_id } : {}) }));
  try {
    localStorage.setItem(LS_RECENT_REPOS, JSON.stringify(persisted));
  } catch {
    // Quota or disabled — fine, this is best-effort.
  }
}

function savePinnedPaths(pinned: Set<string>) {
  try {
    localStorage.setItem(LS_PINNED_REPOS, JSON.stringify([...pinned]));
  } catch {
    // best-effort
  }
}

function loadPinnedPaths(): Set<string> {
  try {
    const raw = localStorage.getItem(LS_PINNED_REPOS);
    if (raw) return new Set(JSON.parse(raw) as string[]);
  } catch {
    // corrupted — start fresh
  }
  return new Set();
}

function saveRepoOrder(order: string[]) {
  try {
    localStorage.setItem(LS_REPO_ORDER, JSON.stringify(order));
  } catch {
    // best-effort
  }
}

function loadRepoOrder(): string[] {
  try {
    const raw = localStorage.getItem(LS_REPO_ORDER);
    if (raw) return JSON.parse(raw) as string[];
  } catch {
    // corrupted — start fresh
  }
  return [];
}

/**
 * Sort repos according to: pinned first (alphabetic among pinned),
 * then unpinned in user-defined order (or alphabetic if no custom order).
 */
export function sortRepos(repos: OpenRepo[], pinnedPaths: Set<string>, repoOrder: string[]): OpenRepo[] {
  const pinned = repos.filter((r) => pinnedPaths.has(repositoryKey(r)));
  const unpinned = repos.filter((r) => !pinnedPaths.has(repositoryKey(r)));

  // Pinned: sort alphabetically by label
  pinned.sort((a, b) => a.label.localeCompare(b.label, undefined, { sensitivity: "base" }));

  // Unpinned: if we have a persisted order, use it; otherwise alphabetic
  if (repoOrder.length > 0) {
    const orderMap = new Map(repoOrder.map((p, i) => [p, i]));
    unpinned.sort((a, b) => {
      const ia = orderMap.get(repositoryKey(a)) ?? Infinity;
      const ib = orderMap.get(repositoryKey(b)) ?? Infinity;
      if (ia !== ib) return ia - ib;
      // Fallback: alphabetic for repos not in the order list
      return a.label.localeCompare(b.label, undefined, { sensitivity: "base" });
    });
  } else {
    unpinned.sort((a, b) => a.label.localeCompare(b.label, undefined, { sensitivity: "base" }));
  }

  return [...pinned, ...unpinned];
}

const MAX_LOG_LINES_HARD_CAP = 20_000;

// Reducer-visible cap. Tightened at runtime by the Settings-driven
// `setMaxLogLines` action so the ring buffer can shrink live.
let runtimeMaxLogLines = 2000;

export type Action =
  | { type: "add-repo"; repo: OpenRepo; activation?: "if-empty" }
  | { type: "remove-repo"; repo_id: string }
  | { type: "set-active"; repo_id: string | null }
  | { type: "pin-repo"; path: string }
  | { type: "unpin-repo"; path: string }
  | { type: "reorder-repos"; orderedPaths: string[] }
  | { type: "file-transfer-state"; activity: FileTransferActivity }
  | { type: "file-transfer-change"; change: FileTransferChange }
  | { type: "file-transfer-error"; error: string }
  | { type: "job-start"; job_id: string; op: string; nativeSessionId?: string }
  | { type: "job-line"; line: ProgressLine }
  | { type: "job-lines-batch"; lines: ProgressLine[] }
  | { type: "job-complete"; job_id: string; status: number; outcome?: ProgressComplete["outcome"] }
  | { type: "set-max-log-lines"; value: number }
  | { type: "clear-logs" }
  | WorkspaceAction
  | { type: "ssh-status-changed"; profileId: string; status: SshConnectionState["status"]; error?: string }
  | { type: "ssh-server-info"; profileId: string; info: NonNullable<SshConnectionState["serverInfo"]> }
  | { type: "ssh-health-update"; profileId: string; health: NonNullable<SshConnectionState["health"]> }
  | { type: "ssh-health-alerts"; profileId: string; alerts: NonNullable<SshConnectionState["healthAlerts"]> }
  | { type: "ssh-agents-detected"; profileId: string; agents: DetectedAgentCli[] }
  | { type: "ssh-latency-update"; profileId: string; latencyMs: number }
  | { type: "ssh-high-latency-mode"; profileId: string; enabled: boolean }
  | { type: "ssh-bandwidth-update"; profileId: string; bytesIn: number; bytesOut: number }
  | { type: "ssh-connection-remove"; profileId: string }
  | { type: "repo-runtime-changed"; repo_id: string; capabilities: CapabilitySet; native_session_id: string | null; error?: string }
  | { type: "repo-capabilities-loaded"; repo_id: string; capabilities: CapabilitySet; expected_session_id: string | null };

function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case "add-repo": {
      const key = repositoryKey(action.repo);
      const repos = [action.repo, ...state.repos.filter(r => r.repo_id !== action.repo.repo_id && !(r.deferred && repositoryKey(r) === key))];
      // If the new repo isn't in the order list yet, append it
      const repoOrder =
        !state.repoOrder.includes(key)
          ? [...state.repoOrder, key]
          : state.repoOrder;
      // Deferred repos are sidebar placeholders — don't activate them
      // automatically. Activating a deferred repo causes every
      // activeRepo-watching hook (workspace settings, tracking patterns,
      // TimelineView, etc.) to fire RPCs against an id the agent has never
      // seen, producing "repository not open" errors and unbounded memory
      // growth from concurrent git_log / read_text calls.
      const activeRepoId = action.repo.deferred || (action.activation === "if-empty" && state.activeRepoId !== null)
        ? state.activeRepoId
        : action.repo.repo_id;
      return { ...state, repos, activeRepoId, repoOrder };
    }
    case "remove-repo": {
      const repos = state.repos.filter((r) => r.repo_id !== action.repo_id);
      const activeRepoId =
        state.activeRepoId === action.repo_id ? (repos[0]?.repo_id ?? null) : state.activeRepoId;
      return { ...state, repos, activeRepoId };
    }
    case "set-active": {
      // Update openedAt so the "last touched" repo is always the most
      // recently activated one — used for restore-on-relaunch ordering.
      const repos = action.repo_id
        ? state.repos.map((r) =>
            r.repo_id === action.repo_id ? { ...r, openedAt: Date.now() } : r,
          )
        : state.repos;

      // Auto-select the parent workspace when a repo is activated.
      // This ensures the TopBar dropdown and dashboard follow the repo.
      let activeWorkspaceRootPath = state.activeWorkspaceRootPath;
      if (action.repo_id) {
        for (const ws of state.workspaces) {
          if (ws.repos.some((e) => e.openRepoId === action.repo_id)) {
            activeWorkspaceRootPath = ws.rootPath;
            break;
          }
        }
      }

      return { ...state, repos, activeRepoId: action.repo_id, activeWorkspaceRootPath };
    }
    case "pin-repo": {
      const pinnedPaths = new Set(state.pinnedPaths);
      pinnedPaths.add(action.path);
      return { ...state, pinnedPaths };
    }
    case "unpin-repo": {
      const pinnedPaths = new Set(state.pinnedPaths);
      pinnedPaths.delete(action.path);
      return { ...state, pinnedPaths };
    }
    case "reorder-repos": {
      return { ...state, repoOrder: action.orderedPaths };
    }
    case "file-transfer-error": return { ...state, activityError: action.error };
    case "file-transfer-state": {
      if (action.activity.version < state.activityVersion) return state;
      const jobs = { ...state.jobs };
      for (const job of action.activity.jobs) jobs[job.job_id] = { ...job, lines: jobs[job.job_id]?.lines ?? 0 };
      return { ...state, jobs, activityVersion: action.activity.version, activityError: action.activity.error };
    }
    case "file-transfer-change": {
      const change = action.change;
      if (change.version < state.activityVersion) return state;
      const jobs = { ...state.jobs };
      for (const id of change.removed ?? []) delete jobs[id];
      if (change.job) jobs[change.job.job_id] = { ...change.job, lines: jobs[change.job.job_id]?.lines ?? 0 };
      return { ...state, jobs, activityVersion: change.version, activityError: change.error };
    }
    case "job-start": {
      const now = Date.now();
      return {
        ...state,
        jobs: {
          ...state.jobs,
          [action.job_id]: {
            op: action.op,
            nativeSessionId: action.nativeSessionId,
            status: "running",
            lines: 0,
            startedAt: now,
            updatedAt: now,
          },
        },
      };
    }
    case "job-line": {
      const logs = [...state.logs, action.line];
      const cap = Math.min(runtimeMaxLogLines, MAX_LOG_LINES_HARD_CAP);
      if (logs.length > cap) logs.splice(0, logs.length - cap);
      const existing = state.jobs[action.line.job_id];
      const jobs = existing
        ? {
            ...state.jobs,
            [action.line.job_id]: {
              ...existing,
              lines: existing.lines + 1,
              updatedAt: Date.now(),
            },
          }
        : {
            ...state.jobs,
            [action.line.job_id]: {
              op: action.line.op,
              status: "running" as const,
              lines: 1,
              startedAt: Date.now(),
              updatedAt: Date.now(),
            },
          };
      return { ...state, logs, jobs };
    }
    case "job-lines-batch": {
      if (action.lines.length === 0) return state;
      const logs = [...state.logs, ...action.lines];
      const cap = Math.min(runtimeMaxLogLines, MAX_LOG_LINES_HARD_CAP);
      if (logs.length > cap) logs.splice(0, logs.length - cap);
      const now = Date.now();
      const jobs = { ...state.jobs };
      for (const line of action.lines) {
        const existing = jobs[line.job_id];
        if (existing) {
          jobs[line.job_id] = { ...existing, lines: existing.lines + 1, updatedAt: now };
        } else {
          jobs[line.job_id] = { op: line.op, status: "running" as const, lines: 1, startedAt: now, updatedAt: now };
        }
      }
      return { ...state, logs, jobs };
    }
    case "job-complete": {
      const existing = state.jobs[action.job_id];
      if (!existing || existing.intent) return state;
      return {
        ...state,
        jobs: {
          ...state.jobs,
          [action.job_id]: {
            ...existing,
            status: action.outcome === "cancelled" ? "cancelled" : action.status === 0 ? "done" : "failed",
            exitCode: action.status,
            updatedAt: Date.now(),
          },
        },
      };
    }
    case "clear-logs":
      return { ...state, logs: [] };
    case "set-max-log-lines": {
      runtimeMaxLogLines = Math.max(100, Math.min(action.value, MAX_LOG_LINES_HARD_CAP));
      // Trim current buffer to respect the new cap right away.
      const logs =
        state.logs.length > runtimeMaxLogLines
          ? state.logs.slice(state.logs.length - runtimeMaxLogLines)
          : state.logs;
      return { ...state, logs };
    }

    // ── Workspace actions ─────────────────────────────────────────────

    case "set-active-workspace":
    case "workspace-set":
    case "workspace-close":
    case "workspace-update-status":
    case "workspace-promote-l2":
    case "workspace-remove-repo":
    case "tab-bar-activate":
    case "tab-bar-close":
      return { ...state, ...workspaceReducer(state, action) };

    // ── SSH connection actions ────────────────────────────────────────

    case "ssh-status-changed": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      const updated: SshConnectionState = existing
        ? { ...existing, status: action.status, error: action.error }
        : { profileId: action.profileId, status: action.status, error: action.error, openRepos: [], terminals: [], portForwards: [] };
      conns.set(action.profileId, updated);
      return { ...state, sshConnections: conns };
    }
    case "ssh-server-info": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, { ...existing, serverInfo: action.info });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-health-update": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, { ...existing, health: action.health });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-health-alerts": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, { ...existing, healthAlerts: action.alerts });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-agents-detected": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, { ...existing, detectedAgents: action.agents });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-latency-update": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, { ...existing, latencyMs: action.latencyMs });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-high-latency-mode": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, { ...existing, highLatencyMode: action.enabled });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-bandwidth-update": {
      const conns = new Map(state.sshConnections);
      const existing = conns.get(action.profileId);
      if (existing) {
        conns.set(action.profileId, {
          ...existing,
          bandwidth: { bytesIn: action.bytesIn, bytesOut: action.bytesOut },
        });
      }
      return { ...state, sshConnections: conns };
    }
    case "ssh-connection-remove": {
      const conns = new Map(state.sshConnections);
      conns.delete(action.profileId);
      return { ...state, sshConnections: conns };
    }

    // ── Repo capabilities ────────────────────────────────────────────

    case "repo-capabilities-loaded": {
      const target = state.repos.find((r) => r.repo_id === action.repo_id);
      // An initial query cannot re-enable controls after a later process loss.
      if (!target || target.native_session_id !== action.expected_session_id) return state;
      return { ...state, repos: state.repos.map(repo => repo.repo_id === action.repo_id ? { ...repo, capabilities: action.capabilities } : repo) };
    }
    case "repo-runtime-changed": {
      return { ...state, repos: state.repos.map(repo => repo.repo_id === action.repo_id ? { ...repo, capabilities: action.capabilities, native_session_id: action.native_session_id, native_error: action.error } : repo) };
    }

  }
}

function initialState(): AppState {
  const repos = loadPersistedRepos();
  const repoOrder = loadRepoOrder();
  for (const repo of repos) if (!repoOrder.includes(repositoryKey(repo))) repoOrder.push(repositoryKey(repo));
  return {
    repos,
    activeRepoId: null,
    logs: [],
    jobs: {},
    activityVersion: -1,
    activityError: null,
    pinnedPaths: loadPinnedPaths(),
    repoOrder,
    workspaces: [],
    activeWorkspaceRootPath: null,
    tabBar: [],
    sshConnections: new Map(),
  };
}

interface Ctx {
  state: AppState;
  dispatch: React.Dispatch<Action>;
  addRepo: (r: OpenRepo) => void;
  removeRepo: (id: string) => void;
  setActive: (id: string | null) => void;
  pinRepo: (path: string) => void;
  unpinRepo: (path: string) => void;
  reorderRepos: (orderedPaths: string[]) => void;
  startJob: (job_id: string, op: string) => void;
  runFileTransfer: (repoId: string, selection: FileTransfer, nativeSessionId: string, jobId?: string) => Promise<{ written_bytes: number }>;
  closeTransferRepo: (repoId: string) => Promise<unknown>;
  setMaxLogLines: (value: number) => void;
  clearLogs: () => void;
}

const StoreContext = createContext<Ctx | null>(null);

export function StoreProvider({ children }: { children: React.ReactNode }) {
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  useEffect(() => { retainNotebookRepositories(new Set(state.repos.map(repo => repo.repo_id))); }, [state.repos]);
  useEffect(() => {
    retainFileQueryRepositories(new Map(state.repos.map(repo => [repo.repo_id, repo.native_session_id])));
  }, [state.repos]);

  useEffect(() => {
    // Batch progress lines to avoid re-rendering the entire tree on every
    // single line. Lines are buffered and flushed at most every 100ms.
    let lineBuf: ProgressLine[] = [];
    let flushTimer: ReturnType<typeof setTimeout> | null = null;

    const flushLines = () => {
      flushTimer = null;
      if (lineBuf.length === 0) return;
      const batch = lineBuf;
      lineBuf = [];
      dispatch({ type: "job-lines-batch", lines: batch });
    };

    const protectPendingEdits = (event: BeforeUnloadEvent) => {
      if (!fileBuffers.pendingCount() && !workspaceDocuments.pendingCount()) return;
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", protectPendingEdits);
    let loadingActivity = true;
    const activityChanges: FileTransferChange[] = [];
    const off = onAgentEvent((evt) => {
      if (evt.method === "activity/file-transfer") {
        const change = evt.params as FileTransferChange;
        if (loadingActivity) activityChanges.push(change);
        else dispatch({ type: "file-transfer-change", change });
      } else if (evt.method === "progress/line") {
        lineBuf.push(evt.params as ProgressLine);
        if (!flushTimer) {
          flushTimer = setTimeout(flushLines, 100);
        }
      } else if (evt.method === "progress/complete") {
        // Flush any pending lines before marking complete so the UI
        // sees the final lines before the job transitions to done.
        if (lineBuf.length > 0) flushLines();
        const p = evt.params as ProgressComplete;
        dispatch({ type: "job-complete", job_id: p.job_id, status: p.status, outcome: p.outcome });
      } else if (evt.method === "repo:storage-source-changed") {
        invalidateFileQuerySources((evt.params as { repo_ids: string[] }).repo_ids);
      } else if (evt.method === "repo:capabilities-changed") {
        const p = evt.params as { repo_id: string; capabilities: CapabilitySet; native_session_id: string | null; error?: string };
        fileBuffers.runtime(p.repo_id, p.capabilities.canEdit, p.native_session_id);
        dispatch({ type: "repo-runtime-changed", ...p });
      }
    });
    let disposed = false;
    void rpc.fileTransferHistory().then(activity => {
      if (!disposed) dispatch({ type: "file-transfer-state", activity });
    }).catch(error => {
      if (!disposed) dispatch({ type: "file-transfer-error", error: error instanceof Error ? error.message : String(error) });
    }).finally(() => {
      loadingActivity = false;
      if (!disposed) for (const change of activityChanges) dispatch({ type: "file-transfer-change", change });
    });
    return () => {
      disposed = true;
      window.removeEventListener("beforeunload", protectPendingEdits);
      off();
      if (flushTimer) clearTimeout(flushTimer);
      // Flush remaining lines on unmount.
      if (lineBuf.length > 0) {
        dispatch({ type: "job-lines-batch", lines: lineBuf });
      }
    };
  }, []);

  // Fetch capabilities for repos that don't have them yet. This runs
  // whenever the repo list changes and picks up newly-opened repos.
  // Deferred repos (not yet opened via the agent) are skipped.
  // Only pending queries are shared. Reopening the same logical repository
  // must fetch for its new native session, not reuse a completed admission.
  const capabilitiesPending = useRef(new Map<string, { nativeSessionId: string | null }>());
  useEffect(() => {
    for (const repo of state.repos) {
      if (repo.deferred) continue;
      if (repo.capabilities !== undefined) continue;
      if (capabilitiesPending.current.get(repo.repo_id)?.nativeSessionId === repo.native_session_id) continue;
      const request = { nativeSessionId: repo.native_session_id };
      capabilitiesPending.current.set(repo.repo_id, request);
      // Fire-and-forget: fetch capabilities and dispatch the result.
      mainProcess.getCapabilities(repo.repo_id).then(
        (caps) => {
          dispatch({ type: "repo-capabilities-loaded", repo_id: repo.repo_id, capabilities: caps, expected_session_id: repo.native_session_id });
        },
        () => {
          // A failed query does not establish native availability.
          dispatch({ type: "repo-capabilities-loaded", repo_id: repo.repo_id, capabilities: noCapabilities(), expected_session_id: repo.native_session_id });
        },
      ).finally(() => {
        if (capabilitiesPending.current.get(repo.repo_id) === request) capabilitiesPending.current.delete(repo.repo_id);
      });
    }
  }, [state.repos]);

  // The active key matches the same physical path or logical SDK identity
  // used by sidebar state; two endpoints may share the exact same URL.
  useEffect(() => {
    savePersistedRepos(state.repos);
    const activeRepo = state.repos.find((r) => r.repo_id === state.activeRepoId);
    const activePath = activeRepo ? repositoryKey(activeRepo) : null;
    if (activePath) localStorage.setItem(LS_ACTIVE_PATH, activePath);
    else {
      const pendingPath = localStorage.getItem(LS_ACTIVE_PATH);
      // Startup may be waiting for host trust or a recoverable connection error.
      // Keep the selected path while its deferred entry remains available to retry.
      if (!state.repos.some(repo => repo.deferred && repositoryKey(repo) === pendingPath)) localStorage.removeItem(LS_ACTIVE_PATH);
    }
  }, [state.repos, state.activeRepoId]);

  // Persist pin and order state
  useEffect(() => {
    savePinnedPaths(state.pinnedPaths);
  }, [state.pinnedPaths]);

  useEffect(() => {
    saveRepoOrder(state.repoOrder);
  }, [state.repoOrder]);

  // Persist workspace state.
  useEffect(() => {
    try {
      if (state.workspaces.length > 0) {
        const paths = state.workspaces.map((w) => w.rootPath);
        localStorage.setItem(LS_WORKSPACE, JSON.stringify(paths));
      } else {
        localStorage.removeItem(LS_WORKSPACE);
      }
    } catch {
      // best-effort
    }
  }, [state.workspaces]);

  useEffect(() => {
    try {
      if (state.tabBar.length > 0) {
        localStorage.setItem(LS_WORKSPACE_TAB_BAR, JSON.stringify(state.tabBar));
      } else {
        localStorage.removeItem(LS_WORKSPACE_TAB_BAR);
      }
    } catch {
      // best-effort
    }
  }, [state.tabBar]);

  // Persist last-active workspace repo relative path.
  useEffect(() => {
    try {
      if (state.workspaces.length > 0 && state.activeRepoId) {
        for (const ws of state.workspaces) {
          const activeEntry = ws.repos.find(
            (e) => e.openRepoId === state.activeRepoId,
          );
          if (activeEntry) {
            localStorage.setItem(LS_WORKSPACE_ACTIVE_REPO, activeEntry.relativePath);
            break;
          }
        }
      }
    } catch {
      // best-effort
    }
  }, [state.workspaces, state.activeRepoId]);

  // Stable action helpers — dispatch identity never changes (React guarantee
  // for useReducer), so these closures can be memoized once. Components that
  // destructure only actions (e.g. `const { dispatch } = useStore()`) still
  // re-render when state changes due to Context limitations, but the batched
  // progress line dispatch (100ms throttle) keeps the frequency manageable.
  const closeTransferRepo = useCallback((repoId: string) => rpc.close(repoId), []);

  const value = useMemo<Ctx>(
    () => ({
      state,
      dispatch,
      addRepo: (r) => dispatch({ type: "add-repo", repo: r }),
      removeRepo: (id) => dispatch({ type: "remove-repo", repo_id: id }),
      setActive: (id) => dispatch({ type: "set-active", repo_id: id }),
      pinRepo: (path) => dispatch({ type: "pin-repo", path }),
      unpinRepo: (path) => dispatch({ type: "unpin-repo", path }),
      reorderRepos: (orderedPaths) => dispatch({ type: "reorder-repos", orderedPaths }),
      startJob: (job_id, op) => dispatch({ type: "job-start", job_id, op }),
      runFileTransfer: (repoId, selection, nativeSessionId, jobId) => {
        const op = "verb" in selection ? selection.verb : "hydrate";
        return rpc.fileTransfer(repoId, selection, jobId ?? nextJobId(op), nativeSessionId);
      },
      closeTransferRepo,
      setMaxLogLines: (value) => dispatch({ type: "set-max-log-lines", value }),
      clearLogs: () => dispatch({ type: "clear-logs" }),
    }),
    [state, closeTransferRepo],
  );

  return <StoreContext.Provider value={value}>{children}</StoreContext.Provider>;
}

export function useStore(): Ctx {
  const ctx = useContext(StoreContext);
  if (!ctx) throw new Error("useStore used outside StoreProvider");
  return ctx;
}

export function useActiveRepo(): OpenRepo | null {
  const { state } = useStore();
  return useCallback(() => state.repos.find((r) => r.repo_id === state.activeRepoId) ?? null, [state])();
}
