import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { SearchAddon } from "@xterm/addon-search";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import { SerializeAddon } from "@xterm/addon-serialize";
import "@xterm/xterm/css/xterm.css";
import {
  X,
  ChevronDown,
  ChevronDown as DropdownIcon,
  SplitSquareHorizontal,
  SplitSquareVertical,
  Terminal as TerminalIcon,
  LayoutGrid,
  Copy,
  Radio,
  ArrowDownToLine,
  Lock,
  Unlock,
  FileDown,
  Command,
  UserPlus,
  Palette,
  SendHorizonal,
  SlidersHorizontal,
  AlertTriangle,
  ArrowUp,
  ArrowDown,
  ClipboardCopy,
  Type,
  Bookmark,
  RotateCcw,
  PanelLeftOpen,
  PanelRightOpen,
  Search,
  EyeOff,
  Pin,
  PinOff,
  Trash2,
  Columns2,
  MoreHorizontal,
  Circle,
} from "lucide-react";
import clsx from "clsx";
import { OpenRepo } from "../state";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import {
  registerTerminal,
  appendOutput,
  markTerminalClosed,
  removeTerminal,
  terminalTail,
} from "../lib/terminal-registry";
import {
  TERMINAL_SCHEMES,
  getScheme,
  getStoredSchemeId,
  resolveTheme,
  setStoredSchemeId,
} from "../lib/terminal-themes";
import {
  attachShellIntegration,
  formatDuration,
  type ShellIntegrationState,
} from "../lib/terminal-shell-integration";
import {
  validateScrollbackConfig,
  estimateMemory,
  MIN_MAX_LINES,
  MAX_MAX_LINES,
  type ScrollbackConfig,
} from "../lib/terminal-scrollback";
import {
  analyzePaste,
  wrapBracketedPaste,
  DEFAULT_PASTE_GUARD_CONFIG,
  type PasteGuardConfig,
  type PasteAnalysis,
} from "../lib/terminal-paste-guard";
import { PasteConfirmDialog } from "../components/PasteConfirmDialog";
import {
  shouldNotify,
  formatNotification,
  DEFAULT_NOTIFICATION_CONFIG,
  type NotificationConfig,
  type CommandCompletion,
} from "../lib/terminal-notifications";
import { publishToast } from "../lib/toasts";
import { TerminalBreadcrumbBar } from "../components/TerminalBreadcrumbBar";
import {
  serializeLayout,
  deserializeLayout,
  type TerminalViewState,
  type SaveStateRequest,
} from "../lib/terminal-persistence";
import {
  queueRestoreSnapshot,
  replayQueuedRestoreSnapshot,
  type PendingRestoreSnapshotMap,
} from "../lib/terminal-restore-snapshots";
import {
  type SplitNode,
  type LeafNode,
  type LeafTab,
  type SplitDirection,
  createLeaf,
  splitLeafInPlace,
  removeLeaf,
  updateLeaf,
  findLeaf,
  findLeafByTab,
  getAllLeaves,
  getAllTabIds,
  getNextLeaf,
  getPrevLeaf,
  PRESET_LAYOUTS,
  createPresetLayout,
} from "../lib/terminal-split-tree";
import { SplitTreeRenderer } from "../components/SplitTreeRenderer";
import { PresetDropdown } from "../components/PresetDropdown";
import {
  loadFontConfig,
  applyFontConfig,
  extractDropPaths,
  detectAgent,
  stripAnsiCodes,
  getLastCommand,
  BookmarkManager,
  getTimeFilteredRange,
  type FontConfig,
  type SemanticFilter,
} from "../lib/terminal-ux-enhancements";
import {
  buildFontFamily,
  attachWebgl,
  rebuildWebgl,
  type WebglState,
} from "../lib/terminal-font";
import { TerminalFontSettings } from "../components/TerminalFontSettings";
import { TerminalStatusBar } from "../components/TerminalStatusBar";
import { TerminalSearchEnhanced } from "../components/TerminalSearchEnhanced";
import { TerminalBookmarksPanel } from "../components/TerminalBookmarks";
import { usePtyMode } from "../lib/pty-mode-context";

// --- Types ---

export type TerminalStartupRequest = Pick<
  import("../lib/terminal-presets").StartupCommand,
  "name" | "command" | "cwd" | "env" | "agentMeta"
>;

export interface SshTerminalRequest {
  profileId: string;
  hostname: string;
  colour?: string;
  cwd?: string;
}

interface Props {
  persistenceKey: string;
  sshRequest?: SshTerminalRequest | null;
  onSshRequestHandled?: () => void;
  startupRequest?: TerminalStartupRequest | null;
  onStartupRequestHandled?: () => void;
  repo: OpenRepo;
  /**
   * Optional — when provided, non-zero exit codes surface a "Send error
   * to agent" pill in the toolbar. The callback receives a pre-formatted
   * feedback string (last 200 lines + cwd + exit code) ready to drop
   * into the chat composer.
   */
  onSendErrorToAgent?: (feedback: string) => void;
  /**
   * Whether this terminal view is currently visible. When false, the
   * container is hidden via `display: none` by the parent (e.g.
   * TerminalKeepAlive). On transition from false → true, all xterm
   * instances are re-fitted to account for container resize while hidden.
   * Defaults to `true` for backward compatibility.
   */
  visible?: boolean;
  /**
   * Default backend for newly spawned local terminals. Normal repo terminals
   * remain in-process by default; agent-task terminals opt into daemon mode so
   * the PTY survives view unmount/remount cycles.
   */
  defaultSessionType?: "daemon" | "in-process";
}

interface TermTab {
  id: string;
  title: string;
  shell: string;
  /** When set, indicates this tab was spawned with a startup command
   *  and is waiting for the agent to produce output. Used to show
   *  a loading indicator in the viewport. */
  startupCommand?: string;
  /** When set, this terminal is an SSH remote terminal. */
  sshRemote?: {
    profileId: string;
    hostname: string;
    colour?: string;
  };
  /** The resolved session type returned by the main process on spawn. */
  resolvedSessionType?: "daemon" | "in-process";
  /** When set, this tab was cold-restored from a checkpoint. */
  coldRestored?: {
    savedAt: number;
  };
}

interface TermInstance {
  xterm: XTerm;
  fit: FitAddon;
  search: SearchAddon;
  serialize: SerializeAddon;
  webglState: WebglState;
  memoryCheckInterval?: ReturnType<typeof setInterval>;
}

/** Saved terminal profile. */
interface TermProfile {
  name: string;
  shell: string;
  cwd?: string;
  env?: Record<string, string>;
  startupCommand?: string;
  agentMeta?: AgentMeta;
}

/** Pending cold-restore request waiting for user confirmation. */
interface ColdRestoreRequest {
  id: string;
  savedAt: number;
  sanitizedKeys: string[];
  cwd: string | null;
  cols: number;
  rows: number;
  /** User-provided values for sanitized env keys (filled in the dialog). */
  envValues: Record<string, string>;
}

const LS_PROFILES_KEY = "crab.termProfiles";
const LS_HISTORY_KEY = "crab.termHistory";
const MAX_HISTORY = 50;

// --- IPC ---

const sshTermIpc = {
  openTerminal: (profileId: string, cols: number, rows: number, cwd?: string) =>
    (window.crab as any).ssh.openTerminal(profileId, cols, rows, cwd) as Promise<{ terminalId: string }>,
  closeTerminal: (terminalId: string) =>
    (window.crab as any).ssh.closeTerminal(terminalId) as Promise<void>,
  resize: (terminalId: string, cols: number, rows: number) =>
    (window.crab as any).ssh.terminalResize(terminalId, cols, rows) as Promise<void>,
  write: (terminalId: string, data: string) =>
    (window.crab as any).ssh.terminalWrite(terminalId, data) as Promise<void>,
  reconnect: (terminalId: string) =>
    (window.crab as any).ssh.terminalReconnect(terminalId) as Promise<string>,
  onData: (listener: (evt: { terminalId: string; data: string }) => void) =>
    (window.crab as any).ssh.onTerminalData(listener) as () => void,
  onClosed: (listener: (evt: { terminalId: string; profileId: string }) => void) =>
    (window.crab as any).ssh.onTerminalClosed(listener) as () => void,
  onSessionEnded: (listener: (evt: { terminalId: string; profileId: string }) => void) =>
    (window.crab as any).ssh.onTerminalSessionEnded(listener) as () => void,
  onResumed: (listener: (evt: { terminalId: string; profileId: string }) => void) =>
    (window.crab as any).ssh.onTerminalResumed(listener) as () => void,
};

interface AgentMeta {
  taskId: string;
  agentType: string;
  worktreePath: string;
}

interface DaemonSessionSummary {
  id: string;
  daemonSessionId: string;
  state: string;
  shellState?: string;
  isAlive: boolean;
  pid: number | null;
  initialCwd?: string | null;
  cwd: string | null;
  cols: number;
  rows: number;
  createdAt: number;
  lastDataAt: number;
  agentMeta?: AgentMeta & {
    activityState?: string;
    stateChangedAt?: number;
  };
}

interface DaemonAttachResult {
  id: string;
  daemonSessionId: string;
  snapshot: string | null;
  cwd: string | null;
  summary: DaemonSessionSummary;
}

interface TerminalWorkspaceStateV1 {
  version: 1;
  openSessionIds: string[];
  activeSessionId: string | null;
  pinnedSessionIds: string[];
  hiddenSessionIds: string[];
  sessionSidebarSide: TerminalSessionSidebarSide;
  titles: Record<string, string>;
  updatedAt: number;
}

type TerminalSessionSidebarSide = "left" | "right";

const ptyIpc = {
  spawn: (
    cwd: string,
    shell?: string,
    extraEnv?: Record<string, string>,
    startupCommand?: string,
    agentMeta?: AgentMeta,
    sessionType?: "daemon" | "in-process",
  ) =>
    (window.crab as any).termSpawn({
      cwd,
      shell,
      env: extraEnv,
      startupCommand,
      // Agent sessions use daemon mode for durability (survive app restarts).
      // User terminals use in-process mode — ephemeral, no session leak risk.
      sessionType: sessionType ?? (agentMeta != null ? "daemon" : "in-process"),
      ...(agentMeta != null ? { agentMeta } : {}),
    }) as Promise<{ id: string; resolvedSessionType?: "daemon" | "in-process" }>,
  write: (id: string, data: string) =>
    (window.crab as any).termWrite(id, data) as Promise<void>,
  resize: (id: string, cols: number, rows: number) =>
    (window.crab as any).termResize(id, cols, rows) as Promise<void>,
  kill: (id: string) =>
    (window.crab as any).termKill(id) as Promise<void>,
  listDaemonSessions: (repoPath: string) =>
    (window.crab as any).termListDaemonSessions(repoPath) as Promise<DaemonSessionSummary[]>,
  attachDaemonSession: (daemonSessionId: string, repoPath: string) =>
    (window.crab as any).termAttachDaemonSession(daemonSessionId, repoPath) as Promise<DaemonAttachResult>,
  killDaemonSession: (daemonSessionId: string, repoPath: string) =>
    (window.crab as any).termKillDaemonSession(daemonSessionId, repoPath) as Promise<void>,
  detachDaemonSession: (rendererId: string) =>
    (window.crab as any).termDetachDaemonSession(rendererId) as Promise<void>,
  listShells: () =>
    (window.crab as any).termListShells() as Promise<string[]>,
  poolHint: (cwd: string) =>
    (window.crab as any).termPoolHint(cwd) as Promise<void>,
  onData: (listener: (evt: { id: string; data: string }) => void) =>
    (window.crab as any).onTermData(listener) as () => void,
  onExit: (listener: (evt: { id: string; exitCode: number }) => void) =>
    (window.crab as any).onTermExit(listener) as () => void,
  onSaveRequest: (listener: () => void) =>
    (window.crab as any).onTermSaveRequest(listener) as () => void,
  onColdRestore: (listener: (evt: {
    id: string;
    savedAt: number;
    sanitizedKeys: string[];
    cwd: string | null;
    cols: number;
    rows: number;
  }) => void) =>
    (window.crab as any).onTermColdRestore(listener) as () => void,
  coldRestoreConfirm: (sessionId: string, env: Record<string, string>) =>
    (window.crab as any).termColdRestoreConfirm(sessionId, env) as Promise<void>,
  coldRestoreDiscard: (sessionId: string) =>
    (window.crab as any).termColdRestoreDiscard(sessionId) as Promise<void>,
  saveState: (state: SaveStateRequest) =>
    (window.crab as any).termSaveState(state) as Promise<{ success: boolean; error?: string }>,
  restoreState: (persistenceKey: string) =>
    (window.crab as any).termRestoreState(persistenceKey) as Promise<{ layout: unknown | null; scrollbackData: Record<string, string> }>,
  openUrl: (url: string) =>
    (window.crab as any).openUrl(url) as Promise<void>,
};

// --- Helpers ---

function loadProfiles(): TermProfile[] {
  try {
    const raw = localStorage.getItem(LS_PROFILES_KEY);
    return raw ? JSON.parse(raw) : [];
  } catch { return []; }
}

function saveProfiles(profiles: TermProfile[]) {
  localStorage.setItem(LS_PROFILES_KEY, JSON.stringify(profiles));
}

function loadHistory(): string[] {
  try {
    const raw = localStorage.getItem(LS_HISTORY_KEY);
    return raw ? JSON.parse(raw) : [];
  } catch { return []; }
}

function saveHistory(history: string[]) {
  localStorage.setItem(LS_HISTORY_KEY, JSON.stringify(history.slice(0, MAX_HISTORY)));
}

const LS_SCROLLBACK_KEY = "crab.termScrollback";
const LS_PASTE_GUARD_KEY = "crab.termPasteGuard";
const LS_SCROLLBAR_MODE_KEY = "crab.termScrollbar";
const LS_BREADCRUMB_KEY = "crab.termBreadcrumb";
const LS_WORKSPACE_STATE_PREFIX = "crab.termWorkspaceState:";

function workspaceStateKey(persistenceKey: string): string {
  return `${LS_WORKSPACE_STATE_PREFIX}${persistenceKey}`;
}

function loadWorkspaceState(persistenceKey: string): TerminalWorkspaceStateV1 | null {
  try {
    const raw = localStorage.getItem(workspaceStateKey(persistenceKey));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<TerminalWorkspaceStateV1>;
    if (parsed.version !== 1) return null;
    return {
      version: 1,
      openSessionIds: Array.isArray(parsed.openSessionIds) ? parsed.openSessionIds.filter((id): id is string => typeof id === "string") : [],
      activeSessionId: typeof parsed.activeSessionId === "string" ? parsed.activeSessionId : null,
      pinnedSessionIds: Array.isArray(parsed.pinnedSessionIds) ? parsed.pinnedSessionIds.filter((id): id is string => typeof id === "string") : [],
      hiddenSessionIds: Array.isArray(parsed.hiddenSessionIds) ? parsed.hiddenSessionIds.filter((id): id is string => typeof id === "string") : [],
      sessionSidebarSide: parsed.sessionSidebarSide === "right" ? "right" : "left",
      titles: parsed.titles && typeof parsed.titles === "object" && !Array.isArray(parsed.titles) ? parsed.titles as Record<string, string> : {},
      updatedAt: typeof parsed.updatedAt === "number" ? parsed.updatedAt : 0,
    };
  } catch {
    return null;
  }
}

function saveWorkspaceState(persistenceKey: string, state: TerminalWorkspaceStateV1): void {
  try {
    localStorage.setItem(workspaceStateKey(persistenceKey), JSON.stringify(state));
  } catch {
    // Best effort; terminal sessions remain daemon-backed even if layout state cannot persist.
  }
}

/** Scrollbar display mode for terminal viewports. */
type ScrollbarMode = "native" | "overlay" | "hidden";

const SCROLLBAR_MODES: { id: ScrollbarMode; label: string; description: string }[] = [
  { id: "native", label: "Native", description: "OS-styled scrollbar matching app design" },
  { id: "overlay", label: "Overlay", description: "Thin semi-transparent scrollbar" },
  { id: "hidden", label: "Hidden", description: "No visible scrollbar" },
];

/** Load scrollbar mode from localStorage. */
function loadScrollbarMode(): ScrollbarMode {
  try {
    const raw = localStorage.getItem(LS_SCROLLBAR_MODE_KEY);
    if (raw === "native" || raw === "overlay" || raw === "hidden") return raw;
  } catch {
    // Fall through to default
  }
  return "overlay";
}

/** Save scrollbar mode to localStorage. */
function saveScrollbarMode(mode: ScrollbarMode) {
  localStorage.setItem(LS_SCROLLBAR_MODE_KEY, mode);
}

/** Load and validate scrollback config from localStorage. */
function loadScrollbackConfig(): ScrollbackConfig {
  try {
    const raw = localStorage.getItem(LS_SCROLLBACK_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      const { normalized } = validateScrollbackConfig(parsed);
      return normalized;
    }
  } catch {
    // Fall through to defaults
  }
  const { normalized } = validateScrollbackConfig({});
  return normalized;
}

/** Load paste guard config from localStorage. */
function loadPasteGuardConfig(): PasteGuardConfig {
  try {
    const raw = localStorage.getItem(LS_PASTE_GUARD_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      return {
        enabled: typeof parsed.enabled === "boolean" ? parsed.enabled : DEFAULT_PASTE_GUARD_CONFIG.enabled,
        multiLineThreshold: typeof parsed.multiLineThreshold === "number" && parsed.multiLineThreshold >= 1
          ? parsed.multiLineThreshold
          : DEFAULT_PASTE_GUARD_CONFIG.multiLineThreshold,
        dangerousPatterns: DEFAULT_PASTE_GUARD_CONFIG.dangerousPatterns,
      };
    }
  } catch {
    // Fall through to defaults
  }
  return { ...DEFAULT_PASTE_GUARD_CONFIG };
}

/** Save paste guard config to localStorage. */
function savePasteGuardConfig(config: PasteGuardConfig) {
  localStorage.setItem(LS_PASTE_GUARD_KEY, JSON.stringify({
    enabled: config.enabled,
    multiLineThreshold: config.multiLineThreshold,
  }));
}

// --- Main Component ---

/**
 * Integrated terminal panel with advanced features:
 * - Multi-tab, split panes (horizontal + vertical)
 * - Scroll lock + "scroll to bottom" indicator
 * - Broadcast input (type once, send to all)
 * - Terminal profiles (save/load configurations)
 * - File path link detection (click to open in editor)
 * - Drag-and-drop files (paste path)
 * - Environment variable injection (CRAB_*)
 * - Quick commands palette (recent commands)
 * - Export output (text / HTML)
 * - Copy all output, rename tabs, search, context menu
 */
export function TerminalView({ repo, persistenceKey, onSendErrorToAgent, visible = true, defaultSessionType, sshRequest, onSshRequestHandled, startupRequest, onStartupRequestHandled }: Props) {
  const initialLeaf = createLeaf();
  const [splitTree, setSplitTree] = useState<SplitNode>(initialLeaf);
  const [activePaneId, setActivePaneId] = useState(initialLeaf.id);
  const [searchVisible, setSearchVisible] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchCaseSensitive, setSearchCaseSensitive] = useState(false);
  const [searchRegex, setSearchRegex] = useState(false);
  const [availableShells, setAvailableShells] = useState<string[]>([]);
  const [shellMenuOpen, setShellMenuOpen] = useState(false);
  const [contextMenu, setContextMenu] = useState<{
    x: number; y: number; termId: string; paneId: string;
  } | null>(null);
  const [renamingTab, setRenamingTab] = useState<string | null>(null);
  const [broadcastMode, setBroadcastMode] = useState(false);
  const [scrollLocked, setScrollLocked] = useState<Map<string, boolean>>(new Map());
  const [showScrollBtn, setShowScrollBtn] = useState<Map<string, boolean>>(new Map());
  const [profiles, setProfiles] = useState<TermProfile[]>(loadProfiles);
  const [profileMenuOpen, setProfileMenuOpen] = useState(false);
  const [quickCmdOpen, setQuickCmdOpen] = useState(false);
  const [cmdHistory, setCmdHistory] = useState<string[]>(loadHistory);
  const [cmdFilter, setCmdFilter] = useState("");
  const [exportMenuOpen, setExportMenuOpen] = useState(false);
  const [schemeId, setSchemeId] = useState<string>(getStoredSchemeId);
  const [schemeMenuOpen, setSchemeMenuOpen] = useState(false);
  const [scrollbackMenuOpen, setScrollbackMenuOpen] = useState(false);
  const [pasteGuardConfig, setPasteGuardConfig] = useState<PasteGuardConfig>(loadPasteGuardConfig);
  const [pasteGuardMenuOpen, setPasteGuardMenuOpen] = useState(false);
  const [scrollbarMode, setScrollbarMode] = useState<ScrollbarMode>(loadScrollbarMode);
  const [scrollbarMenuOpen, setScrollbarMenuOpen] = useState(false);
  const [scrollbarAutohide, setScrollbarAutohide] = useState(false);
  const [presetMenuOpen, setPresetMenuOpen] = useState(false);
  const [breadcrumbDefault, setBreadcrumbDefault] = useState<boolean>(() => {
    try {
      const raw = localStorage.getItem(LS_BREADCRUMB_KEY);
      return raw === null ? true : raw === "true";
    } catch { return true; }
  });
  /** Per-tab breadcrumb enabled state. Missing entries inherit breadcrumbDefault. */
  const [breadcrumbPerTab, setBreadcrumbPerTab] = useState<Map<string, boolean>>(new Map());
  /**
   * MRU (Most Recently Used) tab history per pane. Maps paneId → ordered
   * list of tabIds (most-recently-activated first). Used to select the
   * next active tab when the current one is closed — iTerm2-style behavior.
   */
  const tabMruRef = useRef<Map<string, string[]>>(new Map());
  // UX Enhancement state (Tasks 26.1-26.6, 27.1-27.5)
  const [fontMenuOpen, setFontMenuOpen] = useState(false);
  const [fontConfig, setFontConfig] = useState<FontConfig>(loadFontConfig);
  const [bookmarksOpen, setBookmarksOpen] = useState(false);
  const [activeTimeFilter, setActiveTimeFilter] = useState<number | null>(null);
  const [activeSemanticFilter, setActiveSemanticFilter] = useState<SemanticFilter>("all");
  const bookmarkManagersRef = useRef<Map<string, BookmarkManager>>(new Map());
  const [pasteDialogState, setPasteDialogState] = useState<{
    analysis: PasteAnalysis;
    content: string;
    termId: string;
  } | null>(null);
  /**
   * Exit codes for terminals that ended non-zero. Surfaces the "Send
   * error to agent" pill, only when the parent wired up the callback.
   */
  const [failedExits, setFailedExits] = useState<Map<string, { exitCode: number; at: number }>>(
    new Map(),
  );

  /**
   * Tab badges: colored dot on unfocused terminal tabs indicating
   * command completion. Green for exit 0, red for non-zero.
   * Cleared when the user activates the tab.
   */
  const [tabBadges, setTabBadges] = useState<Map<string, "success" | "error">>(new Map());
  const restoredWorkspaceStateRef = useRef<TerminalWorkspaceStateV1 | null>(loadWorkspaceState(persistenceKey));
  const [daemonSessions, setDaemonSessions] = useState<DaemonSessionSummary[]>([]);
  const [sessionsManagerOpen, setSessionsManagerOpen] = useState(false);
  const [sessionSidebarSide, setSessionSidebarSide] = useState<TerminalSessionSidebarSide>(
    () => restoredWorkspaceStateRef.current?.sessionSidebarSide ?? "left",
  );
  const [sessionSearch, setSessionSearch] = useState("");
  const [pinnedDaemonSessionIds, setPinnedDaemonSessionIds] = useState<Set<string>>(
    () => new Set(restoredWorkspaceStateRef.current?.pinnedSessionIds ?? []),
  );
  const [hiddenDaemonSessionIds, setHiddenDaemonSessionIds] = useState<Set<string>>(
    () => new Set(restoredWorkspaceStateRef.current?.hiddenSessionIds ?? []),
  );
  const hiddenDaemonSessionIdsRef = useRef(hiddenDaemonSessionIds);
  useEffect(() => { hiddenDaemonSessionIdsRef.current = hiddenDaemonSessionIds; }, [hiddenDaemonSessionIds]);

  /**
   * Pending cold-restore requests — one per session ID. Each entry
   * represents a tab that was cold-restored from a checkpoint and is
   * waiting for the user to confirm or discard the restore.
   */
  const [coldRestoreRequests, setColdRestoreRequests] = useState<Map<string, ColdRestoreRequest>>(new Map());

  /** Notification configuration (persisted in localStorage). */
  const [notificationConfig] = useState<NotificationConfig>(() => {
    try {
      const raw = localStorage.getItem("crab.termNotifications");
      if (raw) {
        const parsed = JSON.parse(raw);
        return { ...DEFAULT_NOTIFICATION_CONFIG, ...parsed };
      }
    } catch { /* fall through */ }
    return { ...DEFAULT_NOTIFICATION_CONFIG };
  });

  /** Whether the PTY daemon is ready and circuit breaker state — from PtyModeContext. */
  const { daemonReady, breakerOpen } = usePtyMode();
  const containerRefs = useRef<Map<string, HTMLDivElement>>(new Map());
  const terminalsRef = useRef<Map<string, TermInstance>>(new Map());
  const pendingRestoreSnapshotsRef = useRef<PendingRestoreSnapshotMap>(new Map());
  const pendingDataRef = useRef<Map<string, string[]>>(new Map());
  const shellIntegrationRef = useRef<Map<string, { state: ShellIntegrationState; jumpToPreviousPrompt: () => void; jumpToNextPrompt: () => void; selectCommandOutput: (markIndex: number) => string | null; dispose: () => void }>>(new Map());
  const pasteGuardConfigRef = useRef(pasteGuardConfig);
  useEffect(() => { pasteGuardConfigRef.current = pasteGuardConfig; }, [pasteGuardConfig]);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const cmdInputRef = useRef<HTMLInputElement>(null);
  const theme = useEffectiveTheme();
  const tabCounter = useRef(0);

  // Track previous visibility state to detect false → true transitions.
  const prevVisibleRef = useRef(visible);
  useEffect(() => {
    const wasHidden = !prevVisibleRef.current;
    prevVisibleRef.current = visible;

    if (wasHidden && visible) {
      // Container may have resized while hidden — refit all xterm instances.
      requestAnimationFrame(() => {
        for (const [id, inst] of terminalsRef.current.entries()) {
          const wrapper = inst.xterm.element?.parentElement;
          if (!wrapper || wrapper.offsetWidth <= 0 || wrapper.offsetHeight <= 0) continue;
          inst.fit.fit();
          replayQueuedRestoreSnapshot(pendingRestoreSnapshotsRef.current, id, inst.xterm);
          const pending = pendingDataRef.current.get(id);
          if (pending && pending.length > 0) {
            inst.xterm.write(pending.join(""));
            pendingDataRef.current.delete(id);
          }
        }
      });
    }

    // Pause/resume cursor blink based on visibility to save CPU.
    // A blinking cursor in a hidden terminal is pure waste.
    for (const inst of terminalsRef.current.values()) {
      inst.xterm.options.cursorBlink = visible;
    }
  }, [visible]);

  // Sync with macOS "Show scroll bars" system preference.
  // The closest web API is detecting overlay scrollbar behavior via
  // a CSS media query heuristic. We listen for scrollbar-related
  // preference changes using a thin scrollbar width test.
  useEffect(() => {
    // Detect if the OS prefers overlay/auto-hide scrollbars.
    // On macOS with "When scrolling" or "Automatically based on mouse",
    // scrollbars are overlay-style. We detect this by checking if a
    // scrollbar-gutter test element has zero scrollbar width.
    function detectAutohide(): boolean {
      const outer = document.createElement("div");
      outer.style.cssText = "width:100px;height:100px;overflow:scroll;position:absolute;top:-9999px;";
      document.body.appendChild(outer);
      const hasOverlay = outer.offsetWidth === outer.clientWidth;
      document.body.removeChild(outer);
      return hasOverlay;
    }
    setScrollbarAutohide(detectAutohide());

    // Re-check when the window regains focus (user may have changed
    // System Preferences while the app was in the background).
    function onFocus() { setScrollbarAutohide(detectAutohide()); }
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, []);

  // Load available shells on mount.
  useEffect(() => {
    void ptyIpc.listShells().then(setAvailableShells).catch(() => {
      setAvailableShells(["/bin/zsh", "/bin/bash", "/bin/sh"]);
    });
  }, []);

  // Hint the terminal pool about the repo's working directory.
  useEffect(() => {
    if (repo.path) {
      void ptyIpc.poolHint(repo.path);
    }
  }, [repo.path]);

  const refreshDaemonSessions = useCallback(async () => {
    if (!repo.path || typeof (window.crab as any).termListDaemonSessions !== "function") {
      setDaemonSessions([]);
      return [];
    }
    try {
      const sessions = await ptyIpc.listDaemonSessions(repo.path);
      setDaemonSessions(sessions);
      return sessions;
    } catch {
      setDaemonSessions([]);
      return [];
    }
  }, [repo.path]);

  // ─── Session persistence: save on app close ─────────────────────
  //
  // Listen for the main process "save-request" signal (fired on
  // before-quit) and serialize the current layout + scrollback.
  const splitTreeRef = useRef(splitTree);
  useEffect(() => { splitTreeRef.current = splitTree; }, [splitTree]);
  const activePaneIdRef = useRef(activePaneId);
  useEffect(() => { activePaneIdRef.current = activePaneId; }, [activePaneId]);

  useEffect(() => {
    const off = ptyIpc.onSaveRequest(() => {
      const currentTree = splitTreeRef.current;
      const allLeaves = getAllLeaves(currentTree);
      const allTabs = allLeaves.flatMap((l) => l.tabs);
      if (allTabs.length === 0) return;

      // Build the view state for serialization.
      const activeLeaf = findLeaf(currentTree, activePaneIdRef.current) ?? allLeaves[0];
      const activeTabIdx = activeLeaf
        ? allTabs.findIndex((t) => t.id === activeLeaf.activeTab)
        : 0;

      const viewState: TerminalViewState = {
        tabs: allTabs.map((tab) => {
          const si = shellIntegrationRef.current.get(tab.id);
          return {
            id: tab.id,
            title: tab.title,
            shell: tab.shell,
            cwd: si?.state.cwd ?? repo.path ?? "",
          };
        }),
        splits: [],
        activeTabIndex: Math.max(0, activeTabIdx),
      };

      const layout = serializeLayout(viewState);

      // Collect scrollback data from each terminal's SerializeAddon.
      const scrollbackData: Record<string, string> = {};
      for (const tab of allTabs) {
        const inst = terminalsRef.current.get(tab.id);
        if (inst) {
          try {
            scrollbackData[tab.id] = inst.serialize.serialize();
          } catch {
            // Non-fatal — skip terminals that can't be serialized.
          }
        }
      }

      void ptyIpc.saveState({ persistenceKey, layout, scrollbackData });
    });
    return off;
  }, [repo.path, persistenceKey]);

  // ─── Session persistence: restore on app start ──────────────────
  //
  // On mount, first try to reattach to existing daemon sessions (they
  // survive app restarts). If no daemon sessions exist, fall back to
  // restoring from saved state (spawns fresh terminals).
  const restoredRef = useRef(false);
  const restoreMountedRef = useRef(false);
  const [initialRestoreComplete, setInitialRestoreComplete] = useState(false);
  useEffect(() => {
    restoreMountedRef.current = true;
    if (restoredRef.current) {
      return () => { restoreMountedRef.current = false; };
    }
    restoredRef.current = true;

    void (async () => {
      try {
        // Step 1: Discover daemon sessions for this repo without turning
        // every persistent PTY into a visible tab.
        try {
          const crab = window.crab as any;
          if (typeof crab?.termListDaemonSessions === "function" && typeof crab?.termAttachDaemonSession === "function") {
            const repoPath = repo.path || "";
            const sessions = await ptyIpc.listDaemonSessions(repoPath);
            if (!restoreMountedRef.current) return;
            setDaemonSessions(sessions);

            if (sessions.length > 0) {
              const savedWorkspaceState = restoredWorkspaceStateRef.current;
              const sessionById = new Map(sessions.map((session) => [session.id, session]));
              const openIds = savedWorkspaceState
                ? [...savedWorkspaceState.openSessionIds, ...savedWorkspaceState.pinnedSessionIds]
                : [sessions[0].id];
              const uniqueOpenIds = [...new Set(openIds)];
              const sessionsToAttach = uniqueOpenIds
                .map((id) => sessionById.get(id))
                .filter((session): session is DaemonSessionSummary => Boolean(session));
              const reattachedTabs: TermTab[] = [];

              for (const session of sessionsToAttach) {
                const attached = await ptyIpc.attachDaemonSession(session.daemonSessionId, repoPath);
                if (!restoreMountedRef.current) return;
                tabCounter.current++;
                const tab: TermTab = {
                  id: attached.id,
                  title: savedWorkspaceState?.titles?.[attached.id] ?? daemonSessionTitle(attached.summary ?? session, tabCounter.current),
                  shell: "",
                  resolvedSessionType: "daemon",
                };
                reattachedTabs.push(tab);

                // Register with the global terminal registry.
                registerTerminal({
                  id: attached.id,
                  label: `${repo.label ? repo.label + " · " : ""}${tab.title}`,
                  cwd: attached.cwd || session.cwd || session.initialCwd || repo.path || "",
                });

                queueRestoreSnapshot(pendingRestoreSnapshotsRef.current, attached.id, attached.snapshot);
              }

              if (reattachedTabs.length > 0) {
                const activeId = savedWorkspaceState?.activeSessionId && reattachedTabs.some((tab) => tab.id === savedWorkspaceState.activeSessionId)
                  ? savedWorkspaceState.activeSessionId
                  : reattachedTabs[0].id;
                const restoredLeaf: LeafNode = {
                  type: "leaf",
                  id: "restored-pane",
                  tabs: reattachedTabs as LeafTab[],
                  activeTab: activeId,
                };
                setSplitTree(restoredLeaf);
                setActivePaneId(restoredLeaf.id);

                return; // Successfully attached visible daemon tabs — skip saved state restore.
              }
            }
          }
        } catch {
          // Reattach failed — fall through to saved state restore.
        }

        // Step 2: Fall back to restoring from saved state (spawns fresh terminals).
        try {
          const { layout: rawLayout } = await ptyIpc.restoreState(persistenceKey);
          const layout = deserializeLayout(rawLayout);
          if (!restoreMountedRef.current) return;
          if (!layout || layout.tabs.length === 0) return;

          // This layout belongs only to this terminal owner. Live daemon
          // selection is separate; fresh shell IDs cannot match those old IDs.
          const restoredTabs: TermTab[] = [];
          for (const session of layout.tabs) {
            try {
              const cwd = session.cwd || repo.path || "";
              const { id } = await ptyIpc.spawn(
                cwd,
                session.shell || undefined,
                session.env,
                session.startupCommand,
                undefined,
                defaultSessionType,
              );
              if (!restoreMountedRef.current) return;
              tabCounter.current++;
              const tab: TermTab = {
                id,
                title: session.title || `shell ${tabCounter.current}`,
                shell: session.shell,
              };
              restoredTabs.push(tab);

              // Register with the global terminal registry.
              registerTerminal({
                id,
                label: `${repo.label ? repo.label + " · " : ""}${tab.title}`,
                cwd,
              });
            } catch {
              // Skip sessions that fail to spawn.
            }
          }

          if (restoredTabs.length > 0) {
            const activeIdx = Math.min(layout.activeTabIndex, restoredTabs.length - 1);
            const restoredLeaf: LeafNode = {
              type: "leaf",
              id: "restored-pane",
              tabs: restoredTabs as LeafTab[],
              activeTab: restoredTabs[activeIdx]?.id ?? restoredTabs[0].id,
            };
            setSplitTree(restoredLeaf);
            setActivePaneId(restoredLeaf.id);

            // Session restored — fresh shells are spawned so there's no
            // stale scrollback to replay. Skip the indicator entirely for
            // a clean terminal experience.
          }
        } catch {
          // Restore failed — fall through to default auto-spawn behavior.
        }
      } finally {
        if (restoreMountedRef.current) {
          setInitialRestoreComplete(true);
        }
      }
    })();
    return () => { restoreMountedRef.current = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Persist the visible terminal tab layout separately from daemon lifetime.
  // This lets daemon PTYs survive in the background without being restored as
  // visible tabs unless the user had explicitly opened or pinned them.
  useEffect(() => {
    if (!initialRestoreComplete) return;
    const allLeaves = getAllLeaves(splitTree);
    const allTabs = allLeaves.flatMap((leaf) => leaf.tabs as TermTab[]);
    const activeLeaf = findLeaf(splitTree, activePaneId) ?? allLeaves[0];
    const titles: Record<string, string> = {};
    for (const tab of allTabs) {
      titles[tab.id] = tab.title;
    }
    const state: TerminalWorkspaceStateV1 = {
      version: 1,
      openSessionIds: allTabs.map((tab) => tab.id),
      activeSessionId: activeLeaf?.activeTab ?? null,
      pinnedSessionIds: [...pinnedDaemonSessionIds],
      hiddenSessionIds: [...hiddenDaemonSessionIds],
      sessionSidebarSide,
      titles,
      updatedAt: Date.now(),
    };
    saveWorkspaceState(persistenceKey, state);
  }, [splitTree, activePaneId, pinnedDaemonSessionIds, hiddenDaemonSessionIds, sessionSidebarSide, initialRestoreComplete, persistenceKey]);

  useEffect(() => {
    if (!sessionsManagerOpen) return;
    void refreshDaemonSessions();
    const interval = window.setInterval(() => {
      void refreshDaemonSessions();
    }, 5000);
    return () => window.clearInterval(interval);
  }, [sessionsManagerOpen, refreshDaemonSessions]);

  // Auto-spawn a terminal when all tabs in the active pane are closed.
  const spawningRef = useRef(false);
  const suppressNextAutoSpawnRef = useRef(false);
  useEffect(() => {
    if (!initialRestoreComplete || !visible || startupRequest || sshRequest) return;
    const activeLeaf = findLeaf(splitTree, activePaneId);
    if (activeLeaf && activeLeaf.tabs.length === 0 && !spawningRef.current) {
      if (suppressNextAutoSpawnRef.current) {
        suppressNextAutoSpawnRef.current = false;
        return;
      }
      spawningRef.current = true;
      void spawnTab(activeLeaf.id).finally(() => {
        spawningRef.current = false;
      });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [splitTree, activePaneId, initialRestoreComplete, visible, startupRequest, sshRequest]);

  // Restore owns the initial layout; append requests only after it completes
  // so a late restore cannot replace the new tab or race normal auto-spawn.
  useEffect(() => {
    if (!startupRequest || !visible || !initialRestoreComplete) return;
    const leaf = findLeaf(splitTreeRef.current, activePaneId) ?? getAllLeaves(splitTreeRef.current)[0];
    if (!leaf) return;
    spawningRef.current = true;
    void spawnTab(leaf.id, undefined, {
      name: startupRequest.name,
      shell: "",
      cwd: startupRequest.cwd,
      env: startupRequest.env,
      startupCommand: startupRequest.command,
      agentMeta: startupRequest.agentMeta,
    }).finally(() => { spawningRef.current = false; });
    onStartupRequestHandled?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [startupRequest, visible, initialRestoreComplete]);

  // App queues this request until the active global terminal has mounted.
  // Hidden repo and task terminals must never consume another terminal's request.
  useEffect(() => {
    if (!sshRequest || !visible || !initialRestoreComplete) return;
    spawningRef.current = true;
    void spawnSshTab(sshRequest.profileId, sshRequest.hostname, sshRequest.cwd, sshRequest.colour).finally(() => { spawningRef.current = false; });
    onSshRequestHandled?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sshRequest, visible, initialRestoreComplete]);

  // Cmd+Shift+T: open a remote terminal on the most recently used SSH connection.
  useEffect(() => {
    if (!visible) return;
    function handleQuickSshTerminal(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key === "T") {
        e.preventDefault();
        void (async () => {
          try {
            const profiles = await ((window.crab as any).ssh.listProfiles() as Promise<Array<{ id: string; label: string; host: string; colour?: string; lastConnectedAt?: number }>>);
            if (!profiles || profiles.length === 0) return;
            // Pick the most recently connected profile
            const sorted = [...profiles].sort((a, b) => (b.lastConnectedAt ?? 0) - (a.lastConnectedAt ?? 0));
            const profile = sorted[0];
            void spawnSshTab(profile.id, profile.host, undefined, profile.colour);
          } catch {
            // No SSH profiles available — ignore
          }
        })();
      }
    }
    window.addEventListener("keydown", handleQuickSshTerminal);
    return () => window.removeEventListener("keydown", handleQuickSshTerminal);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [splitTree, activePaneId, visible]);

  const getActivePane = useCallback(() => {
    return findLeaf(splitTree, activePaneId) ?? getAllLeaves(splitTree)[0] ?? null;
  }, [splitTree, activePaneId]);

  /** Get all active terminal IDs across all panes. */
  function getAllTermIds(): string[] {
    return getAllTabIds(splitTree);
  }

  async function spawnTab(paneId: string, shell?: string, profile?: TermProfile, sessionTypeOverride?: "daemon" | "in-process") {
    // Inherit the active tab's tracked cwd (from shell integration) when opening a new tab.
    const activeLeaf = findLeaf(splitTree, paneId) ?? findLeaf(splitTree, activePaneId);
    const activeTabId = activeLeaf?.activeTab;
    const trackedCwd = activeTabId ? shellIntegrationRef.current.get(activeTabId)?.state.cwd : null;
    const cwd = profile?.cwd || trackedCwd || repo.path || process.env.HOME || "/";
    const shellPath = profile?.shell || shell;
    const extraEnv: Record<string, string> = {
      CRAB_REPO_ID: repo.repo_id || "",
      CRAB_REPO_URL: repo.url || "",
      CRAB_REPO_LABEL: repo.label || "",
      ...(profile?.env || {}),
    };
    try {
      const result = await ptyIpc.spawn(cwd, shellPath, extraEnv, profile?.startupCommand, profile?.agentMeta, sessionTypeOverride ?? defaultSessionType);
      const { id, resolvedSessionType } = result;
      tabCounter.current++;
      const shellName = profile?.name || (shellPath ? shellPath.split("/").pop() ?? "shell" : "zsh");
      const title = profile?.name || `${shellName} ${tabCounter.current}`;
      const tab: TermTab = { id, title, shell: shellPath ?? "", startupCommand: profile?.startupCommand, resolvedSessionType };
      // Mark this terminal as pending startup so the first data clears the loading indicator.
      if (profile?.startupCommand) {
        startupPendingRef.current.add(id);
      }
      // Register with the global terminal registry so the @ context
      // picker in the orchestrator composer can reference this session.
      registerTerminal({
        id,
        label: `${repo.label ? repo.label + " · " : ""}${title}`,
        cwd,
      });
      setSplitTree((prev) =>
        updateLeaf(prev, paneId, (leaf) => ({
          ...leaf,
          tabs: [...leaf.tabs, tab as LeafTab],
          activeTab: id,
        })),
      );
      // Track in MRU history.
      const mru = tabMruRef.current.get(paneId) ?? [];
      tabMruRef.current.set(paneId, [id, ...mru.filter((x) => x !== id)]);
      setActivePaneId(paneId);
    } catch (e) {
      console.error("Failed to spawn terminal:", e);
    }
  }

  /**
   * Spawn an SSH remote terminal tab. Opens a shell channel on the
   * given SSH profile and wires up bidirectional data flow via IPC.
   */
  async function spawnSshTab(profileId: string, hostname: string, cwd?: string, colour?: string) {
    const activeLeaf = findLeaf(splitTree, activePaneId) ?? getAllLeaves(splitTree)[0];
    if (!activeLeaf) return;

    try {
      const { terminalId } = await sshTermIpc.openTerminal(profileId, 80, 24, cwd);
      tabCounter.current++;
      const title = `ssh ${tabCounter.current}`;
      const tab: TermTab = {
        id: terminalId,
        title,
        shell: "ssh",
        sshRemote: { profileId, hostname, colour },
      };
      setSplitTree((prev) =>
        updateLeaf(prev, activeLeaf.id, (leaf) => ({
          ...leaf,
          tabs: [...leaf.tabs, tab as LeafTab],
          activeTab: terminalId,
        })),
      );
      // Track in MRU history.
      const mru = tabMruRef.current.get(activeLeaf.id) ?? [];
      tabMruRef.current.set(activeLeaf.id, [terminalId, ...mru.filter((x) => x !== terminalId)]);
      setActivePaneId(activeLeaf.id);
    } catch (e) {
      console.error("Failed to spawn SSH terminal:", e);
    }
  }

  async function attachDaemonSession(session: DaemonSessionSummary, paneId = activePaneId, splitDirection?: SplitDirection) {
    const existingLeaf = findLeafByTab(splitTreeRef.current, session.id);
    if (existingLeaf) {
      setActivePaneId(existingLeaf.id);
      setSplitTree((prev) => updateLeaf(prev, existingLeaf.id, (leaf) => ({ ...leaf, activeTab: session.id })));
      setSessionsManagerOpen(false);
      return;
    }

    try {
      const attached = await ptyIpc.attachDaemonSession(session.daemonSessionId, repo.path || "");
      const summary = attached.summary ?? session;
      tabCounter.current++;
      const title = restoredWorkspaceStateRef.current?.titles?.[attached.id] ?? daemonSessionTitle(summary, tabCounter.current);
      const tab: TermTab = {
        id: attached.id,
        title,
        shell: "",
        resolvedSessionType: "daemon",
      };

      registerTerminal({
        id: attached.id,
        label: `${repo.label ? repo.label + " · " : ""}${title}`,
        cwd: attached.cwd || summary.cwd || summary.initialCwd || repo.path || "",
      });
      queueRestoreSnapshot(pendingRestoreSnapshotsRef.current, attached.id, attached.snapshot);

      let mruPaneId = paneId;
      if (splitDirection) {
        const newLeaf = createLeaf();
        const populatedLeaf: LeafNode = {
          ...newLeaf,
          tabs: [tab as LeafTab],
          activeTab: attached.id,
        };
        setSplitTree((prev) => splitLeafInPlace(prev, paneId, splitDirection, populatedLeaf));
        setActivePaneId(populatedLeaf.id);
        mruPaneId = populatedLeaf.id;
      } else {
        setSplitTree((prev) =>
          updateLeaf(prev, paneId, (leaf) => ({
            ...leaf,
            tabs: [...leaf.tabs, tab as LeafTab],
            activeTab: attached.id,
          })),
        );
        setActivePaneId(paneId);
      }

      const mru = tabMruRef.current.get(mruPaneId) ?? [];
      tabMruRef.current.set(mruPaneId, [attached.id, ...mru.filter((id) => id !== attached.id)]);
      setHiddenDaemonSessionIds((prev) => {
        const next = new Set(prev);
        next.delete(attached.id);
        return next;
      });
      setDaemonSessions((prev) => {
        const next = prev.filter((item) => item.id !== attached.id);
        next.push(summary);
        return next.sort((a, b) => (b.lastDataAt || b.createdAt) - (a.lastDataAt || a.createdAt));
      });
      setSessionsManagerOpen(false);
    } catch (e) {
      publishToast({
        kind: "error",
        title: "Cannot attach terminal session",
        body: e instanceof Error ? e.message : String(e),
        duration: 5000,
      });
    }
  }

  function focusOpenTab(tabId: string) {
    const leaf = findLeafByTab(splitTreeRef.current, tabId);
    if (!leaf) return;
    setActivePaneId(leaf.id);
    setSplitTree((prev) => updateLeaf(prev, leaf.id, (l) => ({ ...l, activeTab: tabId })));
    setSessionsManagerOpen(false);
  }

  function togglePinnedDaemonSession(sessionId: string) {
    setPinnedDaemonSessionIds((prev) => {
      const next = new Set(prev);
      if (next.has(sessionId)) next.delete(sessionId);
      else next.add(sessionId);
      return next;
    });
  }

  function killDaemonSessionFromManager(session: DaemonSessionSummary) {
    const leaf = findLeafByTab(splitTreeRef.current, session.id);
    if (leaf) {
      killTabImmediate(leaf.id, session.id);
    } else {
      void ptyIpc.killDaemonSession(session.daemonSessionId, repo.path || "");
      setPinnedDaemonSessionIds((prev) => {
        const next = new Set(prev);
        next.delete(session.id);
        return next;
      });
      setHiddenDaemonSessionIds((prev) => {
        const next = new Set(prev);
        next.delete(session.id);
        return next;
      });
      setDaemonSessions((prev) => prev.filter((item) => item.id !== session.id));
    }
  }

  function disposeTabView(tabId: string) {
    pendingDataRef.current.delete(tabId);
    pendingRestoreSnapshotsRef.current.delete(tabId);
    // Dispose shell integration for this terminal.
    const si = shellIntegrationRef.current.get(tabId);
    if (si) { si.dispose(); shellIntegrationRef.current.delete(tabId); }
    // Clean up bookmark state for this tab.
    bookmarkManagersRef.current.delete(tabId);
    // Clean up per-tab breadcrumb state.
    setBreadcrumbPerTab((prev) => { const next = new Map(prev); next.delete(tabId); return next; });
    const term = terminalsRef.current.get(tabId);
    if (term) {
      if (term.memoryCheckInterval) clearInterval(term.memoryCheckInterval);
      const wrapper = term.xterm.element?.parentElement;
      if (wrapper) wrapper.remove();
      term.xterm.dispose();
      terminalsRef.current.delete(tabId);
    }
    removeTerminal(tabId);
  }

  function removeTabFromLayout(paneId: string, tabId: string) {
    // Remove the closed tab from MRU history and determine the next active tab.
    const mru = tabMruRef.current.get(paneId) ?? [];
    const updatedMru = mru.filter((id) => id !== tabId);
    tabMruRef.current.set(paneId, updatedMru);
    setSplitTree((prev) =>
      updateLeaf(prev, paneId, (leaf) => {
        const remaining = leaf.tabs.filter((t) => t.id !== tabId);
        if (leaf.activeTab !== tabId) {
          return { ...leaf, tabs: remaining };
        }
        // Pick next active tab using MRU history (iTerm2-style):
        // 1. Most recently used tab that's still open
        // 2. Fallback: right neighbor, then left neighbor in the tab bar
        const mruNext = updatedMru.find((id) => remaining.some((t) => t.id === id));
        if (mruNext) {
          return { ...leaf, tabs: remaining, activeTab: mruNext };
        }
        // Positional fallback: prefer the tab that was to the right of the
        // closed tab (same index), otherwise the one to the left.
        const closedIdx = leaf.tabs.findIndex((t) => t.id === tabId);
        const fallback = remaining[closedIdx] ?? remaining[closedIdx - 1] ?? remaining[0];
        return { ...leaf, tabs: remaining, activeTab: fallback?.id ?? null };
      }),
    );
  }

  /** Hide a daemon tab from the visible layout while leaving the PTY alive. */
  function hideTabImmediate(paneId: string, tabId: string) {
    void ptyIpc.detachDaemonSession(tabId);
    disposeTabView(tabId);
    setHiddenDaemonSessionIds((prev) => {
      const next = new Set(prev);
      next.add(tabId);
      return next;
    });
    suppressNextAutoSpawnRef.current = true;
    removeTabFromLayout(paneId, tabId);
  }

  /** Unconditionally kill a terminal tab (no confirmation). */
  function killTabImmediate(paneId: string, tabId: string) {
    void ptyIpc.kill(tabId);
    disposeTabView(tabId);
    setPinnedDaemonSessionIds((prev) => {
      const next = new Set(prev);
      next.delete(tabId);
      return next;
    });
    setHiddenDaemonSessionIds((prev) => {
      const next = new Set(prev);
      next.delete(tabId);
      return next;
    });
    setDaemonSessions((prev) => prev.filter((session) => session.id !== tabId));
    removeTabFromLayout(paneId, tabId);
  }

  function closeTabFromTabStrip(paneId: string, tabId: string) {
    const leaf = findLeaf(splitTreeRef.current, paneId);
    const tab = leaf?.tabs.find((t) => t.id === tabId) as TermTab | undefined;
    if (tab?.resolvedSessionType === "daemon") {
      hideTabImmediate(paneId, tabId);
      return;
    }
    killTab(paneId, tabId);
  }

  /**
   * Smart close: if the shell is at a prompt (no running command), close
   * immediately without confirmation. Otherwise show a confirmation dialog.
   */
  function killTab(paneId: string, tabId: string) {
    const si = shellIntegrationRef.current.get(tabId);
    const atPrompt = si?.state.atPrompt ?? true;
    if (atPrompt) {
      // No command running — close immediately.
      killTabImmediate(paneId, tabId);
    } else {
      // Command is running — ask for confirmation.
      const confirmed = window.confirm(
        "A command is still running in this terminal. Are you sure you want to close it?",
      );
      if (confirmed) {
        killTabImmediate(paneId, tabId);
      }
    }
  }

  function splitPane(direction?: SplitDirection, targetId?: string) {
    const dir = direction ?? "horizontal";
    const targetLeafId = targetId ?? activePaneId;
    const newLeaf = createLeaf();
    setSplitTree((prev) => {
      const newRoot = splitLeafInPlace(prev, targetLeafId, dir, newLeaf);
      return newRoot;
    });
    // Set the new leaf as active so the auto-spawn effect picks it up
    // and creates a terminal tab in it.
    setActivePaneId(newLeaf.id);
  }

  /**
   * Apply a preset layout — redistributes existing terminals into the
   * new grid layout. Existing terminals are preserved (not killed).
   * If there are more existing terminals than the preset has panes,
   * the operation is rejected with a toast explaining why.
   */
  function applyPresetLayout(cols: number, rows: number) {
    const requiredPanes = cols * rows;
    const allLeaves = getAllLeaves(splitTree);

    // Collect all existing tabs across all panes (one tab per pane for redistribution).
    // Each leaf may have multiple tabs — we count total terminals, not panes.
    const existingTabs: { tab: LeafTab; paneId: string }[] = [];
    for (const leaf of allLeaves) {
      for (const tab of leaf.tabs) {
        existingTabs.push({ tab, paneId: leaf.id });
      }
    }

    if (existingTabs.length > requiredPanes) {
      publishToast({
        kind: "error",
        title: "Cannot apply layout",
        body: `You have ${existingTabs.length} open terminal${existingTabs.length === 1 ? "" : "s"} but the ${cols}×${rows} layout only has ${requiredPanes} pane${requiredPanes === 1 ? "" : "s"}. Close some terminals first or choose a larger layout.`,
        duration: 6000,
      });
      return;
    }

    // Create the new layout structure (empty leaves).
    const { root, leafIds } = createPresetLayout(cols, rows);

    // Place existing tabs into the first N leaves of the new layout.
    let populatedRoot = root;
    for (let i = 0; i < existingTabs.length; i++) {
      const { tab } = existingTabs[i];
      const targetLeafId = leafIds[i];
      populatedRoot = updateLeaf(populatedRoot, targetLeafId, (leaf) => ({
        ...leaf,
        tabs: [tab as LeafTab],
        activeTab: tab.id,
      }));
    }

    setSplitTree(populatedRoot);
    setActivePaneId(leafIds[0]);

    // Spawn new terminals in the remaining empty panes.
    for (let i = existingTabs.length; i < leafIds.length; i++) {
      // Stagger spawns so state settles between each.
      const idx = i;
      setTimeout(() => void spawnTab(leafIds[idx]), (idx - existingTabs.length + 1) * 50);
    }
  }

  function removePane(paneId: string) {
    const leaf = findLeaf(splitTree, paneId);
    if (leaf) {
      for (const tab of leaf.tabs) {
        if ((tab as TermTab).resolvedSessionType === "daemon") {
          void ptyIpc.detachDaemonSession(tab.id);
          disposeTabView(tab.id);
          setHiddenDaemonSessionIds((prev) => {
            const next = new Set(prev);
            next.add(tab.id);
            return next;
          });
        } else {
          void ptyIpc.kill(tab.id);
          disposeTabView(tab.id);
        }
      }
    }
    // Clean up MRU history for this pane.
    tabMruRef.current.delete(paneId);
    setSplitTree((prev) => {
      const result = removeLeaf(prev, paneId);
      if (!result) {
        // Tree is now empty — create a fresh leaf
        const fresh = createLeaf();
        setActivePaneId(fresh.id);
        return fresh;
      }
      return result;
    });
    if (activePaneId === paneId) {
      const leaves = getAllLeaves(splitTree).filter((l) => l.id !== paneId);
      setActivePaneId(leaves[0]?.id ?? "");
    }
  }

  /** Copy the entire scrollback + visible buffer. */
  function copyAllOutput(termId?: string) {
    const id = termId ?? getActivePane()?.activeTab;
    if (!id) return;
    const t = terminalsRef.current.get(id);
    if (!t) return;
    t.xterm.selectAll();
    const text = t.xterm.getSelection();
    t.xterm.clearSelection();
    if (text) void navigator.clipboard.writeText(text);
  }

  /** Export terminal output as plain text or HTML. */
  function exportOutput(format: "text" | "html") {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const t = terminalsRef.current.get(pane.activeTab);
    if (!t) return;

    let content: string;
    if (format === "html") {
      content = t.serialize.serialize({ excludeModes: true });
      // Wrap in basic HTML with ANSI-to-HTML styling
      content = `<!DOCTYPE html><html><head><meta charset="utf-8"><title>Terminal Output</title><style>body{background:#0b0d12;color:#e7eaf1;font-family:monospace;white-space:pre;padding:16px;}</style></head><body>${content}</body></html>`;
    } else {
      t.xterm.selectAll();
      content = t.xterm.getSelection() || "";
      t.xterm.clearSelection();
    }

    const blob = new Blob([content], { type: format === "html" ? "text/html" : "text/plain" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `terminal-output.${format === "html" ? "html" : "txt"}`;
    a.click();
    URL.revokeObjectURL(url);
    setExportMenuOpen(false);
  }

  /** Rename a terminal tab. */
  function renameTab(tabId: string, newTitle: string) {
    if (!newTitle.trim()) return;
    const leaf = findLeafByTab(splitTree, tabId);
    if (leaf) {
      setSplitTree((prev) =>
        updateLeaf(prev, leaf.id, (l) => ({
          ...l,
          tabs: l.tabs.map((t) => t.id === tabId ? { ...t, title: newTitle.trim() } : t),
        })),
      );
    }
    setRenamingTab(null);
  }

  /** Run a command from the quick commands palette. */
  function runQuickCommand(cmd: string) {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    void ptyIpc.write(pane.activeTab, cmd + "\r");
    // Add to history (deduplicate).
    setCmdHistory((prev) => {
      const next = [cmd, ...prev.filter((c) => c !== cmd)].slice(0, MAX_HISTORY);
      saveHistory(next);
      return next;
    });
    setQuickCmdOpen(false);
    setCmdFilter("");
  }

  /** Save current terminal config as a profile. */
  function saveProfile(name: string, shell: string) {
    const newProfile: TermProfile = {
      name,
      shell: shell || getActivePane()?.tabs.find((t) => t.id === getActivePane()?.activeTab)?.shell || "/bin/zsh",
      cwd: repo.path,
    };
    const updated = [...profiles.filter((p) => p.name !== name), newProfile];
    setProfiles(updated);
    saveProfiles(updated);
  }

  /** Scroll to bottom for a terminal. */
  function scrollToBottom(termId: string) {
    const t = terminalsRef.current.get(termId);
    if (t) {
      t.xterm.scrollToBottom();
      setShowScrollBtn((prev) => { const m = new Map(prev); m.set(termId, false); return m; });
    }
  }

  /** Jump to the previous prompt mark in the active terminal. */
  function jumpToPrevPrompt() {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const si = shellIntegrationRef.current.get(pane.activeTab);
    if (si) si.jumpToPreviousPrompt();
  }

  /** Jump to the next prompt mark in the active terminal. */
  function jumpToNextPrompt() {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const si = shellIntegrationRef.current.get(pane.activeTab);
    if (si) si.jumpToNextPrompt();
  }

  /** Add a bookmark at the current scroll position (Task 27.5). */
  function addBookmark() {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const term = terminalsRef.current.get(pane.activeTab);
    if (!term) return;
    const line = (term.xterm.buffer?.active?.baseY ?? 0) + (term.xterm.buffer?.active?.cursorY ?? 0);
    let mgr = bookmarkManagersRef.current.get(pane.activeTab);
    if (!mgr) {
      mgr = new BookmarkManager();
      bookmarkManagersRef.current.set(pane.activeTab, mgr);
    }
    mgr.add(line);
    // Force re-render by toggling bookmarks panel open.
    setBookmarksOpen(true);
  }

  /** Navigate to next bookmark (Task 27.5). */
  function nextBookmark() {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const term = terminalsRef.current.get(pane.activeTab);
    const mgr = bookmarkManagersRef.current.get(pane.activeTab);
    if (!term || !mgr) return;
    const currentLine = (term.xterm.buffer?.active?.baseY ?? 0) + (term.xterm.buffer?.active?.cursorY ?? 0);
    const bm = mgr.findNext(currentLine);
    if (bm) term.xterm.scrollToLine(bm.line);
  }

  /** Navigate to previous bookmark (Task 27.5). */
  function prevBookmark() {
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const term = terminalsRef.current.get(pane.activeTab);
    const mgr = bookmarkManagersRef.current.get(pane.activeTab);
    if (!term || !mgr) return;
    const currentLine = (term.xterm.buffer?.active?.baseY ?? 0) + (term.xterm.buffer?.active?.cursorY ?? 0);
    const bm = mgr.findPrevious(currentLine);
    if (bm) term.xterm.scrollToLine(bm.line);
  }

  /** Apply font config to all open terminals (Task 26.1). */
  function handleFontChange(config: FontConfig) {
    setFontConfig(config);
    for (const inst of terminalsRef.current.values()) {
      applyFontConfig(inst.xterm, config);
      // The WebGL renderer caches font glyphs in a texture atlas built at
      // activation time. Dispose and reattach so it rebuilds with the new font.
      rebuildWebgl(inst.xterm, inst.webglState);
      inst.fit.fit();
    }
  }

  /** Handle time filter change (Task 26.6). */
  function handleTimeFilter(minutes: number | null) {
    setActiveTimeFilter(minutes);
    if (minutes === null) return;
    // Scroll to the start of the time-filtered range.
    const pane = getActivePane();
    if (!pane?.activeTab) return;
    const si = shellIntegrationRef.current.get(pane.activeTab);
    if (!si) return;
    const range = getTimeFilteredRange(si.state.marks, minutes);
    if (range) {
      const term = terminalsRef.current.get(pane.activeTab);
      if (term) term.xterm.scrollToLine(range.startLine);
    }
  }


  // Hidden kept-alive terminals retain PTYs, but must not consume app shortcuts.
  useEffect(() => {
    if (!visible) return;
    function handleKeyDown(e: KeyboardEvent) {
      const meta = e.metaKey || e.ctrlKey;

      if (meta && e.shiftKey && e.key === "`") { e.preventDefault(); void spawnTab(activePaneId); return; }
      // Cmd+T: new terminal tab (iTerm2/Chrome-style).
      if (meta && !e.shiftKey && e.key === "t") { e.preventDefault(); void spawnTab(activePaneId); return; }
      if (meta && e.key === "\\") { e.preventDefault(); splitPane("horizontal"); return; }
      if (meta && e.shiftKey && e.key === "|") { e.preventDefault(); splitPane("vertical"); return; }
      // iTerm2-style split shortcuts: Cmd+D split right, Cmd+Shift+D split down.
      if (meta && !e.shiftKey && e.key === "d") { e.preventDefault(); splitPane("horizontal"); return; }
      if (meta && e.shiftKey && e.key === "D") { e.preventDefault(); splitPane("vertical"); return; }
      if (meta && e.key === "f") { e.preventDefault(); setSearchVisible((v) => !v); if (!searchVisible) setTimeout(() => searchInputRef.current?.focus(), 50); return; }
      if (meta && e.key === "w") { const pane = getActivePane(); if (pane?.activeTab) { e.preventDefault(); closeTabFromTabStrip(pane.id, pane.activeTab); } return; }
      // Cmd+Shift+W: close the entire pane (if more than one pane exists).
      if (meta && e.shiftKey && e.key === "W") { e.preventDefault(); if (getAllLeaves(splitTree).length > 1) removePane(activePaneId); return; }
      if (meta && e.key === "k") { const pane = getActivePane(); if (pane?.activeTab) { const term = terminalsRef.current.get(pane.activeTab); if (term) { e.preventDefault(); term.xterm.clear(); void (window.crab as any).termClearSession(pane.activeTab, persistenceKey); } } return; }
      if (meta && e.shiftKey && e.key === "C") { e.preventDefault(); copyAllOutput(); return; }
      if (meta && e.shiftKey && e.key === "[") { e.preventDefault(); switchTab(-1); return; }
      if (meta && e.shiftKey && e.key === "]") { e.preventDefault(); switchTab(1); return; }
      if (meta && e.shiftKey && e.key === "R") { e.preventDefault(); setQuickCmdOpen((v) => !v); return; }
      // Cmd+Shift+B: toggle broadcast mode.
      if (meta && e.shiftKey && e.key === "B") { e.preventDefault(); setBroadcastMode((v) => !v); return; }
      // Cmd+Shift+N: rename current tab.
      if (meta && e.shiftKey && e.key === "N") { e.preventDefault(); const pane = getActivePane(); if (pane?.activeTab) setRenamingTab(pane.activeTab); return; }
      // Cmd+Alt+L: toggle scroll lock on active tab.
      if (meta && e.altKey && e.key === "l") { e.preventDefault(); const pane = getActivePane(); if (pane?.activeTab) { setScrollLocked((prev) => { const m = new Map(prev); m.set(pane.activeTab!, !(m.get(pane.activeTab!) ?? false)); return m; }); } return; }
      // Focus next/previous pane: Cmd+Alt+] / Cmd+Alt+[
      if (meta && e.altKey && e.key === "]") { e.preventDefault(); const next = getNextLeaf(splitTree, activePaneId); if (next) setActivePaneId(next.id); return; }
      if (meta && e.altKey && e.key === "[") { e.preventDefault(); const prev = getPrevLeaf(splitTree, activePaneId); if (prev) setActivePaneId(prev.id); return; }
      // Cmd+1 through Cmd+9: jump to tab by position (Cmd+9 always goes to last tab).
      if (meta && !e.shiftKey && !e.altKey && e.key >= "1" && e.key <= "9") {
        e.preventDefault();
        const activeLeaf = findLeaf(splitTree, activePaneId);
        if (activeLeaf && activeLeaf.tabs.length > 0) {
          const idx = e.key === "9" ? activeLeaf.tabs.length - 1 : Math.min(parseInt(e.key) - 1, activeLeaf.tabs.length - 1);
          const targetTab = activeLeaf.tabs[idx];
          if (targetTab && targetTab.id !== activeLeaf.activeTab) {
            const mru = tabMruRef.current.get(activePaneId) ?? [];
            tabMruRef.current.set(activePaneId, [targetTab.id, ...mru.filter((id) => id !== targetTab.id)]);
            setSplitTree((prev) => updateLeaf(prev, activePaneId, (leaf) => ({ ...leaf, activeTab: targetTab.id })));
          }
        }
        return;
      }
      // Prompt jumping: Cmd+Up / Cmd+Down (Ctrl+Up / Ctrl+Down on non-Mac)
      if (meta && e.key === "ArrowUp") { e.preventDefault(); jumpToPrevPrompt(); return; }
      if (meta && e.key === "ArrowDown") { e.preventDefault(); jumpToNextPrompt(); return; }
      if (e.key === "Escape") { if (searchVisible) setSearchVisible(false); if (quickCmdOpen) setQuickCmdOpen(false); return; }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activePaneId, splitTree, searchVisible, quickCmdOpen, visible]);

  function switchTab(direction: number) {
    const activeLeaf = findLeaf(splitTree, activePaneId);
    if (!activeLeaf || activeLeaf.tabs.length < 2) return;
    const idx = activeLeaf.tabs.findIndex((t) => t.id === activeLeaf.activeTab);
    const next = (idx + direction + activeLeaf.tabs.length) % activeLeaf.tabs.length;
    const nextTabId = activeLeaf.tabs[next]?.id;
    if (nextTabId) {
      const mru = tabMruRef.current.get(activePaneId) ?? [];
      tabMruRef.current.set(activePaneId, [nextTabId, ...mru.filter((id) => id !== nextTabId)]);
    }
    setSplitTree((prev) =>
      updateLeaf(prev, activePaneId, (leaf) => ({
        ...leaf,
        activeTab: nextTabId ?? leaf.activeTab,
      })),
    );
  }

  function searchNext() { const pane = getActivePane(); if (!pane?.activeTab) return; const term = terminalsRef.current.get(pane.activeTab); if (term) term.search.findNext(searchQuery, { caseSensitive: searchCaseSensitive, regex: searchRegex }); }
  function searchPrev() { const pane = getActivePane(); if (!pane?.activeTab) return; const term = terminalsRef.current.get(pane.activeTab); if (term) term.search.findPrevious(searchQuery, { caseSensitive: searchCaseSensitive, regex: searchRegex }); }

  function handleContextMenu(e: React.MouseEvent, termId: string, paneId: string) {
    e.preventDefault();
    setContextMenu({ x: e.clientX, y: e.clientY, termId, paneId });
  }

  /** Handle paste through the paste guard. */
  function handlePasteWithGuard(termId: string, content: string) {
    const config = pasteGuardConfigRef.current;
    if (!config.enabled) {
      // Paste protection disabled — write directly to PTY.
      void ptyIpc.write(termId, content);
      return;
    }
    const analysis = analyzePaste(content, config);
    if (analysis.requiresConfirmation) {
      setPasteDialogState({ analysis, content, termId });
    } else {
      void ptyIpc.write(termId, content);
    }
  }

  function confirmPaste() {
    if (!pasteDialogState) return;
    const { content, termId } = pasteDialogState;
    void ptyIpc.write(termId, wrapBracketedPaste(content));
    setPasteDialogState(null);
  }

  function cancelPaste() {
    setPasteDialogState(null);
  }

  /** Handle command completion: set tab badge and dispatch notification. */
  function handleCommandComplete(tabId: string, completion: CommandCompletion) {
    // Set tab badge for unfocused tabs.
    if (!completion.tabFocused) {
      const badge: "success" | "error" = completion.exitCode === 0 ? "success" : "error";
      setTabBadges((prev) => { const next = new Map(prev); next.set(tabId, badge); return next; });
    }

    // Dispatch native notification if conditions are met.
    if (shouldNotify(completion, notificationConfig)) {
      const { title, body, icon } = formatNotification(completion);
      // Find the tab index for the notification click handler.
      let tabIndex = 0;
      const allLeaves = getAllLeaves(splitTree);
      for (const leaf of allLeaves) {
        const idx = leaf.tabs.findIndex((t) => t.id === tabId);
        if (idx >= 0) { tabIndex = idx; break; }
      }
      void (window.crab as any).termNotify({ title, body, icon, terminalId: tabId, tabIndex });
    }

    // Play completion sound if enabled.
    if (notificationConfig.soundEnabled && !completion.tabFocused) {
      playCompletionSound(notificationConfig.soundFile);
    }
  }

  /** Play a completion sound (system bell or custom audio file). */
  function playCompletionSound(soundFile?: string) {
    try {
      if (soundFile) {
        const audio = new Audio(soundFile);
        void audio.play();
      } else {
        // System bell: use a short beep via AudioContext.
        const ctx = new AudioContext();
        const oscillator = ctx.createOscillator();
        const gain = ctx.createGain();
        oscillator.connect(gain);
        gain.connect(ctx.destination);
        oscillator.frequency.value = 880;
        gain.gain.value = 0.1;
        oscillator.start();
        oscillator.stop(ctx.currentTime + 0.1);
      }
    } catch {
      // Non-fatal — sound playback failure shouldn't break anything.
    }
  }

  function contextMenuAction(action: string) {
    if (!contextMenu) return;
    const term = terminalsRef.current.get(contextMenu.termId);
    switch (action) {
      case "copy": { if (term) { const sel = term.xterm.getSelection(); if (sel) navigator.clipboard.writeText(sel); } break; }
      case "copyAll": copyAllOutput(contextMenu.termId); break;
      case "copyOutput": {
        const si = shellIntegrationRef.current.get(contextMenu.termId);
        if (si) {
          const marks = si.state.marks;
          let lastExecIndex = -1;
          for (let i = marks.length - 1; i >= 0; i--) {
            if (marks[i].type === "command-executed") { lastExecIndex = i; break; }
          }
          if (lastExecIndex >= 0) {
            const text = si.selectCommandOutput(lastExecIndex);
            if (text) void navigator.clipboard.writeText(text);
          }
        }
        break;
      }
      case "paste": navigator.clipboard.readText().then((text) => { handlePasteWithGuard(contextMenu.termId, text); }); break;
      case "selectAll": if (term) term.xterm.selectAll(); break;
      case "clear": if (term) { term.xterm.clear(); void (window.crab as any).termClearSession(contextMenu.termId, persistenceKey); } break;
      case "splitRight": splitPane("horizontal", contextMenu.paneId); break;
      case "splitDown": splitPane("vertical", contextMenu.paneId); break;
      case "addBookmark": {
        const bTerm = terminalsRef.current.get(contextMenu.termId);
        if (bTerm) {
          const line = (bTerm.xterm.buffer?.active?.baseY ?? 0) + (bTerm.xterm.buffer?.active?.cursorY ?? 0);
          let mgr = bookmarkManagersRef.current.get(contextMenu.termId);
          if (!mgr) { mgr = new BookmarkManager(); bookmarkManagersRef.current.set(contextMenu.termId, mgr); }
          mgr.add(line);
        }
        break;
      }
      case "rename": setRenamingTab(contextMenu.termId); break;
      case "kill": killTab(contextMenu.paneId, contextMenu.termId); break;
      case "exportText": exportOutput("text"); break;
      case "exportHtml": exportOutput("html"); break;
      case "newDaemonTerminal": void spawnTab(contextMenu.paneId, undefined, undefined, "daemon"); break;
      case "newLocalTerminal": void spawnTab(contextMenu.paneId, undefined, undefined, "in-process"); break;
    }
    setContextMenu(null);
  }

  useEffect(() => { if (!contextMenu) return; const close = () => setContextMenu(null); window.addEventListener("click", close); return () => window.removeEventListener("click", close); }, [contextMenu]);

  // Listen for data from the pty with buffering.
  /** Tracks terminal IDs that were spawned with a startup command and
   *  haven't received their first real data yet. On first data, we clear
   *  the loading indicator from the xterm buffer. */
  const startupPendingRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    const off = ptyIpc.onData(({ id, data }) => {
      const t = terminalsRef.current.get(id);
      if (t) {
        // If this is the first data for a startup-command tab, clear the
        // loading indicator before writing the real output.
        if (startupPendingRef.current.has(id)) {
          startupPendingRef.current.delete(id);
          t.xterm.clear();
          t.xterm.write("\x1b[H"); // Move cursor to top-left
        }
        // Strip ED 3 (\x1b[3J) — "Erase Saved Lines" — from PTY output.
        // Shells send this in response to Ctrl+L to destroy scrollback, but
        // we want scrollback preserved so users can scroll up to find history.
        // The viewport clear (\x1b[2J) still works normally — our Ctrl+L
        // handler already pushed viewport content into scrollback before the
        // shell's response arrives.
        const sanitized = data.replace(/\x1b\[3J/g, "");
        t.xterm.write(sanitized);
        // Track command history from Enter keypresses (heuristic).
      } else {
        const hidden = hiddenDaemonSessionIdsRef.current.has(id);
        if (!hidden) {
          const buf = pendingDataRef.current.get(id);
          if (buf) buf.push(data); else pendingDataRef.current.set(id, [data]);
        }
      }
      if (!hiddenDaemonSessionIdsRef.current.has(id)) {
        appendOutput(id, data);
      }
    });
    return off;
  }, []);

  // SSH remote terminal data listener
  useEffect(() => {
    const off = sshTermIpc.onData(({ terminalId, data }) => {
      const t = terminalsRef.current.get(terminalId);
      if (t) {
        // Strip ED 3 to preserve scrollback (same as local PTY handler).
        const sanitized = data.replace(/\x1b\[3J/g, "");
        t.xterm.write(sanitized);
      } else {
        const buf = pendingDataRef.current.get(terminalId);
        if (buf) buf.push(data); else pendingDataRef.current.set(terminalId, [data]);
      }
    });
    return off;
  }, []);

  // SSH terminal session ended (channel died unexpectedly)
  useEffect(() => {
    const off = sshTermIpc.onSessionEnded(({ terminalId }) => {
      const t = terminalsRef.current.get(terminalId);
      if (t) {
        t.xterm.write("\r\n\x1b[90m[Session ended — press Enter to reconnect]\x1b[0m\r\n");
      }
    });
    return off;
  }, []);

  // SSH terminal session resumed seamlessly after reconnection
  useEffect(() => {
    const off = sshTermIpc.onResumed(({ terminalId }) => {
      const t = terminalsRef.current.get(terminalId);
      if (t) {
        t.xterm.write("\x1b[90m[Session resumed]\x1b[0m\r\n");
      }
    });
    return off;
  }, []);

  // IPC exit listener — fires regardless of the `visible` prop because IPC
  // events are not gated on DOM visibility. This ensures that when a kept-alive
  // terminal's process exits while hidden, state updates (failedExits, xterm
  // output) still occur. Tab badges are set separately via handleCommandComplete
  // (shell integration path), which also runs regardless of visibility.
  useEffect(() => {
    const off = ptyIpc.onExit(({ id, exitCode }) => {
      const t = terminalsRef.current.get(id);
      if (t) {
        t.xterm.write(`\r\n\x1b[90m[Process exited with code ${exitCode}]\x1b[0m\r\n`);
      }
      markTerminalClosed(id);
      if (exitCode !== 0 && onSendErrorToAgent) {
        setFailedExits((prev) => {
          const next = new Map(prev);
          next.set(id, { exitCode, at: Date.now() });
          return next;
        });
      }
    });
    return off;
  }, [onSendErrorToAgent]);

  // Listen for cold-restore events from the daemon bridge.
  // Each event represents a session that was checkpointed before a crash
  // and can be resumed. We register the request and mark the tab with a
  // "Restored from <ts>" badge so the user can confirm or discard.
  useEffect(() => {
    const off = ptyIpc.onColdRestore((evt) => {
      // Register the cold-restore request — the tab badge and confirm dialog
      // are rendered from this state.
      setColdRestoreRequests((prev) => {
        const next = new Map(prev);
        next.set(evt.id, {
          id: evt.id,
          savedAt: evt.savedAt,
          sanitizedKeys: evt.sanitizedKeys,
          cwd: evt.cwd,
          cols: evt.cols,
          rows: evt.rows,
          envValues: {},
        });
        return next;
      });

      // Mark the tab as cold-restored so the tab bar shows the badge.
      const leaf = findLeafByTab(splitTreeRef.current, evt.id);
      if (leaf) {
        setSplitTree((prev) =>
          updateLeaf(prev, leaf.id, (l) => ({
            ...l,
            tabs: l.tabs.map((t) =>
              t.id === evt.id
                ? { ...t, coldRestored: { savedAt: evt.savedAt } } as LeafTab
                : t,
            ),
          })),
        );
      }
    });
    return off;
  }, []);

  /** Confirm a cold-restore: send the user-provided env to the daemon. */
  const confirmColdRestore = useCallback((sessionId: string) => {
    const req = coldRestoreRequests.get(sessionId);
    if (!req) return;
    void ptyIpc.coldRestoreConfirm(sessionId, req.envValues);
    setColdRestoreRequests((prev) => {
      const next = new Map(prev);
      next.delete(sessionId);
      return next;
    });
  }, [coldRestoreRequests]);

  /** Discard a cold-restore: tell the daemon to remove the history dir. */
  const discardColdRestore = useCallback((sessionId: string) => {
    void ptyIpc.coldRestoreDiscard(sessionId);
    setColdRestoreRequests((prev) => {
      const next = new Map(prev);
      next.delete(sessionId);
      return next;
    });
    // Remove the cold-restored badge from the tab.
    const leaf = findLeafByTab(splitTreeRef.current, sessionId);
    if (leaf) {
      setSplitTree((prev) =>
        updateLeaf(prev, leaf.id, (l) => ({
          ...l,
          tabs: l.tabs.map((t) =>
            t.id === sessionId
              ? { ...t, coldRestored: undefined } as LeafTab
              : t,
          ),
        })),
      );
    }
  }, []);

  // ─── Disk-full / write error toast for session logging ──────────
  useEffect(() => {
    const off = (window.crab as any).onTermLogError?.((evt: { sessionId: string; error: string }) => {
      const errorMsg = evt.error === "ENOSPC"
        ? "Disk full — session logging disabled for this terminal."
        : "Permission denied — session logging disabled for this terminal.";
      publishToast({
        kind: "warning",
        title: "Terminal Logging",
        body: errorMsg,
        duration: 8000,
      });
    });
    return off ?? (() => {});
  }, []);

  // ─── Send terminal error to the agent ───────────────────────────
  //
  // Drops the last 200 ANSI-stripped lines plus the workspace cwd and
  // exit code into the composer, then clears the failure marker. The
  // chat composer owns the "how do we get this in front of the user"
  // part — we just hand it a formatted feedback blob.

  const sendErrorToAgent = useCallback(
    (termId: string) => {
      if (!onSendErrorToAgent) return;
      const failure = failedExits.get(termId);
      if (!failure) return;

      const rawTail = terminalTail(termId, 16 * 1024);
      const stripped = stripAnsiForAgent(rawTail);
      const lines = stripped.split(/\r?\n/).slice(-200).join("\n");
      const allTabs = getAllLeaves(splitTree).flatMap((l) => l.tabs);
      const termMeta = allTabs.find((t) => t.id === termId);
      const label = termMeta?.title ?? "terminal";

      const feedback = [
        `The last command in \`${label}\` (${repo.path ?? "workspace"}) exited ${failure.exitCode}.`,
        "Investigate the failure and fix the root cause. Output (last 200 lines):",
        "",
        "```",
        lines,
        "```",
      ].join("\n");
      onSendErrorToAgent(feedback);
      setFailedExits((prev) => {
        const next = new Map(prev);
        next.delete(termId);
        return next;
      });
    },
    [failedExits, onSendErrorToAgent, splitTree, repo.path],
  );

  const dismissError = useCallback((termId: string) => {
    setFailedExits((prev) => {
      const next = new Map(prev);
      next.delete(termId);
      return next;
    });
  }, []);

  // Filtered command history for quick commands palette.
  const filteredHistory = cmdFilter
    ? cmdHistory.filter((c) => c.toLowerCase().includes(cmdFilter.toLowerCase()))
    : cmdHistory;
  const openTerminalTabs = getAllLeaves(splitTree).flatMap((leaf) =>
    leaf.tabs.map((tab) => ({ paneId: leaf.id, tab: tab as TermTab })),
  );
  const openTerminalIds = new Set(openTerminalTabs.map(({ tab }) => tab.id));
  const backgroundDaemonCount = daemonSessions.filter((session) => !openTerminalIds.has(session.id)).length;
  const sessionManager = (
    <TerminalSessionManager
      open={sessionsManagerOpen}
      side={sessionSidebarSide}
      sessions={daemonSessions}
      openTabs={openTerminalTabs}
      openTerminalIds={openTerminalIds}
      activeTabId={getActivePane()?.activeTab ?? null}
      pinnedSessionIds={pinnedDaemonSessionIds}
      search={sessionSearch}
      onSearchChange={setSessionSearch}
      onSideChange={setSessionSidebarSide}
      onClose={() => setSessionsManagerOpen(false)}
      onRefresh={() => void refreshDaemonSessions()}
      onAttach={(session) => void attachDaemonSession(session, activePaneId)}
      onOpenInSplit={(session) => void attachDaemonSession(session, activePaneId, "horizontal")}
      onFocusTab={focusOpenTab}
      onHide={(paneId, tabId) => hideTabImmediate(paneId, tabId)}
      onKill={killDaemonSessionFromManager}
      onTogglePin={togglePinnedDaemonSession}
    />
  );

  return (
    <div className="flex h-full w-full min-w-0 flex-col bg-gx-bg-sub">
      {/* Toolbar row */}
      <div className="flex shrink-0 items-center gap-1 border-y border-gx-border bg-gx-surface px-2 py-1">
        {/* Broadcast toggle */}
        <button
          type="button"
          onClick={() => setBroadcastMode((v) => !v)}
          className={clsx(
            "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]",
            broadcastMode ? "bg-gx-warn/20 text-gx-warn" : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
          )}
          title="Broadcast input to all terminals"
        >
          <Radio size={10} />
          {broadcastMode && <span>Broadcast</span>}
        </button>

        {/* Quick commands */}
        <button
          type="button"
          onClick={() => { setQuickCmdOpen((v) => !v); setTimeout(() => cmdInputRef.current?.focus(), 50); }}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
          title="Quick commands (⌘⇧R)"
        >
          <Command size={10} />
        </button>

        {/* Profiles */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setProfileMenuOpen((v) => !v)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title="Terminal profiles"
          >
            <UserPlus size={10} />
          </button>
          {profileMenuOpen && (
            <ProfileMenu
              profiles={profiles}
              onSelect={(p) => { setProfileMenuOpen(false); void spawnTab(activePaneId, undefined, p); }}
              onSave={(name) => { setProfileMenuOpen(false); saveProfile(name, ""); }}
              onDelete={(name) => { const updated = profiles.filter((p) => p.name !== name); setProfiles(updated); saveProfiles(updated); }}
              onClose={() => setProfileMenuOpen(false)}
            />
          )}
        </div>

        {/* Export */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setExportMenuOpen((v) => !v)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title="Export output"
          >
            <FileDown size={10} />
          </button>
          {exportMenuOpen && (
            <div className="absolute left-0 top-full z-50 mt-1 min-w-[120px] rounded border border-gx-border bg-gx-surface py-1 shadow-lg">
              <button type="button" onClick={() => exportOutput("text")} className="block w-full px-3 py-1 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover">Plain Text</button>
              <button type="button" onClick={() => exportOutput("html")} className="block w-full px-3 py-1 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover">HTML (styled)</button>
            </div>
          )}
        </div>

        {/* Colour scheme picker */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setSchemeMenuOpen((v) => !v)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title={`Terminal colour scheme (${getScheme(schemeId).name})`}
            aria-label="Terminal colour scheme"
          >
            <Palette size={10} />
          </button>
          {schemeMenuOpen && (
            <SchemeMenu
              currentId={schemeId}
              mode={theme}
              onSelect={(id) => {
                setSchemeId(id);
                setStoredSchemeId(id);
                setSchemeMenuOpen(false);
              }}
              onClose={() => setSchemeMenuOpen(false)}
            />
          )}
        </div>

        {/* Font settings (Task 26.1) */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setFontMenuOpen((v) => !v)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title="Terminal font settings"
            aria-label="Terminal font settings"
          >
            <Type size={10} />
          </button>
          {fontMenuOpen && (
            <TerminalFontSettings
              onClose={() => setFontMenuOpen(false)}
              onChange={handleFontChange}
            />
          )}
        </div>

        {/* Scrollback settings */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setScrollbackMenuOpen((v) => !v)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title="Scrollback buffer settings"
            aria-label="Scrollback buffer settings"
          >
            <SlidersHorizontal size={10} />
          </button>
          {scrollbackMenuOpen && (
            <ScrollbackSettingsMenu
              onClose={() => setScrollbackMenuOpen(false)}
            />
          )}
        </div>

        {/* Paste guard settings */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setPasteGuardMenuOpen((v) => !v)}
            className={clsx(
              "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]",
              pasteGuardConfig.enabled
                ? "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
                : "text-gx-warn/70 hover:text-gx-warn hover:bg-gx-warn/10",
            )}
            title={pasteGuardConfig.enabled ? "Paste protection enabled" : "Paste protection disabled"}
            aria-label="Paste protection settings"
          >
            <AlertTriangle size={10} />
          </button>
          {pasteGuardMenuOpen && (
            <PasteGuardSettingsMenu
              config={pasteGuardConfig}
              onChange={(updated) => {
                setPasteGuardConfig(updated);
                savePasteGuardConfig(updated);
              }}
              onClose={() => setPasteGuardMenuOpen(false)}
            />
          )}
        </div>

        {/* Scrollbar mode settings */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setScrollbarMenuOpen((v) => !v)}
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
            title={`Scrollbar: ${scrollbarMode}`}
            aria-label="Scrollbar mode settings"
          >
            <SlidersHorizontal size={10} className="rotate-90" />
          </button>
          {scrollbarMenuOpen && (
            <ScrollbarModeMenu
              currentMode={scrollbarMode}
              onSelect={(mode) => {
                setScrollbarMode(mode);
                saveScrollbarMode(mode);
                setScrollbarMenuOpen(false);
              }}
              onClose={() => setScrollbarMenuOpen(false)}
            />
          )}
        </div>

        {/* Breadcrumb bar toggle (global default) */}
        <button
          type="button"
          onClick={() => {
            const next = !breadcrumbDefault;
            setBreadcrumbDefault(next);
            localStorage.setItem(LS_BREADCRUMB_KEY, String(next));
            // Apply to all existing tabs that haven't been individually overridden
            // by clearing per-tab state so they inherit the new default.
            setBreadcrumbPerTab(new Map());
          }}
          className={clsx(
            "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]",
            breadcrumbDefault ? "text-gx-accent" : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
          )}
          title={breadcrumbDefault ? "Hide breadcrumb bar (all tabs)" : "Show breadcrumb bar (all tabs)"}
          aria-label="Toggle breadcrumb bar"
        >
          <ChevronDown size={10} className="-rotate-90" />
        </button>

        <button
          type="button"
          onClick={() => setSessionsManagerOpen(true)}
          className={clsx(
            "flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]",
            backgroundDaemonCount > 0
              ? "text-gx-accent hover:bg-gx-accent/10"
              : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
          )}
          title={backgroundDaemonCount > 0 ? `${backgroundDaemonCount} background daemon session${backgroundDaemonCount === 1 ? "" : "s"}` : "Terminal sessions"}
          aria-label="Terminal sessions"
        >
          {sessionSidebarSide === "left" ? <PanelLeftOpen size={10} /> : <PanelRightOpen size={10} />}
          {backgroundDaemonCount > 0 && <span>{backgroundDaemonCount}</span>}
        </button>

        <div className="flex-1" />

        {/* Failed-exit "Send error to agent" pill — surfaces when an
            agent-owned callback is wired and a tab ended non-zero. */}
        {onSendErrorToAgent && (() => {
          const activeTab = getActivePane()?.activeTab;
          if (!activeTab) return null;
          const failure = failedExits.get(activeTab);
          if (!failure) return null;
          return (
            <div className="flex items-center gap-1.5 mr-2 text-[10px]">
              <span className="text-gx-error">Exit {failure.exitCode}</span>
              <button
                type="button"
                onClick={() => sendErrorToAgent(activeTab)}
                className="inline-flex items-center gap-1 rounded-md border border-gx-accent/40 bg-gx-accent/10 px-2 py-0.5 text-gx-accent hover:bg-gx-accent/20 transition"
                title="Send the last 200 lines to the agent and ask it to fix this"
              >
                <SendHorizonal size={9} /> Send error to agent
              </button>
              <button
                type="button"
                onClick={() => dismissError(activeTab)}
                className="rounded p-0.5 text-gx-muted hover:text-gx-text"
                aria-label="Dismiss exit notice"
              >
                <X size={10} />
              </button>
            </div>
          );
        })()}

        {/* Split buttons */}
        <button type="button" onClick={() => splitPane("horizontal")} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Split right (⌘\)">
          <SplitSquareHorizontal size={11} />
        </button>
        <button type="button" onClick={() => splitPane("vertical")} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Split down (⌘⇧|)">
          <SplitSquareVertical size={11} />
        </button>

        {/* Preset layouts */}
        <div className="relative">
          <button
            type="button"
            onClick={() => setPresetMenuOpen((v) => !v)}
            className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            title="Preset layouts"
            aria-label="Preset layouts"
          >
            <LayoutGrid size={11} />
          </button>
          {presetMenuOpen && (
            <div className="absolute right-0 top-full z-50 mt-1 min-w-[140px] rounded border border-gx-border bg-gx-surface py-1 shadow-lg">
              {PRESET_LAYOUTS.map((preset) => (
                <button
                  key={preset.id}
                  type="button"
                  onClick={() => { applyPresetLayout(preset.cols, preset.rows); setPresetMenuOpen(false); }}
                  className="block w-full px-3 py-1.5 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover"
                >
                  <span className="font-medium">{preset.label}</span>
                  <span className="ml-2 text-gx-muted">{preset.description}</span>
                </button>
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Quick commands palette */}
      {quickCmdOpen && (
        <div className="shrink-0 border-b border-gx-border bg-gx-surface px-3 py-2">
          <input
            ref={cmdInputRef}
            type="text"
            value={cmdFilter}
            onChange={(e) => setCmdFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && cmdFilter.trim()) { runQuickCommand(cmdFilter.trim()); }
              if (e.key === "Escape") { setQuickCmdOpen(false); setCmdFilter(""); }
            }}
            placeholder="Type a command or search history…"
            className="w-full bg-transparent text-[12px] text-gx-text placeholder:text-gx-muted outline-none"
            autoFocus
          />
          {filteredHistory.length > 0 && (
            <div className="mt-1 max-h-32 overflow-y-auto">
              {filteredHistory.slice(0, 10).map((cmd, i) => (
                <button
                  key={i}
                  type="button"
                  onClick={() => runQuickCommand(cmd)}
                  className="block w-full truncate rounded px-2 py-0.5 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover"
                >
                  <span className="font-mono">{cmd}</span>
                </button>
              ))}
            </div>
          )}
        </div>
      )}

      {/* Search bar — enhanced with replace, time filter, and semantic filter */}
      {searchVisible && (
        <TerminalSearchEnhanced
          query={searchQuery} onQueryChange={setSearchQuery}
          caseSensitive={searchCaseSensitive} onCaseSensitiveChange={setSearchCaseSensitive}
          regex={searchRegex} onRegexChange={setSearchRegex}
          onNext={searchNext} onPrev={searchPrev}
          onClose={() => setSearchVisible(false)} inputRef={searchInputRef}
          onReplace={(replacement) => {
            // Replace is informational in terminal context — copy replacement to clipboard
            void navigator.clipboard.writeText(replacement);
          }}
          onReplaceAll={(replacement) => {
            void navigator.clipboard.writeText(replacement);
          }}
          onTimeFilter={handleTimeFilter}
          activeTimeFilter={activeTimeFilter}
          onSemanticFilter={setActiveSemanticFilter}
          activeSemanticFilter={activeSemanticFilter}
        />
      )}

      {/* Bookmarks panel (Task 27.5) */}
      {bookmarksOpen && (() => {
        const activeTab = getActivePane()?.activeTab;
        const mgr = activeTab ? bookmarkManagersRef.current.get(activeTab) : null;
        return (
          <TerminalBookmarksPanel
            bookmarks={mgr?.bookmarks ?? []}
            onAdd={addBookmark}
            onRemove={(id) => { if (mgr) { mgr.remove(id); setBookmarksOpen(true); /* force re-render */ } }}
            onNavigate={(line) => { if (activeTab) { const t = terminalsRef.current.get(activeTab); if (t) t.xterm.scrollToLine(line); } }}
            onNextBookmark={nextBookmark}
            onPrevBookmark={prevBookmark}
            onClose={() => setBookmarksOpen(false)}
          />
        );
      })()}

      <div className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
        {sessionSidebarSide === "left" && sessionManager}

        {/* Split pane layout — recursive tree renderer */}
        <div className="min-h-0 min-w-0 flex-1">
          <SplitTreeRenderer
            root={splitTree}
            renderLeaf={(leaf) => {
              // Build a map of tab ID → resolvedSessionType for this leaf's tabs.
              const sessionTypes = new Map<string, "daemon" | "in-process">();
              for (const t of leaf.tabs) {
                const tab = t as TermTab;
                if (tab.resolvedSessionType) sessionTypes.set(tab.id, tab.resolvedSessionType);
              }
              return (
              <TerminalPane
                pane={leaf}
                isActive={leaf.id === activePaneId}
                theme={theme}
                schemeId={schemeId}
                broadcastMode={broadcastMode}
                allTermIds={getAllTermIds()}
                scrollLocked={scrollLocked.get(leaf.activeTab ?? "") ?? false}
                showScrollBtn={showScrollBtn.get(leaf.activeTab ?? "") ?? false}
                scrollbarMode={scrollbarMode}
                scrollbarAutohide={scrollbarAutohide}
                onScrollLockToggle={(locked) => { const id = leaf.activeTab; if (id) setScrollLocked((prev) => { const m = new Map(prev); m.set(id, locked); return m; }); }}
                onScrollToBottom={() => { if (leaf.activeTab) scrollToBottom(leaf.activeTab); }}
                onSetShowScrollBtn={(show) => { const id = leaf.activeTab; if (id) setShowScrollBtn((prev) => { const m = new Map(prev); m.set(id, show); return m; }); }}
                onActivate={() => setActivePaneId(leaf.id)}
                onSpawnTab={(shell, profile) => spawnTab(leaf.id, shell, profile)}
                onKillTab={(tabId) => closeTabFromTabStrip(leaf.id, tabId)}
                onSplitRight={() => splitPane("horizontal", leaf.id)}
                onSplitDown={() => splitPane("vertical", leaf.id)}
                onClose={getAllLeaves(splitTree).length > 1 ? () => removePane(leaf.id) : undefined}
                onSetActiveTab={(tabId) => { setSplitTree((prev) => updateLeaf(prev, leaf.id, (l) => ({ ...l, activeTab: tabId }))); setTabBadges((prev) => { const next = new Map(prev); next.delete(tabId); return next; }); const mru = tabMruRef.current.get(leaf.id) ?? []; tabMruRef.current.set(leaf.id, [tabId, ...mru.filter((id) => id !== tabId)]); }}
                onRenameTab={renameTab}
                renamingTab={renamingTab}
                setRenamingTab={setRenamingTab}
                containerRefs={containerRefs}
                terminalsRef={terminalsRef}
                pendingRestoreSnapshotsRef={pendingRestoreSnapshotsRef}
                shellIntegrationRef={shellIntegrationRef}
                pendingDataRef={pendingDataRef}
                bookmarkManagersRef={bookmarkManagersRef}
                onContextMenu={(e, termId) => handleContextMenu(e, termId, leaf.id)}
                availableShells={availableShells}
                shellMenuOpen={shellMenuOpen && leaf.id === activePaneId}
                onShellMenuToggle={(open) => { setShellMenuOpen(open); setActivePaneId(leaf.id); }}
                cmdHistory={cmdHistory}
                onAddHistory={(cmd) => setCmdHistory((prev) => { const next = [cmd, ...prev.filter((c) => c !== cmd)].slice(0, MAX_HISTORY); saveHistory(next); return next; })}
                repoCwd={repo.path || ""}
                onUpdateTabTitle={(tabId, title) => { const tabLeaf = findLeafByTab(splitTree, tabId); if (tabLeaf) setSplitTree((prev) => updateLeaf(prev, tabLeaf.id, (l) => ({ ...l, tabs: l.tabs.map((t) => t.id === tabId ? { ...t, title } : t) }))); }}
                onPaste={handlePasteWithGuard}
                tabBadges={tabBadges}
                onCommandComplete={handleCommandComplete}
                breadcrumbDefault={breadcrumbDefault}
                breadcrumbPerTab={breadcrumbPerTab}
                onToggleBreadcrumb={(tabId) => {
                  setBreadcrumbPerTab((prev) => {
                    const next = new Map(prev);
                    const current = next.get(tabId) ?? breadcrumbDefault;
                    next.set(tabId, !current);
                    return next;
                  });
                }}
                fontConfig={fontConfig}
                sessionTypes={sessionTypes}
                visible={visible}
                backgroundSessionCount={backgroundDaemonCount}
                onOpenSessionManager={() => setSessionsManagerOpen(true)}
              />
              );
            }}
          />
        </div>

        {sessionSidebarSide === "right" && sessionManager}
      </div>

      {/* Context menu */}
      {contextMenu && <ContextMenu x={contextMenu.x} y={contextMenu.y} onAction={contextMenuAction} daemonReady={daemonReady} breakerOpen={breakerOpen} />}

      {/* Paste confirmation dialog */}
      {pasteDialogState && (
        <PasteConfirmDialog
          analysis={pasteDialogState.analysis}
          onConfirm={confirmPaste}
          onCancel={cancelPaste}
        />
      )}

      {/* Cold-restore confirmation dialog — shown when a session was
          checkpointed before a crash and the user needs to confirm resume. */}
      {(() => {
        // Show the dialog for the first pending cold-restore request.
        const pending = [...coldRestoreRequests.values()][0];
        if (!pending) return null;
        const savedDate = new Date(pending.savedAt);
        const savedLabel = savedDate.toLocaleString();
        return (
          <div
            className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
            role="dialog"
            aria-modal="true"
            aria-labelledby="cold-restore-title"
          >
            <div className="w-full max-w-md rounded-lg border border-gx-border bg-gx-surface p-5 shadow-xl">
              <h2 id="cold-restore-title" className="mb-2 text-[13px] font-semibold text-gx-text">
                Restore terminal session?
              </h2>
              <p className="mb-4 text-[12px] text-gx-muted">
                A terminal session was saved on {savedLabel}.
                {pending.cwd && (
                  <span className="block mt-1 font-mono text-[11px] text-gx-text truncate" title={pending.cwd}>
                    {pending.cwd}
                  </span>
                )}
              </p>

              {/* Env key prompts — only shown when secrets were stripped */}
              {pending.sanitizedKeys.length > 0 && (
                <div className="mb-4 rounded border border-gx-warn/30 bg-gx-warn/5 p-3">
                  <p className="mb-2 text-[11px] text-gx-warn">
                    These environment variables were stripped on save:{" "}
                    <span className="font-mono">{pending.sanitizedKeys.join(", ")}</span>.
                    Provide values now or leave blank to use defaults.
                  </p>
                  <div className="flex flex-col gap-2">
                    {pending.sanitizedKeys.map((key) => (
                      <label key={key} className="flex flex-col gap-0.5">
                        <span className="text-[10px] font-mono text-gx-muted">{key}</span>
                        <input
                          type="password"
                          placeholder="(leave blank for default)"
                          value={pending.envValues[key] ?? ""}
                          onChange={(e) => {
                            const val = e.target.value;
                            setColdRestoreRequests((prev) => {
                              const next = new Map(prev);
                              const req = next.get(pending.id);
                              if (req) {
                                next.set(pending.id, {
                                  ...req,
                                  envValues: { ...req.envValues, [key]: val },
                                });
                              }
                              return next;
                            });
                          }}
                          className="rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-[11px] text-gx-text outline-none focus:border-gx-accent"
                        />
                      </label>
                    ))}
                  </div>
                </div>
              )}

              <div className="flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => discardColdRestore(pending.id)}
                  className="rounded px-3 py-1.5 text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                >
                  Discard
                </button>
                <button
                  type="button"
                  onClick={() => confirmColdRestore(pending.id)}
                  className="rounded bg-gx-accent px-3 py-1.5 text-[11px] font-medium text-white hover:bg-gx-accent/90"
                >
                  Restore
                </button>
              </div>
            </div>
          </div>
        );
      })()}
    </div>
  );
}

// --- Sub-components ---

interface TerminalPaneProps {
  pane: LeafNode; isActive: boolean; theme: "dark" | "light";
  schemeId: string;
  broadcastMode: boolean; allTermIds: string[];
  scrollLocked: boolean; showScrollBtn: boolean;
  scrollbarMode: ScrollbarMode; scrollbarAutohide: boolean;
  onScrollLockToggle: (locked: boolean) => void;
  onScrollToBottom: () => void;
  onSetShowScrollBtn: (show: boolean) => void;
  onActivate: () => void; onSpawnTab: (shell?: string, profile?: TermProfile) => void;
  onKillTab: (tabId: string) => void; onSplitRight: () => void; onSplitDown: () => void;
  onClose?: () => void; onSetActiveTab: (tabId: string) => void;
  onRenameTab: (tabId: string, newTitle: string) => void;
  renamingTab: string | null; setRenamingTab: (id: string | null) => void;
  containerRefs: React.MutableRefObject<Map<string, HTMLDivElement>>;
  terminalsRef: React.MutableRefObject<Map<string, TermInstance>>;
  pendingRestoreSnapshotsRef: React.MutableRefObject<PendingRestoreSnapshotMap>;
  shellIntegrationRef: React.MutableRefObject<Map<string, { state: ShellIntegrationState; jumpToPreviousPrompt: () => void; jumpToNextPrompt: () => void; selectCommandOutput: (markIndex: number) => string | null; dispose: () => void }>>;
  pendingDataRef: React.MutableRefObject<Map<string, string[]>>;
  bookmarkManagersRef: React.MutableRefObject<Map<string, BookmarkManager>>;
  onContextMenu: (e: React.MouseEvent, termId: string) => void;
  availableShells: string[]; shellMenuOpen: boolean; onShellMenuToggle: (open: boolean) => void;
  cmdHistory: string[];
  onAddHistory: (cmd: string) => void;
  repoCwd: string;
  onUpdateTabTitle: (tabId: string, title: string) => void;
  onPaste: (termId: string, content: string) => void;
  tabBadges: Map<string, "success" | "error">;
  onCommandComplete: (tabId: string, completion: CommandCompletion) => void;
  breadcrumbDefault: boolean;
  breadcrumbPerTab: Map<string, boolean>;
  onToggleBreadcrumb: (tabId: string) => void;
  fontConfig: FontConfig;
  /** Per-tab resolved session types, keyed by tab ID. */
  sessionTypes: Map<string, "daemon" | "in-process">;
  /** Whether the parent TerminalView is visible (not hidden by TerminalKeepAlive). */
  visible: boolean;
  backgroundSessionCount: number;
  onOpenSessionManager: () => void;
}

function TerminalPane({
  pane, isActive, theme, schemeId, broadcastMode, allTermIds,
  scrollLocked, showScrollBtn, scrollbarMode, scrollbarAutohide,
  onScrollLockToggle, onScrollToBottom, onSetShowScrollBtn,
  onActivate, onSpawnTab, onKillTab, onClose,
  onSetActiveTab, onRenameTab, renamingTab, setRenamingTab,
  containerRefs, terminalsRef, pendingRestoreSnapshotsRef, shellIntegrationRef, pendingDataRef, bookmarkManagersRef, onContextMenu,
  availableShells, shellMenuOpen, onShellMenuToggle,
  onAddHistory, repoCwd, onUpdateTabTitle, onPaste, tabBadges, onCommandComplete,
  breadcrumbDefault, breadcrumbPerTab, onToggleBreadcrumb, fontConfig, sessionTypes,
  visible, backgroundSessionCount, onOpenSessionManager,
}: TerminalPaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const renameInputRef = useRef<HTMLInputElement>(null);
  const currentLineRef = useRef("");

  // Tracked cwd for the active tab (used by breadcrumb bar).
  const [trackedCwd, setTrackedCwd] = useState<string | null>(repoCwd || null);

  // Per-tab bookmark UI state.
  const [bookmarksPanelOpen, setBookmarksPanelOpen] = useState(false);
  const [bookmarkVersion, setBookmarkVersion] = useState(0);
  const bumpBookmarks = () => setBookmarkVersion((v) => v + 1);
  const [renamingBookmarkId, setRenamingBookmarkId] = useState<string | null>(null);

  // Keep refs in sync with props so xterm.onData closures always
  // read the latest values (avoids stale closure problem).
  const broadcastRef = useRef(broadcastMode);
  const allTermIdsRef = useRef(allTermIds);
  const onPasteRef = useRef(onPaste);
  const visibleRef = useRef(visible);
  useEffect(() => { broadcastRef.current = broadcastMode; }, [broadcastMode]);
  useEffect(() => { allTermIdsRef.current = allTermIds; }, [allTermIds]);
  useEffect(() => { onPasteRef.current = onPaste; }, [onPaste]);
  useEffect(() => { visibleRef.current = visible; }, [visible]);

  useEffect(() => { if (containerRef.current) containerRefs.current.set(pane.id, containerRef.current); return () => { containerRefs.current.delete(pane.id); }; }, [pane.id, containerRefs]);
  useEffect(() => { if (renamingTab && pane.tabs.some((t) => t.id === renamingTab)) setTimeout(() => renameInputRef.current?.focus(), 50); }, [renamingTab, pane.tabs]);

  // Mount xterm instances.
  useEffect(() => {
    if (!pane.activeTab || !containerRef.current) return;
    const container = containerRef.current;
    const replayRestoreThenBufferedData = (id: string, xterm: XTerm) => {
      replayQueuedRestoreSnapshot(pendingRestoreSnapshotsRef.current, id, xterm);
      const pending = pendingDataRef.current.get(id);
      if (pending && pending.length > 0) {
        xterm.write(pending.join(""));
        pendingDataRef.current.delete(id);
      }
    };

    // Hide ALL xterm instances in this pane's container, then show only
    // the active one. This ensures the old terminal is hidden even when
    // switching to a tab that hasn't been mounted yet.
    let reattached = false;
    for (const tab of pane.tabs) {
      const t = terminalsRef.current.get(tab.id);
      if (t) {
        const el = t.xterm.element?.parentElement;
        if (el) {
          // Re-attach orphaned wrappers: if the wrapper isn't inside the
          // current container (e.g., after a split restructured the tree
          // and React remounted this pane), move it back.
          if (!container.contains(el)) {
            container.appendChild(el);
            reattached = true;
          }
          el.style.display = tab.id === pane.activeTab ? "block" : "none";
        }
      }
    }

    // After re-attaching orphaned terminals, refit to match new container size.
    if (reattached) {
      const inst = terminalsRef.current.get(pane.activeTab);
      if (inst) {
        requestAnimationFrame(() => {
          if (container.offsetWidth > 0 && container.offsetHeight > 0) {
            inst.fit.fit();
            replayRestoreThenBufferedData(pane.activeTab!, inst.xterm);
          }
        });
      }
    }

    if (!terminalsRef.current.has(pane.activeTab)) {
      const scrollbackConfig = loadScrollbackConfig();

      const xterm = new XTerm({
        cursorBlink: true,
        cursorStyle: "bar",
        cursorWidth: 2,
        cursorInactiveStyle: "outline",
        fontSize: fontConfig.fontSize,
        fontFamily: buildFontFamily(fontConfig.fontFamily),
        fontWeight: fontConfig.fontWeight,
        fontWeightBold: fontConfig.fontWeightBold,
        lineHeight: fontConfig.lineHeight,
        letterSpacing: fontConfig.letterSpacing,
        // VS Code default — auto-boosts foreground colors against the
        // background when contrast falls below WCAG AA. Makes dim ANSI
        // colors (gray, dim blue, etc.) actually readable.
        minimumContrastRatio: 4.5,
        // Render bold text in the bright variant of its color (legacy
        // terminal convention; iTerm2 / VS Code / orca all enable this).
        drawBoldTextInBrightColors: true,
        // Use xterm's pixel-perfect glyphs for box-drawing and block
        // characters instead of the font's, which often have inconsistent
        // line widths and gaps.
        customGlyphs: true,
        // Rescale wide glyphs (CJK, emoji, powerline symbols) so they
        // don't overflow into adjacent cells.
        rescaleOverlappingGlyphs: true,
        // Smooth scrolling — 125ms feels polished, matches VS Code.
        smoothScrollDuration: 125,
        scrollback: scrollbackConfig.maxLines, allowProposedApi: true,
        theme: resolveTheme(schemeId, theme),
      });

      const fit = new FitAddon(); xterm.loadAddon(fit);
      const search = new SearchAddon(); xterm.loadAddon(search);
      const serialize = new SerializeAddon(); xterm.loadAddon(serialize);
      const unicode11 = new Unicode11Addon(); xterm.loadAddon(unicode11); xterm.unicode.activeVersion = "11";
      xterm.loadAddon(new ClipboardAddon());

      // File path link detection: match patterns like file.rs:42:5 or ./src/main.ts:10
      xterm.loadAddon(new WebLinksAddon((_event, uri) => { void ptyIpc.openUrl(uri); }));
      xterm.registerLinkProvider({
        provideLinks: (bufferLineNumber, callback) => {
          const line = xterm.buffer.active.getLine(bufferLineNumber - 1)?.translateToString() ?? "";
          const links: Array<{ startIndex: number; length: number; tooltip: string; data: string }> = [];
          // Match file:line:col patterns
          const regex = /(?:^|\s|["'(])([.\w/\\-]+\.\w+):(\d+)(?::(\d+))?/g;
          let match;
          while ((match = regex.exec(line)) !== null) {
            const fullMatch = match[0].replace(/^[\s"'(]/, "");
            const startIdx = line.indexOf(fullMatch, match.index);
            links.push({
              startIndex: startIdx,
              length: fullMatch.length,
              tooltip: `Open ${match[1]}:${match[2]}`,
              data: `${match[1]}:${match[2]}${match[3] ? `:${match[3]}` : ""}`,
            });
          }
          callback(links.map((l) => ({
            range: { start: { x: l.startIndex + 1, y: bufferLineNumber }, end: { x: l.startIndex + l.length + 1, y: bufferLineNumber } },
            text: l.data,
            activate: () => {
              // Dispatch event to open file in editor
              window.dispatchEvent(new CustomEvent("crab:open-file-link", { detail: { link: l.data, cwd: repoCwd } }));
            },
          })));
        },
      });

      const wrapper = document.createElement("div");
      wrapper.style.height = "100%"; wrapper.style.width = "100%";
      wrapper.style.position = "absolute";
      wrapper.style.top = "0";
      wrapper.style.left = "0";
      container.appendChild(wrapper);
      xterm.open(wrapper);

      // If this tab was spawned with a startup command (agent preset),
      // show a brief loading indicator so the user gets immediate feedback
      // while the shell initializes and the agent starts up.
      const activeTabObj = pane.tabs.find((t) => t.id === pane.activeTab);
      if (activeTabObj?.startupCommand) {
        const agentName = activeTabObj.title || activeTabObj.startupCommand;
        xterm.write(`\x1b[90m  Starting ${agentName}...\x1b[0m`);
      }

      // Apply font ligature settings.
      applyFontConfig(xterm, fontConfig);

      // WebGL renderer: attach after open() so the canvas is in the DOM.
      const webglState: WebglState = { addon: null, disabledAfterContextLoss: false };
      requestAnimationFrame(() => {
        attachWebgl(xterm, webglState);
      });

      const id = pane.activeTab;

      // --- Paste handling ---
      // xterm.js processes paste events on its internal textarea and emits
      // the pasted text through onData, which then writes to the PTY.
      // We want to route ALL pastes through our paste guard instead.
      //
      // There are two paste paths:
      // A) Keyboard (Cmd+V): we read clipboard via navigator.clipboard.readText()
      //    and write through the paste guard. We must suppress the native paste
      //    event that the browser fires AFTER the keydown so it doesn't also
      //    write through our paste event handler.
      // B) Edit menu / right-click: no keyboard event, just a native paste event.
      //    We handle it in our paste event listener.
      //
      // The flag `keyboardPasteActive` distinguishes path A from path B.
      let keyboardPasteActive = false;

      xterm.attachCustomKeyEventHandler((event) => {
        if (event.type === "keydown" && (event.metaKey || event.ctrlKey) && event.key === "v") {
          // Mark that this paste is keyboard-initiated. The native paste event
          // that follows will be suppressed (not handled again).
          keyboardPasteActive = true;
          // Reset after a generous window — the native paste event fires
          // synchronously after keydown in the same task, but we use 200ms
          // to be safe across different OS/browser timing.
          setTimeout(() => { keyboardPasteActive = false; }, 200);

          // Read clipboard and route through paste guard.
          void navigator.clipboard.readText().then((text) => {
            if (text) onPasteRef.current(id, text);
          });
          return false; // Suppress xterm's keyboard handling
        }

        // Intercept Ctrl+L to perform a soft clear that preserves scrollback.
        // The shell responds to \x0c with \x1b[H\x1b[2J (clear viewport) plus
        // \x1b[3J (erase saved lines / scrollback). We strip \x1b[3J in the
        // onData handler, so the viewport clears but scrollback is preserved.
        // No local manipulation needed — just let the shell handle it naturally.
        // This matches VS Code's terminal behavior.
        if (event.type === "keydown" && event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey && event.key === "l") {
          // VS Code-style soft clear: scroll the current viewport content into
          // the scrollback buffer before the shell clears the screen. This way
          // the user can scroll up to see previous output after Ctrl+L.
          const buffer = xterm.buffer.active;
          // Find the last row in the viewport that has content.
          let lastContentRow = -1;
          for (let row = xterm.rows - 1; row >= 0; row--) {
            const line = buffer.getLine(buffer.baseY + row);
            if (line && line.translateToString(true).trim().length > 0) {
              lastContentRow = row;
              break;
            }
          }
          // Write newlines to push viewport content into scrollback.
          // We write from the current cursor position down to the bottom,
          // which scrolls all content above into the scrollback buffer.
          if (lastContentRow >= 0) {
            // Move cursor to the last content row, then write enough newlines
            // to push everything above into scrollback.
            const newlinesToWrite = xterm.rows;
            xterm.write("\r\n".repeat(newlinesToWrite));
          }
          // Now send Ctrl+L to the shell so it redraws the prompt at the top.
          // The shell's \x1b[3J response is stripped by our onData handler,
          // and \x1b[2J will just clear the (now empty) viewport.
          if (activeTabObj?.sshRemote) {
            void sshTermIpc.write(id, "\x0c");
          } else if (broadcastRef.current) {
            for (const tid of allTermIdsRef.current) { void ptyIpc.write(tid, "\x0c"); }
          } else {
            void ptyIpc.write(id, "\x0c");
          }
          return false; // Suppress xterm's default handling of Ctrl+L
        }

        return true; // Allow all other key events
      });

      // Intercept native paste events on xterm's element and its textarea.
      // This prevents xterm's core from processing the paste through onData.
      // For keyboard-initiated pastes (Cmd+V), we already handled it above
      // so we just suppress the event. For Edit-menu pastes, we handle here.
      const pasteHandler = (e: ClipboardEvent) => {
        // Always suppress so xterm's core never processes this paste.
        e.preventDefault();
        e.stopPropagation();
        e.stopImmediatePropagation();

        if (keyboardPasteActive) {
          // Keyboard-initiated paste — already handled via clipboard.readText().
          // Just suppress the event, don't write again.
          return;
        }

        // Edit-menu / right-click paste — handle through paste guard.
        const text = e.clipboardData?.getData("text/plain");
        if (text) {
          onPasteRef.current(id, text);
        }
      };
      // Listen in capture phase on the xterm container to catch before children.
      xterm.element?.addEventListener("paste", pasteHandler as EventListener, true);
      // Also directly on the textarea as a belt-and-suspenders approach.
      const xtermTextarea = xterm.element?.querySelector("textarea.xterm-helper-textarea");
      if (xtermTextarea) {
        xtermTextarea.addEventListener("paste", pasteHandler as EventListener, true);
      }

      // Enforce memory cap: periodically check buffer size and trim if needed.
      const memoryCapBytes = scrollbackConfig.memoryCapBytes;
      const memoryCheckInterval = setInterval(() => {
        const bufferLines = xterm.buffer.active.length;
        const estimated = estimateMemory(bufferLines);
        if (estimated > memoryCapBytes) {
          // Calculate how many lines to keep within the memory cap
          const targetLines = Math.floor(memoryCapBytes / (estimated / bufferLines));
          const linesToTrim = bufferLines - targetLines;
          if (linesToTrim > 0) {
            // xterm.js clear() removes scrollback; we use the internal trim approach
            // by reducing the scrollback option which triggers a trim on next write
            xterm.options.scrollback = Math.max(targetLines, 1000);
          }
        }
      }, 5000); // Check every 5 seconds
      // Wire data flow with broadcast support.
      xterm.onData((data) => {
        // Track command history: detect Enter key
        if (data === "\r" || data === "\n") {
          const cmd = currentLineRef.current.trim();
          if (cmd.length > 2) onAddHistory(cmd);
          currentLineRef.current = "";
        } else if (data === "\x7f") {
          // Backspace
          currentLineRef.current = currentLineRef.current.slice(0, -1);
        } else if (data.length === 1 && data.charCodeAt(0) >= 32) {
          currentLineRef.current += data;
        }

        if (activeTabObj?.sshRemote) {
          // SSH remote terminal — route through SSH IPC
          void sshTermIpc.write(id, data);
        } else if (broadcastRef.current) {
          // Send to ALL terminals
          for (const tid of allTermIdsRef.current) { void ptyIpc.write(tid, data); }
        } else {
          void ptyIpc.write(id, data);
        }
      });
      xterm.onResize(({ cols, rows }) => {
        if (activeTabObj?.sshRemote) {
          void sshTermIpc.resize(id, cols, rows);
        } else {
          void ptyIpc.resize(id, cols, rows);
        }
      });

      // Scroll tracking for scroll-lock and scroll-to-bottom button.
      xterm.onScroll(() => {
        const atBottom = xterm.buffer.active.viewportY >= xterm.buffer.active.baseY;
        onSetShowScrollBtn(!atBottom);
      });

      terminalsRef.current.set(id, { xterm, fit, search, serialize, webglState, memoryCheckInterval: memoryCheckInterval });

      // Attach shell integration (OSC 7 cwd tracking).
      const si = attachShellIntegration(xterm);
      shellIntegrationRef.current.set(id, si);

      // Click-to-move cursor within prompt region.
      // When the user clicks inside an active prompt and the shell is at a
      // prompt, calculate the column difference and send arrow key sequences
      // to reposition the cursor.
      xterm.element?.addEventListener("mousedown", (e: MouseEvent) => {
        const currentSi = shellIntegrationRef.current.get(id);
        if (!currentSi || !currentSi.state.atPrompt) return;

        // Only handle left-click without modifiers (avoid interfering with selection).
        if (e.button !== 0 || e.shiftKey || e.metaKey || e.ctrlKey || e.altKey) return;

        // Use a short timeout to let xterm process the click first (for selection detection).
        setTimeout(() => {
          // If the user is making a selection, don't interfere.
          if (xterm.hasSelection()) return;

          const buffer = xterm.buffer.active;
          const cursorCol = buffer.cursorX;

          // Get the cell position from the mouse coordinates using xterm's
          // internal coordinate system. We calculate the target column from
          // the click position relative to the terminal element.
          const termElement = xterm.element;
          if (!termElement) return;

          const rect = termElement.getBoundingClientRect();
          // xterm renders cells in a grid; calculate cell dimensions.
          const cellWidth = termElement.querySelector(".xterm-rows")?.clientWidth
            ? (termElement.querySelector(".xterm-rows")!.clientWidth / xterm.cols)
            : (rect.width / xterm.cols);

          const relativeX = e.clientX - rect.left;
          const targetCol = Math.floor(relativeX / cellWidth);
          const clampedTarget = Math.max(0, Math.min(targetCol, xterm.cols - 1));

          const diff = clampedTarget - cursorCol;
          if (diff === 0) return;

          // Send arrow key sequences to PTY.
          const arrowKey = diff > 0 ? "\x1b[C" : "\x1b[D"; // Right : Left
          const count = Math.abs(diff);
          const sequence = arrowKey.repeat(count);
          void ptyIpc.write(id, sequence);
        }, 50);
      });

    }

    // Focus the active terminal so keystrokes route to the correct PTY.
    // Without this, the previously focused xterm's hidden textarea still
    // captures input even when its wrapper is display:none.
    const activeInst = terminalsRef.current.get(pane.activeTab);
    if (activeInst) {
      // Blur all other terminals first to ensure clean focus transfer.
      for (const [tid, t] of terminalsRef.current.entries()) {
        if (tid !== pane.activeTab) {
          t.xterm.blur();
        }
      }
      setTimeout(() => activeInst.xterm.focus(), 0);
      requestAnimationFrame(() => {
        if (!visibleRef.current) return;
        if (container.offsetWidth <= 0 || container.offsetHeight <= 0) return;
        activeInst.fit.fit();
        replayRestoreThenBufferedData(pane.activeTab!, activeInst.xterm);
      });
    }

    const obs = new ResizeObserver(() => { if (!visibleRef.current) return; if (!pane.activeTab) return; const t = terminalsRef.current.get(pane.activeTab); if (t && container.offsetWidth > 0 && container.offsetHeight > 0) t.fit.fit(); });
    obs.observe(container);
    return () => obs.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pane.activeTab, theme]);

  // Reapply the chosen scheme whenever the user switches it or the app's
  // light/dark mode flips. xterm merges whatever keys we pass, so we hand
  // over the full ITheme to avoid stale colours leaking through.
  useEffect(() => {
    const palette = resolveTheme(schemeId, theme);
    for (const inst of terminalsRef.current.values()) {
      inst.xterm.options.theme = palette;
    }
  }, [schemeId, theme]);

  // Poll shell integration state for cwd changes and update tab titles.
  // The shell integration state is a plain object (no observable), so we
  // poll at a reasonable interval to detect OSC 7 updates.
  const lastCwdRef = useRef<Map<string, string | null>>(new Map());
  const lastDurationRef = useRef<Map<string, number | null>>(new Map());
  const durationTimerRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  useEffect(() => {
    // Poll at 2s instead of 500ms — cwd changes are infrequent and
    // the 4x reduction significantly cuts idle CPU. Skip entirely
    // when the pane is not visible.
    const interval = setInterval(() => {
      if (!visible) return;
      for (const tab of pane.tabs) {
        const si = shellIntegrationRef.current.get(tab.id);
        if (!si) continue;

        // Check for command completion — show duration briefly in tab title.
        const prevDuration = lastDurationRef.current.get(tab.id) ?? null;
        const currentDuration = si.state.lastCommandDuration;
        if (currentDuration !== null && currentDuration !== prevDuration) {
          lastDurationRef.current.set(tab.id, currentDuration);
          const exitCode = si.state.lastCommandExitCode ?? 0;
          const icon = exitCode === 0 ? "✓" : "✗";
          const durationStr = formatDuration(currentDuration);
          onUpdateTabTitle(tab.id, `${icon} ${durationStr}`);

          // Notify parent about command completion for badges/notifications.
          const isFocused = pane.activeTab === tab.id;
          onCommandComplete(tab.id, {
            command: si.state.runningCommand ?? "Command",
            exitCode,
            durationMs: currentDuration,
            terminalId: tab.id,
            tabFocused: isFocused,
          });

          // Clear any existing revert timer for this tab.
          const existingTimer = durationTimerRef.current.get(tab.id);
          if (existingTimer) clearTimeout(existingTimer);

          // Revert to cwd basename after 4 seconds.
          const timer = setTimeout(() => {
            const currentSi = shellIntegrationRef.current.get(tab.id);
            const cwd = currentSi?.state.cwd;
            if (cwd) {
              const basename = cwd.split("/").filter(Boolean).pop() ?? cwd;
              onUpdateTabTitle(tab.id, basename);
            }
            durationTimerRef.current.delete(tab.id);
          }, 4000);
          durationTimerRef.current.set(tab.id, timer);
        }

        // Update tab title to show the directory basename (only if no duration is being shown).
        const currentCwd = si.state.cwd;
        const lastCwd = lastCwdRef.current.get(tab.id) ?? null;
        if (currentCwd && currentCwd !== lastCwd) {
          lastCwdRef.current.set(tab.id, currentCwd);
          // Only update if we're not currently showing a duration indicator.
          if (!durationTimerRef.current.has(tab.id)) {
            const basename = currentCwd.split("/").filter(Boolean).pop() ?? currentCwd;
            onUpdateTabTitle(tab.id, basename);
          }
        }

        // Update breadcrumb bar cwd for the active tab.
        if (tab.id === pane.activeTab && currentCwd) {
          setTrackedCwd(currentCwd);
        }
      }
    }, 2000);
    return () => {
      clearInterval(interval);
      // Clean up any pending duration revert timers.
      for (const timer of durationTimerRef.current.values()) {
        clearTimeout(timer);
      }
    };
  }, [pane.tabs, pane.activeTab, visible, shellIntegrationRef, onUpdateTabTitle, onCommandComplete]);

  // Sync trackedCwd when the active tab changes.
  useEffect(() => {
    if (!pane.activeTab) { setTrackedCwd(null); return; }
    const si = shellIntegrationRef.current.get(pane.activeTab);
    setTrackedCwd(si?.state.cwd ?? (repoCwd || null));
  }, [pane.activeTab, shellIntegrationRef, repoCwd]);

  // Handle drag-and-drop files into terminal (Task 26.2).
  const [dragOver, setDragOver] = useState(false);

  function handleDrop(e: React.DragEvent) {
    e.preventDefault();
    setDragOver(false);
    if (!pane.activeTab) return;
    // Use enhanced path extraction with proper quoting.
    const paths = extractDropPaths(e.nativeEvent as DragEvent);
    if (paths) {
      void ptyIpc.write(pane.activeTab, paths + " ");
    }
  }

  function handleDragOver(e: React.DragEvent) {
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
    setDragOver(true);
  }

  function handleDragLeave() {
    setDragOver(false);
  }

  return (
    <div className={clsx(
      "relative flex h-full min-w-0 min-h-0 flex-col",
      isActive && "ring-1 ring-inset ring-gx-accent/50",
      !isActive && "ring-0 opacity-90",
    )} onClick={onActivate}>
      {/* Active pane indicator bar */}
      <div className={clsx(
        "absolute top-0 left-0 right-0 h-[2px] z-10 transition-colors",
        isActive ? "bg-gx-accent" : "bg-transparent",
      )} />
      {/* Tab bar */}
      <div className="flex shrink-0 items-center border-b border-gx-border bg-gx-surface">
        <TerminalTabStrip
          tabs={pane.tabs as TermTab[]}
          activeTab={pane.activeTab}
          tabBadges={tabBadges}
          sessionTypes={sessionTypes}
          renamingTab={renamingTab}
          renameInputRef={renameInputRef}
          onSetActiveTab={onSetActiveTab}
          onRenameTab={onRenameTab}
          setRenamingTab={setRenamingTab}
          onCloseTab={onKillTab}
          onOpenSessionManager={onOpenSessionManager}
          backgroundSessionCount={backgroundSessionCount}
        />
        <PresetDropdown
          onSelect={(cmd) => onSpawnTab(undefined, { name: cmd.name, shell: "", cwd: cmd.cwd, env: cmd.env, startupCommand: cmd.command })}
          onSpawnPlain={() => onSpawnTab()}
        />


        <div className="relative ml-auto flex items-center gap-1 pr-2">
          {/* Scroll lock */}
          <button type="button" onClick={() => onScrollLockToggle(!scrollLocked)}
            className={clsx("flex items-center rounded px-1 py-0.5", scrollLocked ? "text-gx-warn" : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover")}
            title={scrollLocked ? "Scroll locked — new output won't auto-scroll" : "Click to lock scroll"}>
            {scrollLocked ? <Lock size={10} /> : <Unlock size={10} />}
          </button>

          {/* Per-pane terminal actions */}
          <button type="button" onClick={() => { const si = pane.activeTab ? shellIntegrationRef.current.get(pane.activeTab) : null; if (si) si.jumpToPreviousPrompt(); }}
            className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Previous prompt (⌘↑)">
            <ArrowUp size={10} />
          </button>
          <button type="button" onClick={() => { const si = pane.activeTab ? shellIntegrationRef.current.get(pane.activeTab) : null; if (si) si.jumpToNextPrompt(); }}
            className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Next prompt (⌘↓)">
            <ArrowDown size={10} />
          </button>
          <button type="button" onClick={() => {
            if (!pane.activeTab) return;
            const si = shellIntegrationRef.current.get(pane.activeTab);
            if (!si) return;
            const marks = si.state.marks;
            let lastExecIndex = -1;
            for (let i = marks.length - 1; i >= 0; i--) { if (marks[i].type === "command-executed") { lastExecIndex = i; break; } }
            if (lastExecIndex >= 0) { const text = si.selectCommandOutput(lastExecIndex); if (text) void navigator.clipboard.writeText(stripAnsiCodes(text)); }
          }} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Copy last command output">
            <ClipboardCopy size={10} />
          </button>
          <button type="button" onClick={() => {
            if (!pane.activeTab) return;
            const si = shellIntegrationRef.current.get(pane.activeTab);
            const term = terminalsRef.current.get(pane.activeTab);
            if (!si || !term) return;
            const cmd = getLastCommand(term.xterm, si.state.marks);
            if (cmd) void ptyIpc.write(pane.activeTab, cmd + "\r");
          }} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Retry last command">
            <RotateCcw size={10} />
          </button>
          <button type="button" onClick={() => {
            if (!pane.activeTab) return;
            const t = terminalsRef.current.get(pane.activeTab);
            if (!t) return; t.xterm.selectAll();
            const text = t.xterm.getSelection(); t.xterm.clearSelection();
            if (text) void navigator.clipboard.writeText(text);
          }} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Copy all output">
            <Copy size={10} />
          </button>

          {/* Per-tab breadcrumb toggle */}
          <button type="button" onClick={() => { if (pane.activeTab) onToggleBreadcrumb(pane.activeTab); }}
            className={clsx(
              "flex items-center rounded px-1 py-0.5",
              pane.activeTab && (breadcrumbPerTab.get(pane.activeTab) ?? breadcrumbDefault)
                ? "text-gx-accent" : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
            )}
            title={pane.activeTab && (breadcrumbPerTab.get(pane.activeTab) ?? breadcrumbDefault) ? "Hide breadcrumb (this tab)" : "Show breadcrumb (this tab)"}
          >
            <ChevronDown size={10} className="-rotate-90" />
          </button>

          {/* Per-tab bookmark: add bookmark at current cursor position */}
          <button type="button" onClick={() => {
            if (!pane.activeTab) return;
            const term = terminalsRef.current.get(pane.activeTab);
            if (!term) return;
            const line = (term.xterm.buffer?.active?.baseY ?? 0) + (term.xterm.buffer?.active?.cursorY ?? 0);
            let mgr = bookmarkManagersRef.current.get(pane.activeTab);
            if (!mgr) { mgr = new BookmarkManager(); bookmarkManagersRef.current.set(pane.activeTab, mgr); }
            mgr.add(line);
            bumpBookmarks();
            setBookmarksPanelOpen(true);
          }} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Add bookmark at cursor">
            <Bookmark size={10} />
          </button>
          {/* Toggle bookmarks panel */}
          {(() => { const mgr = pane.activeTab ? bookmarkManagersRef.current.get(pane.activeTab) : null; const count = mgr?.count ?? 0; return count > 0 ? (
            <button type="button" onClick={() => setBookmarksPanelOpen((v) => !v)}
              className={clsx("flex items-center rounded px-1 py-0.5 text-[9px]",
                bookmarksPanelOpen ? "text-gx-accent" : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover")}
              title={`${count} bookmark${count > 1 ? "s" : ""}`}
            >{count}</button>
          ) : null; })()}

          {/* Shell selector */}
          <button type="button" onClick={() => onShellMenuToggle(!shellMenuOpen)} className="flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Select shell">
            <TerminalIcon size={10} /><DropdownIcon size={8} />
          </button>
          {shellMenuOpen && (
            <div className="absolute right-0 top-full z-50 mt-1 min-w-[140px] rounded border border-gx-border bg-gx-surface py-1 shadow-lg">
              {availableShells.map((sh) => (
                <button key={sh} type="button" onClick={() => { onShellMenuToggle(false); onSpawnTab(sh); }} className="block w-full px-3 py-1 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover">
                  {sh.split("/").pop()}<span className="ml-2 text-gx-muted">{sh}</span>
                </button>
              ))}
            </div>
          )}

          {onClose && <button type="button" onClick={onClose} className="flex items-center rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-error" title="Close pane"><X size={12} /></button>}
        </div>
      </div>

      {/* Breadcrumb bar — shows tracked cwd above terminal viewport (per-tab toggle) */}
      {(() => {
        const tabId = pane.activeTab;
        const enabled = tabId ? (breadcrumbPerTab.get(tabId) ?? breadcrumbDefault) : breadcrumbDefault;
        return enabled && trackedCwd ? (
          <TerminalBreadcrumbBar
            cwd={trackedCwd}
            onNavigate={(path) => {
              if (pane.activeTab) {
                void ptyIpc.write(pane.activeTab, `cd ${path.includes(" ") ? `"${path}"` : path}\n`);
              }
            }}
          />
        ) : null;
      })()}

      {/* Per-tab bookmarks panel */}
      {bookmarksPanelOpen && pane.activeTab && (() => {
        const mgr = bookmarkManagersRef.current.get(pane.activeTab!);
        const bookmarks = mgr?.bookmarks ?? [];
        // Use bookmarkVersion to track changes
        void bookmarkVersion;
        return bookmarks.length > 0 ? (
          <div className="shrink-0 border-b border-gx-border bg-gx-surface px-2 py-1 max-h-24 overflow-y-auto">
            <div className="flex items-center justify-between mb-1">
              <span className="text-[9px] text-gx-muted uppercase tracking-wider">Bookmarks</span>
              <button type="button" onClick={() => setBookmarksPanelOpen(false)} className="text-gx-muted hover:text-gx-text"><X size={9} /></button>
            </div>
            {bookmarks.map((bm) => (
              <div key={bm.id} className="group flex items-center gap-1 py-0.5 text-[10px]">
                {renamingBookmarkId === bm.id ? (
                  <input
                    autoFocus
                    type="text"
                    defaultValue={bm.label}
                    className="flex-1 bg-transparent text-[10px] text-gx-text outline-none border-b border-gx-accent px-1"
                    onBlur={(e) => { if (mgr) { mgr.rename(bm.id, e.target.value || bm.label); bumpBookmarks(); } setRenamingBookmarkId(null); }}
                    onKeyDown={(e) => { if (e.key === "Enter") { if (mgr) { mgr.rename(bm.id, (e.target as HTMLInputElement).value || bm.label); bumpBookmarks(); } setRenamingBookmarkId(null); } if (e.key === "Escape") setRenamingBookmarkId(null); }}
                  />
                ) : (
                  <button
                    type="button"
                    onClick={() => { const t = terminalsRef.current.get(pane.activeTab!); if (t) t.xterm.scrollToLine(bm.line); }}
                    onDoubleClick={() => setRenamingBookmarkId(bm.id)}
                    className="flex-1 text-left truncate text-gx-text hover:text-gx-accent"
                    title={`Line ${bm.line} — double-click to rename`}
                  >
                    {bm.label}
                  </button>
                )}
                <button
                  type="button"
                  onClick={() => { if (mgr) { mgr.remove(bm.id); bumpBookmarks(); } }}
                  className="opacity-0 group-hover:opacity-100 text-gx-muted hover:text-gx-error"
                  title="Remove bookmark"
                >
                  <X size={8} />
                </button>
              </div>
            ))}
          </div>
        ) : (
          <div className="shrink-0 border-b border-gx-border bg-gx-surface px-2 py-1.5 text-[10px] text-gx-muted flex items-center justify-between">
            <span>No bookmarks — click <Bookmark size={9} className="inline" /> to add one</span>
            <button type="button" onClick={() => setBookmarksPanelOpen(false)} className="text-gx-muted hover:text-gx-text"><X size={9} /></button>
          </div>
        );
      })()}

      {/* Terminal container with drag-and-drop */}
      <div ref={containerRef} className={clsx(
        "min-h-0 min-w-0 flex-1 overflow-hidden relative",
        `scrollbar-mode-${scrollbarMode}`,
        scrollbarMode === "native" && scrollbarAutohide && "scrollbar-autohide",
        dragOver && "ring-2 ring-inset ring-gx-accent/50",
      )}
        onContextMenu={(e) => { if (pane.activeTab) onContextMenu(e, pane.activeTab); }}
        onDrop={handleDrop} onDragOver={handleDragOver} onDragLeave={handleDragLeave}
      >
        {!pane.activeTab && (
          <div className="absolute inset-0 z-10 flex items-center justify-center bg-gx-bg-sub">
            <div className="flex items-center gap-2 rounded-md border border-gx-border bg-gx-surface px-3 py-2 text-[11px] text-gx-muted">
              <button
                type="button"
                onClick={() => onSpawnTab()}
                className="rounded bg-gx-accent px-2 py-1 font-medium text-white hover:bg-gx-accent/90"
              >
                New terminal
              </button>
              <button
                type="button"
                onClick={onOpenSessionManager}
                className="rounded border border-gx-border px-2 py-1 text-gx-text hover:bg-gx-surface-hover"
              >
                Sessions{backgroundSessionCount > 0 ? ` (${backgroundSessionCount})` : ""}
              </button>
            </div>
          </div>
        )}
        {/* Drop indicator overlay (Task 26.2) */}
        {dragOver && (
          <div className="absolute inset-0 z-30 flex items-center justify-center bg-gx-accent/5 pointer-events-none">
            <span className="rounded-lg border-2 border-dashed border-gx-accent/40 px-4 py-2 text-[12px] text-gx-accent">
              Drop to insert path
            </span>
          </div>
        )}
      </div>

      {/* Terminal status bar (Task 26.5) */}
      <TerminalStatusBar
        shellName={pane.tabs.find((t) => t.id === pane.activeTab)?.shell?.split("/").pop() ?? "zsh"}
        cwd={trackedCwd}
        shellIntegration={pane.activeTab ? shellIntegrationRef.current.get(pane.activeTab)?.state ?? null : null}
        scrollbackLines={pane.activeTab ? (terminalsRef.current.get(pane.activeTab)?.xterm.buffer?.active?.length ?? 0) : 0}
        maxScrollback={10000}
        connected={true}
        agentType={(() => {
          if (!pane.activeTab) return null;
          const si = shellIntegrationRef.current.get(pane.activeTab);
          if (!si?.state.runningCommand) return null;
          const detection = detectAgent(si.state.runningCommand);
          return detection?.type ?? null;
        })()}
        agentDuration={(() => {
          if (!pane.activeTab) return null;
          const si = shellIntegrationRef.current.get(pane.activeTab);
          if (!si?.state.commandStartedAt) return null;
          return Date.now() - si.state.commandStartedAt;
        })()}
        onCwdClick={(cwd) => {
          // Navigate to cwd in file tree (placeholder — emits to parent).
          if (pane.activeTab) {
            void ptyIpc.write(pane.activeTab, `cd "${cwd}"\n`);
          }
        }}
        onShellClick={() => onShellMenuToggle(!shellMenuOpen)}
      />

      {/* Scroll to bottom button */}
      {showScrollBtn && (
        <button type="button" onClick={onScrollToBottom}
          className="absolute bottom-8 right-4 z-20 flex items-center gap-1 rounded-full bg-gx-surface border border-gx-border px-2 py-1 text-[10px] text-gx-muted shadow-lg hover:text-gx-text hover:bg-gx-surface-hover transition"
          title="Scroll to bottom">
          <ArrowDownToLine size={12} /> Bottom
        </button>
      )}
    </div>
  );
}

function TerminalTabStrip({
  tabs,
  activeTab,
  tabBadges,
  sessionTypes,
  renamingTab,
  renameInputRef,
  onSetActiveTab,
  onRenameTab,
  setRenamingTab,
  onCloseTab,
  onOpenSessionManager,
  backgroundSessionCount,
}: {
  tabs: TermTab[];
  activeTab: string | null;
  tabBadges: Map<string, "success" | "error">;
  sessionTypes: Map<string, "daemon" | "in-process">;
  renamingTab: string | null;
  renameInputRef: React.RefObject<HTMLInputElement>;
  onSetActiveTab: (tabId: string) => void;
  onRenameTab: (tabId: string, newTitle: string) => void;
  setRenamingTab: (id: string | null) => void;
  onCloseTab: (tabId: string) => void;
  onOpenSessionManager: () => void;
  backgroundSessionCount: number;
}) {
  const scrollerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!activeTab || !scrollerRef.current) return;
    const el = scrollerRef.current.querySelector<HTMLElement>(`[data-tab-id="${CSS.escape(activeTab)}"]`);
    el?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [activeTab, tabs.length]);

  return (
    <div className="relative flex min-w-0 flex-1 items-stretch overflow-hidden">
      <div
        ref={scrollerRef}
        className="flex min-w-0 flex-1 items-stretch overflow-x-auto overflow-y-hidden [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
        onWheel={(e) => {
          if (!scrollerRef.current) return;
          if (Math.abs(e.deltaY) <= Math.abs(e.deltaX)) return;
          scrollerRef.current.scrollLeft += e.deltaY;
        }}
      >
        {tabs.map((t) => {
          const sType = sessionTypes.get(t.id) ?? t.resolvedSessionType;
          const isDaemon = sType === "daemon";
          return (
            <div
              key={t.id}
              data-tab-id={t.id}
              onClick={() => onSetActiveTab(t.id)}
              className={clsx(
                "group flex h-7 min-w-[116px] max-w-[190px] shrink-0 cursor-pointer items-center gap-1 border-r border-gx-border px-2 text-[11px]",
                activeTab === t.id
                  ? "bg-gx-bg-sub text-gx-text"
                  : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
              )}
              title={isDaemon ? "Daemon terminal - survives app restart" : sType === "in-process" ? "Local in-process terminal" : undefined}
            >
              {activeTab !== t.id && tabBadges.has(t.id) && (
                <span
                  className={clsx(
                    "inline-block h-2 w-2 shrink-0 rounded-full",
                    tabBadges.get(t.id) === "success" ? "bg-green-500" : "bg-red-500",
                  )}
                  aria-label={tabBadges.get(t.id) === "success" ? "Command succeeded" : "Command failed"}
                />
              )}
              {isDaemon && (
                <Circle size={7} className="shrink-0 fill-gx-accent text-gx-accent" aria-label="Daemon terminal" />
              )}
              {t.sshRemote && (
                <span
                  className="inline-flex max-w-[80px] shrink-0 items-center truncate rounded px-1 py-0.5 text-[9px] font-medium text-white"
                  style={{ backgroundColor: t.sshRemote.colour || "#6366f1" }}
                  aria-label={`Remote terminal: ${t.sshRemote.hostname}`}
                >
                  {t.sshRemote.hostname}
                </span>
              )}
              {t.coldRestored && (
                <span
                  className="inline-flex shrink-0 items-center rounded px-1 py-0.5 text-[8px] font-medium leading-none bg-gx-warn/15 text-gx-warn"
                  title={`Restored from ${new Date(t.coldRestored.savedAt).toLocaleString()}`}
                >
                  Restored
                </span>
              )}
              {renamingTab === t.id ? (
                <input
                  ref={renameInputRef}
                  type="text"
                  defaultValue={t.title}
                  className="min-w-0 flex-1 bg-transparent text-[11px] text-gx-text outline-none border-b border-gx-accent"
                  onClick={(e) => e.stopPropagation()}
                  onBlur={(e) => onRenameTab(t.id, e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") onRenameTab(t.id, (e.target as HTMLInputElement).value);
                    if (e.key === "Escape") setRenamingTab(null);
                  }}
                />
              ) : (
                <button
                  type="button"
                  onClick={(e) => { e.stopPropagation(); onSetActiveTab(t.id); }}
                  onDoubleClick={() => setRenamingTab(t.id)}
                  className="min-w-0 flex-1 truncate text-left"
                  title="Double-click to rename"
                >
                  {t.title}
                </button>
              )}
              <button
                type="button"
                onClick={(e) => { e.stopPropagation(); onCloseTab(t.id); }}
                className={clsx(
                  "shrink-0 opacity-0 transition group-hover:opacity-100",
                  isDaemon ? "hover:text-gx-accent" : "hover:text-gx-error",
                )}
                aria-label={isDaemon ? "Hide daemon terminal" : "Close terminal"}
                title={isDaemon ? "Hide tab - session keeps running" : "Close terminal"}
              >
                {isDaemon ? <EyeOff size={10} /> : <X size={10} />}
              </button>
            </div>
          );
        })}
      </div>
      <button
        type="button"
        onClick={onOpenSessionManager}
        className={clsx(
          "flex h-7 shrink-0 items-center gap-1 border-l border-gx-border px-2 text-[10px]",
          backgroundSessionCount > 0
            ? "text-gx-accent hover:bg-gx-accent/10"
            : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
        )}
        title="Open terminal session manager"
        aria-label="Open terminal session manager"
      >
        <MoreHorizontal size={12} />
      </button>
    </div>
  );
}

function TerminalSessionManager({
  open,
  side,
  sessions,
  openTabs,
  openTerminalIds,
  activeTabId,
  pinnedSessionIds,
  search,
  onSearchChange,
  onSideChange,
  onClose,
  onRefresh,
  onAttach,
  onOpenInSplit,
  onFocusTab,
  onHide,
  onKill,
  onTogglePin,
}: {
  open: boolean;
  side: TerminalSessionSidebarSide;
  sessions: DaemonSessionSummary[];
  openTabs: Array<{ paneId: string; tab: TermTab }>;
  openTerminalIds: Set<string>;
  activeTabId: string | null;
  pinnedSessionIds: Set<string>;
  search: string;
  onSearchChange: (value: string) => void;
  onSideChange: (side: TerminalSessionSidebarSide) => void;
  onClose: () => void;
  onRefresh: () => void;
  onAttach: (session: DaemonSessionSummary) => void;
  onOpenInSplit: (session: DaemonSessionSummary) => void;
  onFocusTab: (tabId: string) => void;
  onHide: (paneId: string, tabId: string) => void;
  onKill: (session: DaemonSessionSummary) => void;
  onTogglePin: (sessionId: string) => void;
}) {
  if (!open) return null;

  const sessionById = new Map(sessions.map((session) => [session.id, session]));
  const query = search.trim().toLowerCase();
  const matches = (text: string) => !query || text.toLowerCase().includes(query);
  const visibleOpenTabs = openTabs.filter(({ tab }) => {
    const session = sessionById.get(tab.id);
    return matches(`${tab.title} ${session?.cwd ?? ""} ${session?.initialCwd ?? ""} ${session?.agentMeta?.agentType ?? ""}`);
  });
  const background = sessions.filter((session) => !openTerminalIds.has(session.id) && matches(sessionSearchText(session)));
  const pinned = background.filter((session) => pinnedSessionIds.has(session.id));
  const agentSessions = background.filter((session) => !pinnedSessionIds.has(session.id) && session.agentMeta);
  const shellSessions = background.filter((session) => !pinnedSessionIds.has(session.id) && !session.agentMeta);

  return (
    <aside
      className={clsx(
        "flex h-full w-[clamp(300px,24vw,420px)] shrink-0 flex-col bg-gx-surface",
        side === "left" ? "border-r border-gx-border" : "border-l border-gx-border",
      )}
      aria-label="Terminal sessions"
    >
        <div className="flex shrink-0 items-center gap-2 border-b border-gx-border px-3 py-2">
          <div className="min-w-0 flex-1">
            <div className="text-[12px] font-semibold text-gx-text">Terminal Sessions</div>
            <div className="text-[10px] text-gx-muted">
              {sessions.length} daemon session{sessions.length === 1 ? "" : "s"} · {openTabs.length} open tab{openTabs.length === 1 ? "" : "s"}
            </div>
          </div>
          <div className="flex shrink-0 items-center rounded border border-gx-border bg-gx-bg-sub p-0.5" aria-label="Pin sidebar side">
            <button
              type="button"
              onClick={() => onSideChange("left")}
              className={clsx(
                "rounded px-1 py-0.5",
                side === "left" ? "bg-gx-accent text-white" : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
              )}
              title="Pin sidebar to left"
              aria-label="Pin sidebar to left"
              aria-pressed={side === "left"}
            >
              <PanelLeftOpen size={12} />
            </button>
            <button
              type="button"
              onClick={() => onSideChange("right")}
              className={clsx(
                "rounded px-1 py-0.5",
                side === "right" ? "bg-gx-accent text-white" : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
              )}
              title="Pin sidebar to right"
              aria-label="Pin sidebar to right"
              aria-pressed={side === "right"}
            >
              <PanelRightOpen size={12} />
            </button>
          </div>
          <button type="button" onClick={onRefresh} className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" title="Refresh sessions">
            <RotateCcw size={13} />
          </button>
          <button type="button" onClick={onClose} className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text" aria-label="Close">
            <X size={14} />
          </button>
        </div>

        <div className="shrink-0 border-b border-gx-border px-3 py-2">
          <div className="flex items-center gap-2 rounded border border-gx-border bg-gx-bg-sub px-2 py-1.5">
            <Search size={13} className="shrink-0 text-gx-muted" />
            <input
              value={search}
              onChange={(e) => onSearchChange(e.target.value)}
              placeholder="Search sessions by title, cwd, or agent"
              className="min-w-0 flex-1 bg-transparent text-[11px] text-gx-text placeholder:text-gx-muted outline-none"
              autoFocus
            />
          </div>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-2 py-2">
          <SessionGroup title="Open Tabs" empty="No open terminal tabs." count={visibleOpenTabs.length}>
            {visibleOpenTabs.map(({ paneId, tab }) => {
              const session = sessionById.get(tab.id);
              const isDaemon = (session != null) || tab.resolvedSessionType === "daemon";
              return (
                <SessionRow
                  key={tab.id}
                  title={tab.title}
                  subtitle={session ? sessionSubtitle(session) : tab.shell || "Local terminal"}
                  status={tab.id === activeTabId ? "Active" : isDaemon ? "Open" : "Local"}
                  pinned={pinnedSessionIds.has(tab.id)}
                  onPrimary={() => onFocusTab(tab.id)}
                  primaryLabel="Focus"
                  actions={(
                    <>
                      {isDaemon && (
                        <IconButton title="Hide tab" onClick={() => onHide(paneId, tab.id)}>
                          <EyeOff size={12} />
                        </IconButton>
                      )}
                      {session && (
                        <IconButton title={pinnedSessionIds.has(tab.id) ? "Unpin" : "Pin"} onClick={() => onTogglePin(tab.id)}>
                          {pinnedSessionIds.has(tab.id) ? <PinOff size={12} /> : <Pin size={12} />}
                        </IconButton>
                      )}
                      {session && (
                        <IconButton
                          title="Kill daemon session"
                          danger
                          onClick={() => {
                            if (window.confirm("Kill this daemon terminal session?")) onKill(session);
                          }}
                        >
                          <Trash2 size={12} />
                        </IconButton>
                      )}
                    </>
                  )}
                />
              );
            })}
          </SessionGroup>

          <SessionGroup title="Pinned Background" empty="No pinned background sessions." count={pinned.length}>
            {pinned.map((session) => (
              <DaemonSessionRow
                key={session.id}
                session={session}
                pinned
                onAttach={onAttach}
                onOpenInSplit={onOpenInSplit}
                onTogglePin={onTogglePin}
                onKill={onKill}
              />
            ))}
          </SessionGroup>

          <SessionGroup title="Agent Sessions" empty="No background agent sessions." count={agentSessions.length}>
            {agentSessions.map((session) => (
              <DaemonSessionRow
                key={session.id}
                session={session}
                pinned={false}
                onAttach={onAttach}
                onOpenInSplit={onOpenInSplit}
                onTogglePin={onTogglePin}
                onKill={onKill}
              />
            ))}
          </SessionGroup>

          <SessionGroup title="Repo Shells" empty="No background repo shells." count={shellSessions.length}>
            {shellSessions.map((session) => (
              <DaemonSessionRow
                key={session.id}
                session={session}
                pinned={false}
                onAttach={onAttach}
                onOpenInSplit={onOpenInSplit}
                onTogglePin={onTogglePin}
                onKill={onKill}
              />
            ))}
          </SessionGroup>
        </div>
    </aside>
  );
}

function SessionGroup({ title, count, empty, children }: {
  title: string;
  count: number;
  empty: string;
  children: React.ReactNode;
}) {
  return (
    <section className="mb-3">
      <div className="mb-1 flex items-center justify-between px-1">
        <h3 className="text-[10px] font-semibold uppercase tracking-wide text-gx-muted">{title}</h3>
        <span className="text-[10px] text-gx-muted">{count}</span>
      </div>
      {count > 0 ? (
        <div className="space-y-1">{children}</div>
      ) : (
        <div className="rounded border border-dashed border-gx-border px-2 py-2 text-[11px] text-gx-muted">{empty}</div>
      )}
    </section>
  );
}

function DaemonSessionRow({
  session,
  pinned,
  onAttach,
  onOpenInSplit,
  onTogglePin,
  onKill,
}: {
  session: DaemonSessionSummary;
  pinned: boolean;
  onAttach: (session: DaemonSessionSummary) => void;
  onOpenInSplit: (session: DaemonSessionSummary) => void;
  onTogglePin: (sessionId: string) => void;
  onKill: (session: DaemonSessionSummary) => void;
}) {
  return (
    <SessionRow
      title={daemonSessionTitle(session)}
      subtitle={sessionSubtitle(session)}
      status={sessionStatusLabel(session)}
      pinned={pinned}
      onPrimary={() => onAttach(session)}
      primaryLabel="Attach"
      actions={(
        <>
          <IconButton title="Open in split" onClick={() => onOpenInSplit(session)}>
            <Columns2 size={12} />
          </IconButton>
          <IconButton title={pinned ? "Unpin" : "Pin"} onClick={() => onTogglePin(session.id)}>
            {pinned ? <PinOff size={12} /> : <Pin size={12} />}
          </IconButton>
          <IconButton
            title="Kill daemon session"
            danger
            onClick={() => {
              if (window.confirm("Kill this daemon terminal session?")) onKill(session);
            }}
          >
            <Trash2 size={12} />
          </IconButton>
        </>
      )}
    />
  );
}

function SessionRow({
  title,
  subtitle,
  status,
  pinned,
  primaryLabel,
  onPrimary,
  actions,
}: {
  title: string;
  subtitle: string;
  status: string;
  pinned: boolean;
  primaryLabel: string;
  onPrimary: () => void;
  actions: React.ReactNode;
}) {
  return (
    <div className="group flex items-center gap-2 rounded border border-gx-border bg-gx-bg-sub px-2 py-2 hover:border-gx-accent/40">
      <button type="button" onClick={onPrimary} className="min-w-0 flex-1 text-left">
        <div className="flex min-w-0 items-center gap-1.5">
          <span className="truncate text-[12px] font-medium text-gx-text">{title}</span>
          {pinned && <Pin size={10} className="shrink-0 text-gx-accent" />}
          <span className="shrink-0 rounded bg-gx-surface px-1 py-0.5 text-[9px] text-gx-muted">{status}</span>
        </div>
        <div className="mt-0.5 truncate text-[10px] text-gx-muted" title={subtitle}>
          {subtitle}
        </div>
      </button>
      <button
        type="button"
        onClick={onPrimary}
        className="rounded bg-gx-accent px-2 py-1 text-[10px] font-medium text-white hover:bg-gx-accent/90"
      >
        {primaryLabel}
      </button>
      <div className="flex shrink-0 items-center gap-0.5">{actions}</div>
    </div>
  );
}

function IconButton({ title, onClick, children, danger = false }: {
  title: string;
  onClick: () => void;
  children: React.ReactNode;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "rounded p-1",
        danger
          ? "text-gx-muted hover:bg-gx-error/10 hover:text-gx-error"
          : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
      )}
      title={title}
      aria-label={title}
    >
      {children}
    </button>
  );
}

// --- Scheme Menu ---

function SchemeMenu({
  currentId, mode, onSelect, onClose,
}: {
  currentId: string;
  mode: "dark" | "light";
  onSelect: (id: string) => void;
  onClose: () => void;
}) {
  // Close on Escape for keyboard users.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      role="menu"
      aria-label="Terminal colour schemes"
      className="absolute left-0 top-full z-50 mt-1 min-w-[240px] rounded border border-gx-border bg-gx-surface p-1 shadow-xl"
    >
      <div className="px-2 py-1 text-[9px] uppercase tracking-wider text-gx-muted/70">
        Colour scheme · {mode === "dark" ? "dark" : "light"}
      </div>
      {TERMINAL_SCHEMES.map((scheme) => {
        const palette = mode === "dark" ? scheme.dark : scheme.light;
        const selected = scheme.id === currentId;
        // Preview swatches: background + 3 representative ANSI colours.
        const swatches = [
          palette.background ?? "#000",
          palette.red ?? "#f00",
          palette.green ?? "#0f0",
          palette.blue ?? "#00f",
        ];
        return (
          <button
            key={scheme.id}
            type="button"
            role="menuitemradio"
            aria-checked={selected}
            onClick={() => onSelect(scheme.id)}
            className={clsx(
              "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] transition",
              selected
                ? "bg-gx-accent/15 text-gx-text"
                : "text-gx-text hover:bg-gx-surface-hover",
            )}
            title={scheme.description}
          >
            {/* Swatch row — 4 tiny squares so the user can eyeball the palette. */}
            <div
              className="flex h-4 w-12 shrink-0 overflow-hidden rounded border border-gx-border/50"
              aria-hidden
            >
              {swatches.map((color, i) => (
                <span key={i} className="flex-1" style={{ background: color }} />
              ))}
            </div>
            <span className="flex-1 truncate">{scheme.name}</span>
            {selected && (
              <span className="text-[9px] font-semibold text-gx-accent">✓</span>
            )}
          </button>
        );
      })}
    </div>
  );
}

// --- Scrollback Settings Menu ---

function ScrollbackSettingsMenu({ onClose }: { onClose: () => void }) {
  const [config, setConfig] = useState<ScrollbackConfig>(loadScrollbackConfig);

  // Close on Escape.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const memoryBytes = estimateMemory(config.maxLines);
  const showWarning = config.maxLines > 100_000;

  function handleLineCountChange(value: number) {
    const clamped = Math.max(MIN_MAX_LINES, Math.min(MAX_MAX_LINES, Math.round(value)));
    const updated: ScrollbackConfig = { ...config, maxLines: clamped };
    setConfig(updated);
    localStorage.setItem(LS_SCROLLBACK_KEY, JSON.stringify(updated));
  }

  function formatMemory(bytes: number): string {
    const mb = bytes / (1024 * 1024);
    if (mb < 1) return `~${Math.round(bytes / 1024)} KB`;
    return `~${Math.round(mb)} MB`;
  }

  return (
    <div
      role="dialog"
      aria-label="Scrollback buffer settings"
      className="absolute left-0 top-full z-50 mt-1 w-[280px] rounded border border-gx-border bg-gx-surface p-3 shadow-xl"
    >
      <div className="mb-3 text-[11px] font-medium text-gx-text">
        Scrollback Buffer
      </div>

      {/* Line count slider */}
      <div className="mb-2">
        <label className="mb-1 flex items-center justify-between text-[10px] text-gx-muted">
          <span>Buffer size</span>
          <span className="font-mono text-gx-text">
            {config.maxLines.toLocaleString()} lines
          </span>
        </label>
        <input
          type="range"
          min={MIN_MAX_LINES}
          max={MAX_MAX_LINES}
          step={1000}
          value={config.maxLines}
          onChange={(e) => handleLineCountChange(Number(e.target.value))}
          className="w-full accent-gx-accent"
          aria-label="Scrollback line count"
        />
        <div className="mt-0.5 flex justify-between text-[9px] text-gx-muted">
          <span>1K</span>
          <span>1M</span>
        </div>
      </div>

      {/* Numeric input for precise control */}
      <div className="mb-3">
        <div className="flex items-center gap-2">
          <input
            type="number"
            min={MIN_MAX_LINES}
            max={MAX_MAX_LINES}
            step={1000}
            value={config.maxLines}
            onChange={(e) => {
              const n = Number(e.target.value);
              if (Number.isFinite(n)) handleLineCountChange(n);
            }}
            className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 font-mono text-[11px] text-gx-text focus-visible:outline-none focus:border-gx-accent"
            aria-label="Scrollback line count (exact)"
          />
        </div>
      </div>

      {/* Memory estimate */}
      <div className="mb-2 flex items-center justify-between rounded bg-gx-bg-sub px-2 py-1.5 text-[10px]">
        <span className="text-gx-muted">Estimated memory</span>
        <span className="font-mono font-medium text-gx-text">
          {formatMemory(memoryBytes)}
        </span>
      </div>

      {/* Warning for high values */}
      {showWarning && (
        <div className="flex items-start gap-1.5 rounded bg-gx-warn/10 px-2 py-1.5 text-[10px] text-gx-warn">
          <AlertTriangle size={11} className="mt-0.5 shrink-0" />
          <span>
            High scrollback ({config.maxLines.toLocaleString()} lines) will use{" "}
            <strong>{formatMemory(memoryBytes)}</strong> of memory. This may
            impact performance on systems with limited RAM.
          </span>
        </div>
      )}

      <div className="mt-3 text-[9px] text-gx-muted">
        Changes apply to new terminal sessions.
      </div>
    </div>
  );
}

// --- Paste Guard Settings Menu ---

function PasteGuardSettingsMenu({ config, onChange, onClose }: {
  config: PasteGuardConfig;
  onChange: (config: PasteGuardConfig) => void;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      role="dialog"
      aria-label="Paste protection settings"
      className="absolute left-0 top-full z-50 mt-1 w-[260px] rounded border border-gx-border bg-gx-surface p-3 shadow-xl"
    >
      <div className="mb-3 text-[11px] font-medium text-gx-text">
        Paste Protection
      </div>

      {/* Enable/disable toggle */}
      <label className="mb-3 flex cursor-pointer items-center justify-between">
        <span className="text-[11px] text-gx-text">Enable paste guard</span>
        <input
          type="checkbox"
          checked={config.enabled}
          onChange={(e) => onChange({ ...config, enabled: e.target.checked })}
          className="h-4 w-4 rounded border-gx-border accent-gx-accent"
        />
      </label>

      {/* Multi-line threshold */}
      <div className={clsx("transition-opacity", !config.enabled && "opacity-50 pointer-events-none")}>
        <label className="mb-1 block text-[10px] text-gx-muted">
          Multi-line threshold (lines)
        </label>
        <input
          type="number"
          min={1}
          max={1000}
          value={config.multiLineThreshold}
          onChange={(e) => {
            const n = Number(e.target.value);
            if (Number.isFinite(n) && n >= 1) {
              onChange({ ...config, multiLineThreshold: Math.min(n, 1000) });
            }
          }}
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 font-mono text-[11px] text-gx-text focus-visible:outline-none focus:border-gx-accent"
          aria-label="Multi-line paste threshold"
        />
        <div className="mt-1 text-[9px] text-gx-muted">
          Pastes with more lines than this will require confirmation.
        </div>
      </div>

      {!config.enabled && (
        <div className="mt-3 flex items-start gap-1.5 rounded bg-gx-warn/10 px-2 py-1.5 text-[10px] text-gx-warn">
          <AlertTriangle size={11} className="mt-0.5 shrink-0" />
          <span>
            Paste protection is disabled. All pasted content will be written directly to the terminal without inspection.
          </span>
        </div>
      )}
    </div>
  );
}

// --- Scrollbar Mode Menu ---

function ScrollbarModeMenu({ currentMode, onSelect, onClose }: {
  currentMode: ScrollbarMode;
  onSelect: (mode: ScrollbarMode) => void;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      role="menu"
      aria-label="Scrollbar mode"
      className="absolute left-0 top-full z-50 mt-1 w-[220px] rounded border border-gx-border bg-gx-surface p-1 shadow-xl"
    >
      <div className="px-2 py-1 text-[9px] uppercase tracking-wider text-gx-muted/70">
        Scrollbar mode
      </div>
      {SCROLLBAR_MODES.map((mode) => {
        const selected = mode.id === currentMode;
        return (
          <button
            key={mode.id}
            type="button"
            role="menuitemradio"
            aria-checked={selected}
            onClick={() => onSelect(mode.id)}
            className={clsx(
              "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[11px] transition",
              selected
                ? "bg-gx-accent/15 text-gx-text"
                : "text-gx-text hover:bg-gx-surface-hover",
            )}
          >
            <span className="flex-1">
              <span className="block">{mode.label}</span>
              <span className="block text-[9px] text-gx-muted">{mode.description}</span>
            </span>
            {selected && (
              <span className="text-[9px] font-semibold text-gx-accent">✓</span>
            )}
          </button>
        );
      })}
      <div className="mt-1 px-2 py-1 text-[9px] text-gx-muted">
        Changes apply immediately to all terminals.
      </div>
    </div>
  );
}

// --- Profile Menu ---

function ProfileMenu({ profiles, onSelect, onSave, onDelete, onClose }: {
  profiles: TermProfile[]; onSelect: (p: TermProfile) => void;
  onSave: (name: string) => void; onDelete: (name: string) => void; onClose: () => void;
}) {
  const [newName, setNewName] = useState("");
  return (
    <div className="absolute left-0 top-full z-50 mt-1 min-w-[180px] rounded border border-gx-border bg-gx-surface py-1 shadow-lg">
      {profiles.length > 0 ? profiles.map((p) => (
        <div key={p.name} className="flex items-center justify-between px-3 py-1 hover:bg-gx-surface-hover">
          <button type="button" onClick={() => onSelect(p)} className="flex-1 text-left text-[11px] text-gx-text truncate">{p.name}</button>
          <button type="button" onClick={() => onDelete(p.name)} className="text-gx-muted hover:text-gx-error ml-2"><X size={9} /></button>
        </div>
      )) : <div className="px-3 py-1 text-[11px] text-gx-muted">No profiles saved</div>}
      <div className="border-t border-gx-border mt-1 pt-1 px-3 pb-1">
        <div className="flex items-center gap-1">
          <input type="text" value={newName} onChange={(e) => setNewName(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter" && newName.trim()) { onSave(newName.trim()); setNewName(""); } if (e.key === "Escape") onClose(); }}
            placeholder="Save profile…" className="flex-1 bg-transparent text-[11px] text-gx-text placeholder:text-gx-muted outline-none" />
          <button type="button" onClick={() => { if (newName.trim()) { onSave(newName.trim()); setNewName(""); } }} className="text-[10px] text-gx-accent hover:text-gx-accent-hover">Save</button>
        </div>
      </div>
    </div>
  );
}

// --- Context Menu ---

function ContextMenu({ x, y, onAction, daemonReady, breakerOpen }: {
  x: number;
  y: number;
  onAction: (action: string) => void;
  daemonReady: boolean;
  breakerOpen: boolean;
}) {
  const daemonDisabled = !daemonReady || breakerOpen;
  const daemonTooltip = !daemonReady
    ? "Daemon is not available"
    : breakerOpen
    ? "Circuit breaker is open — daemon temporarily unavailable"
    : undefined;

  const items: Array<{ label: string; action: string; shortcut?: string; disabled?: boolean; tooltip?: string }> = [
    { label: "Copy Selection", action: "copy", shortcut: "⌘C" },
    { label: "Copy All Output", action: "copyAll", shortcut: "⌘⇧C" },
    { label: "Copy Command Output", action: "copyOutput", shortcut: "" },
    { label: "Paste", action: "paste", shortcut: "⌘V" },
    { label: "Select All", action: "selectAll", shortcut: "⌘A" },
    { label: "---", action: "" },
    { label: "Clear Terminal", action: "clear", shortcut: "⌘K" },
    { label: "Rename Tab", action: "rename", shortcut: "⌘⇧N" },
    { label: "---", action: "" },
    { label: "New daemon terminal", action: "newDaemonTerminal", shortcut: "", disabled: daemonDisabled, tooltip: daemonTooltip },
    { label: "New in-process terminal", action: "newLocalTerminal", shortcut: "⌘T" },
    { label: "---", action: "" },
    { label: "Split Right", action: "splitRight", shortcut: "⌘D" },
    { label: "Split Down", action: "splitDown", shortcut: "⌘⇧D" },
    { label: "Add Bookmark", action: "addBookmark", shortcut: "" },
    { label: "---", action: "" },
    { label: "Export as Text", action: "exportText", shortcut: "" },
    { label: "Export as HTML", action: "exportHtml", shortcut: "" },
    { label: "---", action: "" },
    { label: "Kill Terminal", action: "kill", shortcut: "⌘W" },
  ];
  return (
    <div className="fixed z-[9999] min-w-[200px] rounded border border-gx-border bg-gx-surface py-1 shadow-xl" style={{ left: x, top: y }}>
      {items.map((item, i) => item.label === "---" ? (
        <div key={i} className="my-1 border-t border-gx-border" />
      ) : (
        <button
          key={item.action}
          type="button"
          onClick={() => { if (!item.disabled) onAction(item.action); }}
          disabled={item.disabled}
          title={item.tooltip}
          className={clsx(
            "flex w-full items-center justify-between px-3 py-1.5 text-left text-[11px]",
            item.disabled
              ? "cursor-not-allowed text-gx-muted/50"
              : "text-gx-text hover:bg-gx-surface-hover",
          )}
        >
          <span>{item.label}</span>
          {item.shortcut && <span className="ml-4 text-gx-muted">{item.shortcut}</span>}
        </button>
      ))}
    </div>
  );
}

// --- Helpers ---

function daemonSessionTitle(session: DaemonSessionSummary, fallbackIndex?: number): string {
  if (session.agentMeta) {
    const agent = session.agentMeta.agentType || "agent";
    const worktree = basename(session.agentMeta.worktreePath || session.cwd || session.initialCwd);
    return worktree ? `${agent} · ${worktree}` : agent;
  }
  const cwdName = basename(session.cwd || session.initialCwd);
  if (cwdName) return cwdName;
  return fallbackIndex ? `shell ${fallbackIndex}` : "shell";
}

function sessionSubtitle(session: DaemonSessionSummary): string {
  const cwd = session.cwd || session.initialCwd || "Unknown cwd";
  const lastActive = session.lastDataAt || session.createdAt;
  const parts = [
    cwd,
    lastActive ? `last output ${formatRelativeTime(lastActive)}` : "no output yet",
  ];
  if (session.agentMeta?.activityState) parts.push(session.agentMeta.activityState);
  return parts.join(" · ");
}

function sessionStatusLabel(session: DaemonSessionSummary): string {
  if (!session.isAlive) return "Exited";
  if (session.agentMeta?.activityState && session.agentMeta.activityState !== "idle") {
    return session.agentMeta.activityState;
  }
  if (session.shellState === "pending") return "Starting";
  return "Daemon";
}

function sessionSearchText(session: DaemonSessionSummary): string {
  return [
    daemonSessionTitle(session),
    session.daemonSessionId,
    session.cwd,
    session.initialCwd,
    session.agentMeta?.taskId,
    session.agentMeta?.agentType,
    session.agentMeta?.worktreePath,
    session.agentMeta?.activityState,
  ].filter(Boolean).join(" ");
}

function basename(value: string | null | undefined): string {
  if (!value) return "";
  const parts = value.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? value;
}

function formatRelativeTime(timestamp: number): string {
  const deltaMs = Date.now() - timestamp;
  if (!Number.isFinite(deltaMs) || deltaMs < 0) return "just now";
  const seconds = Math.floor(deltaMs / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/**
 * Strip ANSI escape codes from terminal output so the agent prompt
 * isn't polluted with colour codes and cursor moves.
 */
function stripAnsiForAgent(str: string): string {
  // eslint-disable-next-line no-control-regex
  return str.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, "").replace(/\x1b\][^\x07]*\x07/g, "");
}
