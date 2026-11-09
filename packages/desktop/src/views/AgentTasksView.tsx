/**
 * Agent Tasks View (Orchestrator Mode).
 *
 * Chat workspace and skill management, accessed through the Chat activity entry.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { MessageSquare, GitCompare, Terminal, MoreHorizontal, Pin, Puzzle, Users, FolderTree } from "lucide-react";
import { OpenRepo } from "../state";
import { TaskList } from "../components/orchestrator/TaskList";
import { OrchestratorTask } from "../components/orchestrator/TaskCard";
import { NewTaskDialog, NewTaskData } from "../components/orchestrator/NewTaskDialog";
import { ChatTab, ChatMessage, ToolEvent } from "../components/orchestrator/ChatTab";
import { DiffTab } from "../components/orchestrator/DiffTab";
import { TerminalView } from "./TerminalView";
import { SkillsView } from "./SkillsView";
import { ActionBar } from "../components/orchestrator/ActionBar";
import { NewChatLanding } from "../components/orchestrator/NewChatLanding";
import { AgentSetupWizard } from "../components/orchestrator/AgentSetupWizard";
import { IssueItem } from "../components/orchestrator/IssueSelector";
import { installIssueDeepLinkListener } from "../components/orchestrator/issueDeepLink";
import { getSuggestedActions } from "../components/orchestrator/SuggestedActions";
import { publishToast } from "../lib/toasts";
import { BudgetWarningDialog } from "../components/orchestrator/BudgetWarningDialog";
import { LiveFileTree, LiveFile } from "../components/orchestrator/LiveFileTree";
import { CheckpointDropdown } from "../components/orchestrator/CheckpointDropdown";
import { KeyboardShortcutsButton } from "../components/orchestrator/KeyboardShortcutsDialog";
import { CompactViewToggle } from "../components/orchestrator/CompactViewToggle";
import { FilesTab } from "../components/orchestrator/FilesTab";

import { ResizeHandle } from "../components/orchestrator/ResizeHandle";
import { ContextPinPanel } from "../components/orchestrator/ContextPinPanel";
import { MultiAgentView } from "../components/orchestrator/MultiAgentView";
import { useAgentSessions } from "../lib/use-agent-sessions";
import { useSession, useActiveRepoPath } from "../lib/session-context";
import { ChatTranscript } from "../components/orchestrator/ChatTranscript";
import type { ChatAttachment } from "../components/orchestrator/ChatInput";
import { rpc, type AgentWorkflowDefinition, type AgentWorkflowRun, type RefEntry } from "../ipc";

// ── Types ───────────────────────────────────────────────────────────

type WorkspaceTab = "chat" | "diff" | "files" | "terminal" | "agents";

interface DetectedAgent {
  /** Registry identifier, e.g. "claude-code", "codex", "custom". */
  type: string;
  /** Display name, e.g. "Claude Code", "Codex". */
  name: string;
  /** Binary name (from IPC). */
  binary?: string;
  /** @deprecated Use `binary` — kept for interface compat. */
  command: string;
  version?: string;
  available: boolean;
  /** When true, spawn in a terminal instead of the headless orchestrator adapter. */
  terminalOnly?: boolean;
}

interface Props {
  repo: OpenRepo;
  onOpenAgentManager?: () => void;
}

// ── Helpers ─────────────────────────────────────────────────────────

function mapBackendTask(t: any): OrchestratorTask {
  return {
    id: t.id,
    title: t.title ?? (t.prompt ?? "").slice(0, 80),
    agentName: t.agentType ?? "unknown",
    status: t.status ?? "created",
    createdAt: t.createdAt ?? Date.now(),
    completedAt: t.completedAt,
    lastActivityAt: t.lastActivityAt,
    branch: t.branch,
    baseBranch: t.baseBranch,
    prUrl: t.prUrl,
    prNumber: t.prNumber,
    agents: t.agents,
    worktreePath: t.worktreePath,
    worktreeMode: t.worktreeMode,
    pinnedAt: t.pinnedAt,
    parentTaskId: t.parentTaskId,
    rootTaskId: t.rootTaskId,
    spawnedByAgentId: t.spawnedByAgentId,
    spawnedRole: t.spawnedRole,
    sidebarLabel: t.sidebarLabel,
    childTaskIds: t.childTaskIds,
    sidebarGroup: t.sidebarGroup,
    workflowRunId: t.workflowRunId,
    workflowNodeRunId: t.workflowNodeRunId,
  } as OrchestratorTask;
}

function normalizeBranchRefName(name: string): string {
  return name.replace(/^refs\/heads\//, "");
}

function branchNamesFromRefs(refs: RefEntry[]): string[] {
  const orderedRefs = [...refs]
    .filter((ref) => ref.kind === "branch")
    .sort((a, b) => Number(Boolean(b.is_head)) - Number(Boolean(a.is_head)));
  const names = orderedRefs
    .map((ref) => normalizeBranchRefName(ref.name))
    .filter(Boolean);
  return Array.from(new Set(names));
}

async function promptForSidebarValue(message: string, defaultValue?: string): Promise<string | null> {
  const dialogPrompt = (window as any).crab?.dialogPrompt;
  if (typeof dialogPrompt === "function") {
    return dialogPrompt(message, defaultValue);
  }
  if (typeof window.prompt === "function") {
    return window.prompt(message, defaultValue);
  }
  return null;
}

/** Path variants to handle macOS /private/var vs /var symlink differences. */
function pathVariants(p: string): string[] {
  if (!p) return [];
  const variants = [p];
  if (p.startsWith("/private/")) variants.push(p.slice("/private".length));
  else if (p.startsWith("/var/") || p.startsWith("/tmp/")) variants.push("/private" + p);
  return variants;
}

/**
 * Regex that matches crab-agent worktree directory paths on any platform.
 *
 * The worktree is always created as `os.tmpdir()/crab-agent-<taskId>/`.
 * The taskId is a UUID (hex + hyphens). Rather than hardcoding OS-specific
 * temp directory patterns, we match on the `crab-agent-<id>/` directory
 * name itself — that's the invariant across macOS, Linux, and Windows.
 *
 * Matches:
 *   /private/var/folders/ns/.../T/crab-agent-c2f4fe17-64bf-4339-8622-3d935b3c7522/src/foo.ts
 *   /tmp/crab-agent-abc123/src/foo.ts
 *   C:\Users\dev\AppData\Local\Temp\crab-agent-abc123\src\foo.ts
 *   /any/path/crab-agent-uuid/remaining/path
 *
 * `worktreePathPrefixRegex` — for path fields where the worktree prefix
 * starts at the beginning of the string. Strips everything up to and
 * including the `crab-agent-<id>/` segment.
 *
 * `worktreePathEmbeddedRegex` — for command/output strings where the
 * worktree path may appear anywhere in the string (possibly multiple times).
 */
function worktreePathPrefixRegex(): RegExp {
  return /^.*?[/\\]crab-agent-[0-9a-f-]+[/\\]/;
}

function worktreePathEmbeddedRegex(): RegExp {
  return /[^\s"']*[/\\]crab-agent-[0-9a-f-]+[/\\]/g;
}

/**
 * Infers the ToolEvent type from a tool name for tool_use events.
 * This maps well-known tool names to their semantic category so the
 * activity feed and filtering work correctly for Codex events.
 */
function inferToolEventType(toolName: string): ToolEvent["type"] {
  const lower = toolName.toLowerCase();
  if (lower === "bash" || lower === "terminal" || lower === "powershell" || lower.includes("command")) return "command";
  if (lower === "write" || lower === "edit" || lower === "filedelete" || lower.includes("edit") || lower.includes("write") || lower.includes("delete")) return "file_edit";
  if (lower === "read" || lower === "imageview" || lower === "image_view" || lower.includes("read")) return "file_read";
  if (lower === "grep" || lower === "glob" || lower.includes("search") || lower.includes("find")) return "search";
  if (lower.includes("web") || lower.includes("fetch")) return "web";
  if (lower === "todowrite" || lower.includes("task") || lower.includes("plan")) return "task";
  return "generic";
}

function shouldDeferDiffFetch(status?: string): boolean {
  return status === "running" || status === "iterating" || status === "queued";
}

/**
 * Strips worktree absolute path prefixes from known path fields in a
 * tool input object. Returns a shallow copy with cleaned paths so the
 * tool cards display relative paths instead of long absolute ones.
 *
 * Uses two strategies:
 * 1. Exact prefix match against the known worktreePath (fast path)
 * 2. Regex match against the crab-agent temp dir pattern (fallback)
 */
function stripInputPaths(input: unknown, worktreePath: string): unknown {
  if (!input || typeof input !== "object") return input;

  const obj = input as Record<string, unknown>;
  const pathFields = ["file_path", "path", "directory", "old_path", "new_path"];
  const variants = worktreePath ? pathVariants(worktreePath) : [];

  let changed = false;
  const result: Record<string, unknown> = { ...obj };

  for (const field of pathFields) {
    const val = obj[field];
    if (typeof val !== "string" || !val) continue;

    // Strategy 1: exact prefix match.
    let stripped = false;
    for (const variant of variants) {
      if (val.startsWith(variant + "/")) {
        result[field] = val.slice(variant.length + 1);
        changed = true;
        stripped = true;
        break;
      }
      if (val === variant) {
        result[field] = "";
        changed = true;
        stripped = true;
        break;
      }
    }

    // Strategy 2: regex fallback for crab-agent temp paths.
    if (!stripped) {
      const cleaned = val.replace(worktreePathPrefixRegex(), "");
      if (cleaned !== val) {
        result[field] = cleaned;
        changed = true;
      }
    }
  }

  // Also strip from command strings (Bash tool).
  if (typeof obj.command === "string" && obj.command) {
    let cmd = obj.command as string;

    // Strategy 1: exact match.
    for (const variant of variants) {
      while (cmd.includes(variant + "/")) {
        cmd = cmd.replaceAll(variant + "/", "");
        changed = true;
      }
    }

    // Strategy 2: regex fallback.
    const cleanedCmd = cmd.replace(worktreePathEmbeddedRegex(), "");
    if (cleanedCmd !== cmd) {
      cmd = cleanedCmd;
      changed = true;
    }

    if (changed) result.command = cmd;
  }

  return changed ? result : input;
}

/** Strips worktree temp paths from any string value (for detail/stdout fields). */
function stripWorktreeFromString(value: string | undefined): string | undefined {
  if (!value) return value;
  const cleaned = value.replace(worktreePathEmbeddedRegex(), "");
  return cleaned !== value ? cleaned : value;
}

/**
 * Merge a new agent text event into the current streaming buffer.
 * Codex sometimes replays the full assistant message after deltas have
 * already streamed. Treat that as a replacement/duplicate, not a delta.
 */
function mergeAgentTextContent(existing: string, incoming: string): string | null {
  const existingTrimmed = existing.trim();
  const incomingTrimmed = incoming.trim();
  if (!incomingTrimmed) return existing;
  if (existingTrimmed === incomingTrimmed || existingTrimmed.endsWith(incomingTrimmed)) {
    return null;
  }
  if (
    existing.length > 0 &&
    (incoming.startsWith(existing) ||
      (existingTrimmed.length > 0 && incomingTrimmed.startsWith(existingTrimmed)))
  ) {
    return incoming;
  }
  return existing + incoming;
}

// ── Composer model catalog ─────────────────────────────────────────
//
// Imported from the shared model-catalog module. Keep that file updated
// when new models are released.

import { chatInputModelsForAgent } from "../components/orchestrator/model-catalog";

function modelsForAgent(agentType: string): { value: string; label: string }[] {
  return chatInputModelsForAgent(agentType);
}

// ── Component ───────────────────────────────────────────────────────

export function AgentTasksView({ repo, onOpenAgentManager }: Props) {
  const [section, setSection] = useState<"chats" | "skills">("chats");

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col">
      {/* Composer suggestions stay below navigation; modal dialogs use the document layer. */}
      <nav aria-label="Agent chat" className="relative z-10 flex shrink-0 border-b border-gx-border bg-gx-surface">
        {([
          { id: "chats", label: "Chats", icon: MessageSquare },
          { id: "skills", label: "Skills", icon: Puzzle },
        ] as const).map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            type="button"
            aria-pressed={section === id}
            onClick={() => setSection(id)}
            className={clsx(
              "flex items-center gap-1.5 border-b-2 px-4 py-2 text-xs transition focus-visible:outline focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-gx-accent",
              section === id
                ? "border-gx-accent text-gx-text"
                : "border-transparent text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
            )}
          >
            <Icon size={13} />
            {label}
          </button>
        ))}
      </nav>
      {/* Keep the chat mounted so Skills navigation preserves drafts and live sessions. */}
      <div data-chat-content className={section === "chats" ? "min-h-0 flex-1" : "hidden"}>
        <AgentTasksWorkspace repo={repo} onOpenAgentManager={onOpenAgentManager} active={section === "chats"} />
      </div>
      {section === "skills" && (
        <div className="min-h-0 flex-1">
          <SkillsView repoPath={repo.path} />
        </div>
      )}
    </div>
  );
}

function AgentTasksWorkspace({ repo, onOpenAgentManager, active }: Props & { active: boolean }) {
  const sessionManager = useSession();
  const repoPath = useActiveRepoPath();

  // ── Session-persisted orchestrator state ──────────────────────────
  // Read saved orchestrator view state from the session so switching
  // repos and back restores the exact task + tab the user was on.
  const savedOrchestrator = useMemo(() => {
    if (!repoPath) return null;
    const session = sessionManager.activate(repoPath);
    return session?.viewState?.orchestrator as {
      selectedTaskId?: string | null;
      activeTab?: WorkspaceTab;
      composerAgent?: Record<string, string>;
      composerModel?: Record<string, string>;
      composerMode?: Record<string, "plan" | "instant">;
      activatedTerminalTasks?: string[];
    } | undefined ?? null;
  }, [repoPath, sessionManager]);

  const [tasks, setTasks] = useState<OrchestratorTask[]>([]);
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(
    savedOrchestrator?.selectedTaskId ?? null,
  );
  const [activeTab, setActiveTab] = useState<WorkspaceTab>(
    savedOrchestrator?.activeTab ?? "chat",
  );
  const [dialogOpen, setDialogOpen] = useState(false);
  const LS_AGENTS = "crab.orchestrator.detectedAgents";
  const LS_TASK_SIDEBAR_COLLAPSED = "crab.orchestrator.taskSidebarCollapsed";
  const LS_TASK_SIDEBAR_WIDTH = "crab.orchestrator.taskSidebarWidth";
  const [taskSidebarCollapsed, setTaskSidebarCollapsed] = useState(() => {
    try {
      return localStorage.getItem(LS_TASK_SIDEBAR_COLLAPSED) === "1";
    } catch {
      return false;
    }
  });

  // Hydrate last-known agents from localStorage so the UI doesn't block on
  // first render while the cold-start agent probe runs in the main process.
  // A fresh probe is kicked off in a useEffect and replaces this synchronously
  // once it returns.
  const [agents, setAgents] = useState<DetectedAgent[]>(() => {
    try {
      const raw = localStorage.getItem(LS_AGENTS);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? (parsed as DetectedAgent[]) : [];
    } catch {
      return [];
    }
  });
  const [branches, setBranches] = useState<string[]>(["main"]);
  const [agentWorkflows, setAgentWorkflows] = useState<AgentWorkflowDefinition[]>([]);
  const [workflowRuns, setWorkflowRuns] = useState<AgentWorkflowRun[]>([]);
  const [workflowRunIdsByTask, setWorkflowRunIdsByTask] = useState<Record<string, Set<string>>>({});
  const [messages, setMessages] = useState<Record<string, ChatMessage[]>>({});
  const [streamingTasks, setStreamingTasks] = useState<Set<string>>(new Set());
  // null = checking, true = at least one agent available, false = show setup.
  // Seeded optimistically from the cached agent list so we don't force the
  // setup wizard on every app open while the probe runs.
  const [setupComplete, setSetupComplete] = useState<boolean | null>(() => {
    try {
      const raw = localStorage.getItem(LS_AGENTS);
      if (!raw) return null;
      const parsed = JSON.parse(raw);
      if (!Array.isArray(parsed)) return null;
      return (parsed as DetectedAgent[]).some((a) => a.available) ? true : null;
    } catch {
      return null;
    }
  });

  useEffect(() => {
    try {
      localStorage.setItem(LS_TASK_SIDEBAR_COLLAPSED, taskSidebarCollapsed ? "1" : "0");
    } catch {
      // best-effort
    }
  }, [taskSidebarCollapsed]);

  useEffect(() => {
    const selectTask = (detail: { taskId?: string; repoPath?: string } | null | undefined) => {
      if (!detail?.taskId || detail.repoPath !== repo.path) return;
      setSelectedTaskId(detail.taskId);
      setActiveTab("chat");
    };

    try {
      const raw = localStorage.getItem("crab.orchestrator.pendingOpenTask");
      if (raw) {
        const pending = JSON.parse(raw) as { taskId?: string; repoPath?: string };
        if (pending.taskId && pending.repoPath === repo.path) {
          selectTask(pending);
          localStorage.removeItem("crab.orchestrator.pendingOpenTask");
        }
      }
    } catch {
      localStorage.removeItem("crab.orchestrator.pendingOpenTask");
    }

    const handler = (event: Event) => {
      selectTask((event as CustomEvent<{ taskId?: string; repoPath?: string }>).detail);
    };
    window.addEventListener("crab:select-agent-task", handler);
    return () => window.removeEventListener("crab:select-agent-task", handler);
  }, [repo.path]);

  // PR metadata per task
  const [prMetadata, setPrMetadata] = useState<Record<string, { url: string; number: number }>>({});

  // Per-task composer preferences (agent/model/mode). Persisted to the
  // session so switching repos and back keeps the user's selection stable.
  const [composerAgent, setComposerAgent] = useState<Record<string, string>>(
    savedOrchestrator?.composerAgent ?? {},
  );
  const [composerModel, setComposerModel] = useState<Record<string, string>>(
    savedOrchestrator?.composerModel ?? {},
  );
  const [composerMode, setComposerMode] = useState<Record<string, "plan" | "instant">>(
    savedOrchestrator?.composerMode ?? {},
  );

  // Live file tracking: files the agent is currently modifying (per task).
  const liveFilesRef = useRef<Record<string, Map<string, LiveFile>>>({});
  // Incrementing version to trigger re-renders when liveFilesRef changes.
  const [, setLiveFilesVersion] = useState(0);

  // Track previous TodoWrite input per task for delta computation.
  const prevTodoInputRef = useRef<Record<string, unknown>>({});

  // Track tasks that have received a `done` event in the current turn.
  // Prevents late-arriving events (e.g. item/completed after turn/completed)
  // from re-enabling the streaming spinner after it's been cleared.
  const doneTasksRef = useRef<Set<string>>(new Set());

  // Daemon agent sessions — live state from the PTY daemon.
  // Refreshed on mount and on every crab:agent-state-changed event.
  const { sessions: agentDaemonSessions } = useAgentSessions();

  // Selected task (computed early so effects can reference it)
  const selectedTask = useMemo(
    () => tasks.find((t) => t.id === selectedTaskId) ?? null,
    [tasks, selectedTaskId],
  );

  const refreshTasksFromBackend = useCallback(
    async (preferredTaskId?: string | null): Promise<OrchestratorTask[]> => {
      const taskRepoPath = repo.path ?? repo.repo_id;
      const result = await (window as any).crab?.orchestrator?.listTasks?.({ repoPath: taskRepoPath });
      if (!result?.success || !Array.isArray(result.tasks)) return [];

      const mapped: OrchestratorTask[] = result.tasks.map(mapBackendTask);
      setTasks(mapped);
      setStreamingTasks((prev) => {
        const next = new Set(prev);
        for (const task of mapped) {
          if (task.agentName === "workflow") {
            next.delete(task.id);
            continue;
          }
          if (task.status === "running" || task.status === "iterating") {
            next.add(task.id);
          } else if (task.status === "reviewing" || task.status === "failed" || task.status === "stopped" || task.status === "planning") {
            next.delete(task.id);
          }
        }
        return next;
      });

      const preferred = preferredTaskId && mapped.some((task) => task.id === preferredTaskId)
        ? preferredTaskId
        : null;
      if (preferred) {
        setSelectedTaskId(preferred);
      } else if (selectedTaskId && !mapped.some((task) => task.id === selectedTaskId)) {
        setSelectedTaskId(mapped[0]?.id ?? null);
      } else if (!selectedTaskId && mapped.length > 0) {
        setSelectedTaskId(mapped[0].id);
      }

      return mapped;
    },
    [repo.path, repo.repo_id, selectedTaskId],
  );

  // Track which tasks have had their terminal activated (visited with a
  // worktreePath). These terminals stay mounted (hidden) when switching
  // tasks so PTY sessions and scrollback are preserved.
  // Restore from session state so terminals survive repo/workspace switches.
  const [activatedTerminalTasks, setActivatedTerminalTasks] = useState<Set<string>>(() => {
    const saved = savedOrchestrator?.activatedTerminalTasks;
    if (Array.isArray(saved) && saved.length > 0) {
      return new Set(saved);
    }
    return new Set();
  });

  // When the terminal tab is opened for a task with a worktreePath, mark it as
  // activated. Activated terminals stay mounted while the user switches tasks,
  // but merely selecting a chat should not spin up a shell in the background.
  useEffect(() => {
    if (activeTab === "terminal" && selectedTask?.worktreePath && selectedTask.id) {
      setActivatedTerminalTasks((prev) => {
        if (prev.has(selectedTask.id)) return prev;
        const next = new Set(prev);
        next.add(selectedTask.id);
        // Cap at 5 kept-alive terminal sets to limit memory usage.
        if (next.size > 5) {
          const oldest = next.values().next().value;
          if (oldest) next.delete(oldest);
        }
        return next;
      });
    }
  }, [activeTab, selectedTask?.id, selectedTask?.worktreePath]);

  // Responsive: collapse task list below 1200px.
  const [compact, setCompact] = useState(false);
  // Tabs overflow into "More" dropdown below 900px.
  const [tabsOverflow, setTabsOverflow] = useState(false);

  // Resizable panel widths.
  const [taskListWidth, setTaskListWidth] = useState(() => {
    try {
      const saved = Number(localStorage.getItem(LS_TASK_SIDEBAR_WIDTH));
      return Number.isFinite(saved) ? Math.max(180, Math.min(420, saved)) : 220;
    } catch {
      return 220;
    }
  });
  const [liveTreeWidth, setLiveTreeWidth] = useState(180);

  useEffect(() => {
    try {
      localStorage.setItem(LS_TASK_SIDEBAR_WIDTH, String(taskListWidth));
    } catch {
      // best-effort
    }
  }, [taskListWidth]);

  // Selected file from the live tree (passed to DiffTab to highlight).
  const [selectedLiveFile, setSelectedLiveFile] = useState<string | null>(null);

  // Diff summary stats for the tab badge.
  const [diffStats, setDiffStats] = useState<{ filesChanged: number; additions: number; deletions: number } | null>(null);

  // Selected checkpoint SHA (when user clicks a checkpoint dot).
  const [checkpointSha, setCheckpointSha] = useState<string | null>(null);
  const [checkpointFromSha, setCheckpointFromSha] = useState<string | null>(null);
  const [selectedCheckpointTurn, setSelectedCheckpointTurn] = useState<number | null>(null);

  // Context pin panel visibility.
  const [contextPanelOpen, setContextPanelOpen] = useState(false);

  // Fetch diff stats for the tab badge whenever task or checkpoint changes.
  useEffect(() => {
    if (!selectedTask) {
      setDiffStats(null);
      return;
    }
    if (shouldDeferDiffFetch(selectedTask.status)) {
      setDiffStats(null);
      return;
    }
    (window as any).crab.orchestrator
      .getDiff(selectedTask.id, checkpointSha || undefined, checkpointFromSha || undefined)
      .then((result: { success?: boolean; diff?: { files: Array<{ additions: number; deletions: number }> }; files?: Array<{ additions: number; deletions: number }> }) => {
        const diffFiles = result?.diff?.files ?? result?.files ?? [];
        if (diffFiles.length > 0) {
          setDiffStats({
            filesChanged: diffFiles.length,
            additions: diffFiles.reduce((s, f) => s + (f.additions ?? 0), 0),
            deletions: diffFiles.reduce((s, f) => s + (f.deletions ?? 0), 0),
          });
        } else {
          setDiffStats(null);
        }
      })
      .catch(() => setDiffStats(null));
  }, [selectedTask?.id, selectedTask?.status, checkpointSha, checkpointFromSha]);

  // Applied files tracking — persisted to localStorage so it survives both
  // DiffTab unmount (tab switch) and AgentTasksView unmount (view switch).
  // Keyed by task ID so each task has its own applied set.
  const LS_APPLIED_FILES = "crab.orchestrator.appliedFiles";

  const [appliedFilesByTask, setAppliedFilesByTask] = useState<Record<string, Set<string>>>(() => {
    try {
      const raw = localStorage.getItem(LS_APPLIED_FILES);
      if (!raw) return {};
      const parsed: Record<string, string[]> = JSON.parse(raw);
      const result: Record<string, Set<string>> = {};
      for (const [taskId, paths] of Object.entries(parsed)) {
        if (Array.isArray(paths)) result[taskId] = new Set(paths);
      }
      return result;
    } catch {
      return {};
    }
  });

  // Persist to localStorage whenever appliedFilesByTask changes.
  useEffect(() => {
    const serializable: Record<string, string[]> = {};
    for (const [taskId, paths] of Object.entries(appliedFilesByTask)) {
      if (paths.size > 0) serializable[taskId] = [...paths];
    }
    localStorage.setItem(LS_APPLIED_FILES, JSON.stringify(serializable));
  }, [appliedFilesByTask]);

  const currentAppliedFiles = useMemo(
    () => appliedFilesByTask[selectedTaskId ?? ""] ?? new Set<string>(),
    [appliedFilesByTask, selectedTaskId],
  );
  const setCurrentAppliedFiles = useCallback(
    (updater: Set<string> | ((prev: Set<string>) => Set<string>)) => {
      if (!selectedTaskId) return;
      setAppliedFilesByTask((prev) => {
        const current = prev[selectedTaskId] ?? new Set<string>();
        const next = typeof updater === "function" ? updater(current) : updater;
        return { ...prev, [selectedTaskId]: next };
      });
    },
    [selectedTaskId],
  );

  // ── Persist orchestrator view state to session ─────────────────────
  // Debounced write so rapid tab/task switches don't thrash localStorage.
  // Also flushes synchronously on unmount so state survives repo switches.
  const orchestratorSaveTimer = useRef<ReturnType<typeof setTimeout>>();
  const orchestratorStateRef = useRef({
    selectedTaskId, activeTab, composerAgent, composerModel, composerMode, activatedTerminalTasks,
  });
  orchestratorStateRef.current = {
    selectedTaskId, activeTab, composerAgent, composerModel, composerMode, activatedTerminalTasks,
  };

  useEffect(() => {
    if (!repoPath) return;
    clearTimeout(orchestratorSaveTimer.current);
    orchestratorSaveTimer.current = setTimeout(() => {
      const session = sessionManager.activate(repoPath);
      const existingViewState = session?.viewState ?? {};
      sessionManager.update(repoPath, {
        viewState: {
          ...existingViewState,
          orchestrator: {
            selectedTaskId,
            activeTab,
            composerAgent,
            composerModel,
            composerMode,
            activatedTerminalTasks: Array.from(activatedTerminalTasks),
          },
        },
      });
    }, 300);
    return () => clearTimeout(orchestratorSaveTimer.current);
  }, [selectedTaskId, activeTab, composerAgent, composerModel, composerMode, activatedTerminalTasks, repoPath, sessionManager]);

  // Flush orchestrator state synchronously on unmount (repo switch causes
  // immediate unmount — the debounced save above won't fire in time).
  const repoPathRef = useRef(repoPath);
  repoPathRef.current = repoPath;
  useEffect(() => {
    return () => {
      const path = repoPathRef.current;
      if (!path) return;
      clearTimeout(orchestratorSaveTimer.current);
      const s = orchestratorStateRef.current;
      const session = sessionManager.activate(path);
      const existingViewState = session?.viewState ?? {};
      sessionManager.update(path, {
        viewState: {
          ...existingViewState,
          orchestrator: {
            selectedTaskId: s.selectedTaskId,
            activeTab: s.activeTab,
            composerAgent: s.composerAgent,
            composerModel: s.composerModel,
            composerMode: s.composerMode,
            activatedTerminalTasks: Array.from(s.activatedTerminalTasks),
          },
        },
      });
      // Force immediate flush so the data is in localStorage before the
      // new RepoDetail mounts and reads it.
      sessionManager.flush();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const check = () => {
      setCompact(window.innerWidth < 1200);
      setTabsOverflow(window.innerWidth < 900);
    };
    check();
    window.addEventListener("resize", check);
    return () => window.removeEventListener("resize", check);
  }, []);

  // ── Keyboard Shortcuts ────────────────────────────────────────────
  useEffect(() => {
    // Hidden chats retain their state, but must not act on Skills keyboard input.
    if (!active) return;
    const handler = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey;
      if (meta && e.shiftKey && e.key === "N") {
        e.preventDefault();
        // Deselect current task to show the New Chat landing page.
        setSelectedTaskId(null);
      } else if (meta && e.shiftKey && e.key === "D") {
        e.preventDefault();
        setActiveTab("diff");
      } else if (meta && e.shiftKey && e.key === "R") {
        e.preventDefault();
        if (selectedTaskId) {
          (window as any).crab.orchestrator?.runWorkspace({ taskId: selectedTaskId, repoPath: repo.repo_id }).catch(() => {});
        }
      } else if (meta && e.key === "Enter") {
        e.preventDefault();
        if (selectedTask?.status === "planning" && !streamingTasks.has(selectedTask.id)) {
          (window as any).crab.orchestrator?.approvePlan(selectedTaskId).catch(() => {});
        }
      } else if (meta && e.shiftKey && e.key === "A") {
        e.preventDefault();
        if (selectedTaskId) {
          (window as any).crab.orchestrator?.applyAll(selectedTaskId).catch(() => {});
        }
      } else if (meta && e.key === ".") {
        e.preventDefault();
        if (selectedTask) {
          const actions = getSuggestedActions(selectedTask);
          const primary = actions.find((a) => a.primary);
          if (primary) {
            dispatchToastAction(primary.action, selectedTask.id);
          }
        }
      } else if (e.key === "Escape") {
        if (dialogOpen) setDialogOpen(false);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [active, selectedTaskId, selectedTask, dialogOpen, repo.repo_id]);

  // ── Status transitions → update task state + notifications ────────
  // Subscribes to `orchestrator:task-status` IPC events so that when a
  // workspace's status changes, the task list state is updated. This
  // triggers React re-renders of child components (ActionBar,
  // SuggestedActions) which recalculate context-sensitive actions based
  // on the new status.

  // Statuses that warrant a "next step" toast with an action button.
  const TOAST_WORTHY_STATUSES = new Set(["reviewing", "applied", "failed", "planning"]);

  useEffect(() => {
    const handler = (_event: any, data: { taskId: string; status: string }) => {
      if (!data?.taskId) return;

      const task = tasks.find((t) => t.id === data.taskId);
      if (!task) {
        refreshTasksFromBackend(selectedTaskId).catch(() => {});
      }

      // Don't downgrade a task from "reviewing" (successfully completed)
      // to "failed" — this happens when the Codex process exits after
      // emitting a done event, and the orchestrator interprets the exit
      // as a failure. The done event is the authoritative completion signal.
      setTasks((prev) =>
        prev.map((t) => {
          if (t.id !== data.taskId) return t;
          if (t.status === "reviewing" && data.status === "failed") return t;
          return { ...t, status: data.status as OrchestratorTask["status"], lastActivityAt: Date.now() };
        }),
      );

      // Mark as streaming when the agent is spawning or running — this
      // bridges the gap between user submit and first content event so
      // the UI shows activity immediately.
      if (data.status === "spawning" || data.status === "running" || data.status === "iterating") {
        if (task?.agentName === "workflow") {
          setStreamingTasks((prev) => {
            if (!prev.has(data.taskId)) return prev;
            const next = new Set(prev);
            next.delete(data.taskId);
            return next;
          });
        } else if (task) {
          setStreamingTasks((prev) => {
            if (prev.has(data.taskId)) return prev;
            return new Set([...prev, data.taskId]);
          });
        }
      }

      // Clear streaming state on terminal statuses.
      if (data.status === "reviewing" || data.status === "failed" || data.status === "stopped" || data.status === "planning") {
        setStreamingTasks((prev) => {
          const next = new Set(prev);
          next.delete(data.taskId);
          return next;
        });
      }

      // Only show toasts for "interesting" transitions.
      if (!TOAST_WORTHY_STATUSES.has(data.status)) return;

      const title = task?.title ?? "Task";

      // Build a temporary task object with the new status to compute actions.
      const updatedTask: OrchestratorTask = {
        ...(task ?? { id: data.taskId, title, agentName: "unknown", createdAt: Date.now() }),
        status: data.status as OrchestratorTask["status"],
      };

      const actions = getSuggestedActions(updatedTask);
      const primaryAction = actions.find((a) => a.primary);

      // Determine toast content based on the new status.
      let toastTitle: string;
      let toastKind: "success" | "error" | "info" | "warning";
      switch (data.status) {
        case "reviewing":
          toastTitle = `${title} — ready for review`;
          toastKind = "success";
          break;
        case "applied":
          toastTitle = `${title} — changes applied`;
          toastKind = "success";
          break;
        case "failed":
          toastTitle = `${title} — task failed`;
          toastKind = "error";
          break;
        case "planning":
          toastTitle = `${title} — plan ready`;
          toastKind = "info";
          break;
        default:
          toastTitle = title;
          toastKind = "info";
      }

      // Build the action button callback that dispatches the primary action.
      const toastAction = primaryAction
        ? {
            label: primaryAction.label,
            onClick: () => {
              // Select the task so the action targets the right workspace.
              setSelectedTaskId(data.taskId);
              dispatchToastAction(primaryAction.action, data.taskId);
            },
          }
        : undefined;

      publishToast({
        kind: toastKind,
        title: toastTitle,
        action: toastAction,
        duration: 5000,
      });
    };

    const unsubscribe = (window as any).crab?.orchestrator?.onTaskStatus?.(handler);
    return () => { if (typeof unsubscribe === "function") unsubscribe(); };
  }, [tasks, refreshTasksFromBackend, selectedTaskId]);

  // ── Nonconcurrent-stop toast ──────────────────────────────────────
  // When the orchestrator stops a run in another workspace to start one
  // here, show a brief informational toast so the user knows what happened.

  useEffect(() => {
    const handler = (_event: any, data: { stoppedTaskId: string; startedTaskId: string }) => {
      if (!data?.stoppedTaskId) return;

      const stoppedTask = tasks.find((t) => t.id === data.stoppedTaskId);
      const title = stoppedTask?.title ?? "another workspace";

      publishToast({
        kind: "info",
        title: `Stopped run in ${title} to start here.`,
        duration: 4000,
      });
    };

    const unsubscribe = (window as any).crab?.orchestrator?.onNonconcurrentStop?.(handler);
    return () => { if (typeof unsubscribe === "function") unsubscribe(); };
  }, [tasks]);

  // ── Budget warning ────────────────────────────────────────────────
  // When the orchestrator emits a budget-warning event (workspace cost
  // exceeded its limit), show the BudgetWarningDialog.

  useEffect(() => {
    const handler = (_event: any, data: { taskId: string; cost: number; limit: number; workspaceTitle?: string }) => {
      if (!data?.taskId) return;
      setBudgetWarning(data);
    };

    const unsubscribe = (window as any).crab?.orchestrator?.onBudgetWarning?.(handler);
    return () => { if (typeof unsubscribe === "function") unsubscribe(); };
  }, []);

  // ── Branch auto-rename suggestion ─────────────────────────────────
  const [branchRenameSuggestion, setBranchRenameSuggestion] = useState<{ taskId: string; suggested: string } | null>(null);

  // ── Budget warning dialog state ───────────────────────────────────
  const [budgetWarning, setBudgetWarning] = useState<{ taskId: string; cost: number; limit: number; workspaceTitle?: string } | null>(null);

  useEffect(() => {
    // When a task starts running, suggest a branch rename
    if (selectedTask?.status === "running" && selectedTask.title && !branchRenameSuggestion) {
      const slug = selectedTask.title
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/^-|-$/g, "")
        .slice(0, 40);
      if (slug.length > 3) {
        setBranchRenameSuggestion({ taskId: selectedTask.id, suggested: `feat/${slug}` });
      }
    }
  }, [selectedTask?.status, selectedTask?.id]);

  // Listen for "Delegate to agent" events from other views (task 6.14).
  const [initialPrompt, setInitialPrompt] = useState<string | undefined>();
  const [initialContextFiles, setInitialContextFiles] = useState<string[] | undefined>();
  const [initialIssue, setInitialIssue] = useState<IssueItem | undefined>();
  const [initialSource, setInitialSource] = useState<"manual" | "github" | "linear" | undefined>();

  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail as { prompt?: string; contextFiles?: string[] } | undefined;
      if (detail) {
        setInitialPrompt(detail.prompt);
        setInitialContextFiles(detail.contextFiles);
        setInitialIssue(undefined);
        setInitialSource(undefined);
        // Show the New Chat landing page with pre-filled context.
        setSelectedTaskId(null);
      }
    };
    window.addEventListener("crab:delegate-to-agent", handler);
    return () => window.removeEventListener("crab:delegate-to-agent", handler);
  }, []);

  // Listen for "Create task from issue" deep-link events (task 12.6).
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail as { issue: IssueItem; source: "github" | "linear" } | undefined;
      if (detail?.issue) {
        setInitialIssue(detail.issue);
        setInitialSource(detail.source);
        setInitialPrompt(undefined);
        setInitialContextFiles(undefined);
        // Show the New Chat landing page with pre-filled issue.
        setSelectedTaskId(null);
        setDialogOpen(true);
      }
    };
    window.addEventListener("crab:create-task-from-issue", handler);
    return () => window.removeEventListener("crab:create-task-from-issue", handler);
  }, []);

  // Listen for "answer-question" events from AskUserCard interactive buttons.
  // Routes the user's answer as iteration feedback to the active task.
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail as { answer: string } | undefined;
      if (!detail?.answer || !selectedTaskId) return;
      (window as any).crab?.orchestrator?.iterate(selectedTaskId, detail.answer).catch(() => {});
    };
    window.addEventListener("orchestrator:answer-question", handler);
    return () => window.removeEventListener("orchestrator:answer-question", handler);
  }, [selectedTaskId]);

  // Install the deep-link click listener for GitHub issue URLs.
  useEffect(() => {
    return installIssueDeepLinkListener();
  }, []);

  // Load saved workflow definitions for @ workflow invocation.
  useEffect(() => {
    const workflowRepoPath = repo.path ?? repo.repo_id;
    if (!workflowRepoPath) {
      setAgentWorkflows([]);
      return;
    }

    let cancelled = false;
    (window as any).crab?.agentWorkflows
      ?.listDefinitions?.(workflowRepoPath)
      .then((result: { success?: boolean; definitions?: AgentWorkflowDefinition[] }) => {
        if (cancelled) return;
        setAgentWorkflows(result?.success ? result.definitions ?? [] : []);
      })
      .catch(() => {
        if (!cancelled) setAgentWorkflows([]);
      });

    return () => {
      cancelled = true;
    };
  }, [repo.path, repo.repo_id]);

  // Subscribe to workflow run updates for live progress in chat.
  useEffect(() => {
    const offRun = (window as any).crab?.agentWorkflows?.onRunUpdated?.((run: AgentWorkflowRun) => {
      const repoPath = repo.path ?? repo.repo_id;
      if (run.repoPath !== repoPath) return;
      setWorkflowRuns((current) => {
        const next = current.filter((candidate) => candidate.id !== run.id);
        return [run, ...next];
      });
    });
    return () => {
      if (offRun) offRun();
    };
  }, [repo.path, repo.repo_id]);

  // Initial data fetch.
  useEffect(() => {
    // Fetch tasks filtered by the current repo.
    const repoPath = repo.path ?? repo.repo_id;
    (window as any).crab.orchestrator
      ?.listTasks({ repoPath })
      .then((result: { success: boolean; tasks?: any[] }) => {
        if (!result?.success || !result.tasks) return;
        // Map backend tasks to frontend OrchestratorTask format
        const mapped: OrchestratorTask[] = result.tasks.map(mapBackendTask);
        setTasks(mapped);

        // Seed streamingTasks from task statuses so the UI shows the
        // streaming indicator immediately for tasks that are still running
        // (e.g. after navigating away and back while an agent is active).
        const activeTaskIds = mapped
          .filter((t) => t.agentName !== "workflow" && (t.status === "running" || t.status === "iterating"))
          .map((t) => t.id);
        if (activeTaskIds.length > 0) {
          setStreamingTasks((prev) => {
            const next = new Set(prev);
            for (const id of activeTaskIds) next.add(id);
            return next;
          });
        }

        if (mapped.length > 0 && !selectedTaskId) {
          setSelectedTaskId(mapped[0].id);
        } else if (selectedTaskId && !mapped.some((t) => t.id === selectedTaskId)) {
          // Restored task ID no longer exists — fall back to first task.
          setSelectedTaskId(mapped.length > 0 ? mapped[0].id : null);
        }
      })
      .catch(() => {});

    // Load active workflow runs for live progress in chat.
    (window as any).crab?.agentWorkflows
      ?.listRuns?.(repoPath)
      .then((result: { success?: boolean; runs?: AgentWorkflowRun[] }) => {
        if (result?.success && result.runs) {
          setWorkflowRuns(result.runs.filter((run) => run.status !== "completed" && run.status !== "failed" && run.status !== "cancelled"));
        }
      })
      .catch(() => {});

    // Detect agents.
    //
    // Two-phase to keep the initial render instant:
    //   1. Cached-only call returns immediately with whatever the main
    //      process has cached (empty on true cold start, but main also
    //      prewarms on startup so this is usually populated).
    //   2. Full probe runs in the background and updates state + localStorage
    //      when fresh results arrive. Only demotes `setupComplete` to `false`
    //      when the fresh probe finds nothing — avoids flashing the setup
    //      wizard when the cache is warmer than a cold probe.
    const applyAgents = (detected: DetectedAgent[], { allowDemotion }: { allowDemotion: boolean }) => {
      setAgents(detected);
      try {
        localStorage.setItem(LS_AGENTS, JSON.stringify(detected));
      } catch {
        // Quota or private mode — safe to ignore.
      }
      const hasAvailable = detected.some((a: DetectedAgent) => a.available);
      if (hasAvailable) {
        setSetupComplete(true);
      } else if (allowDemotion) {
        setSetupComplete(false);
      }
    };

    (window as any).crab.orchestrator
      ?.detectAgents({ cachedOnly: true })
      .then((result: { success?: boolean; agents?: DetectedAgent[]; cached?: boolean }) => {
        if (result?.cached && result.agents) {
          // Don't demote setupComplete here — a fresh probe may still succeed.
          applyAgents(result.agents, { allowDemotion: false });
        }
      })
      .catch(() => {});

    (window as any).crab.orchestrator
      ?.detectAgents()
      .then((result: { success?: boolean; agents?: DetectedAgent[] }) => {
        applyAgents(result?.agents ?? [], { allowDemotion: true });
      })
      .catch(() => setSetupComplete((prev) => prev ?? false));

    // Fetch branches via the existing RPC (not orchestrator-specific).
    // Skip for deferred repos — the agent hasn't opened them yet and
    // would return "repository not open" errors.
    if (!repo.deferred) {
      rpc
        .listRefs(repo.repo_id, { kinds: ["branch"] })
        .then((result) => {
          const nextBranches = branchNamesFromRefs(result.refs);
          setBranches(nextBranches.length > 0 ? nextBranches : ["main"]);
        })
        .catch(() => setBranches(["main"]));
    }
  }, [repo.repo_id, repo.deferred]);

  // ── Chat persistence: load history when selecting a task ────────────
  useEffect(() => {
    if (!selectedTaskId) return;

    // Always reload from the database when selecting a task. This ensures
    // we catch up on messages that were persisted while the view was
    // unmounted (e.g. user navigated away during streaming). Without this,
    // the user sees a blank page when returning to an active chat.
    (window as any).crab?.orchestrator
      ?.loadChat?.(selectedTaskId)
      .then((result: { success?: boolean; messages?: ChatMessage[] }) => {
        if (result?.success && result.messages && result.messages.length > 0) {
          setMessages((prev) => {
            const existing = prev[selectedTaskId] ?? [];
            // If the database has more messages than our in-memory state,
            // replace entirely (catches up on messages persisted while
            // the view was unmounted).
            if (result.messages!.length > existing.length) {
              return {
                ...prev,
                [selectedTaskId]: result.messages!,
              };
            }
            // If in-memory has more messages (live streaming) but is
            // missing the initial user message, prepend it. This handles
            // tasks whose initial prompt was persisted before this view mounted.
            if (existing.length > 0 && existing[0].role !== "user") {
              const dbUserMsg = result.messages!.find((m) => m.role === "user");
              if (dbUserMsg && !existing.some((m) => m.id === dbUserMsg.id)) {
                return {
                  ...prev,
                  [selectedTaskId]: [dbUserMsg, ...existing],
                };
              }
            }
            return prev;
          });
        }
      })
      .catch(() => {});
  }, [selectedTaskId]);

  // ── Chat persistence: save when messages change ─────────────────────
  // Keep a ref to the latest messages so the unmount flush can access
  // the current state without re-running the effect on every change.
  const messagesRef = useRef(messages);
  messagesRef.current = messages;
  const selectedTaskIdRef = useRef(selectedTaskId);
  selectedTaskIdRef.current = selectedTaskId;

  useEffect(() => {
    if (!selectedTaskId) return;
    const taskMsgs = messages[selectedTaskId];
    if (!taskMsgs || taskMsgs.length === 0) return;

    // Debounce saves to avoid writing on every streaming chunk.
    // Reduced from 2s to 500ms so messages are persisted sooner —
    // this minimises data loss when the user navigates away mid-stream.
    const timer = setTimeout(() => {
      (window as any).crab?.orchestrator?.saveChat?.(selectedTaskId, taskMsgs).catch(() => {});
    }, 500);

    return () => clearTimeout(timer);
  }, [selectedTaskId, messages]);

  // Flush unsaved messages immediately on unmount so they're available
  // when the view remounts (prevents the blank page on return).
  useEffect(() => {
    return () => {
      const taskId = selectedTaskIdRef.current;
      if (!taskId) return;
      const taskMsgs = messagesRef.current[taskId];
      if (!taskMsgs || taskMsgs.length === 0) return;
      (window as any).crab?.orchestrator?.saveChat?.(taskId, taskMsgs).catch(() => {});
    };
  }, []);

  // Listen for orchestrator events.
  useEffect(() => {
    const handler = (data: { taskId: string; event: any }) => {
      if (!data?.taskId) return;

      // Convert agent events to chat messages for the Chat tab.
      const eventType = data.event?.type;

      // Skip empty/system events entirely.
      if (!eventType || eventType === "usage") return;
      const activityAt = data.event?.timestamp ?? Date.now();
      setTasks((prev) =>
        prev.map((task) => task.id === data.taskId ? { ...task, lastActivityAt: activityAt } : task),
      );

      // Sandbox approval requests → render a dedicated card with
      // Allow / Allow once / Deny actions. We pipe the structured
      // approval payload through the generic ToolEvent input field so
      // the existing tool-card pipeline can render it without a special
      // case in the dispatcher.
      if (eventType === "approval_request" && data.event.approval) {
        const approval = data.event.approval;
        const toolEvent: ToolEvent = {
          type: "generic",
          tool: "CodexApproval",
          summary: approval.summary,
          input: approval,
        };
        setMessages((prev) => {
          const taskMsgs = prev[data.taskId] ?? [];
          const lastMsg = taskMsgs[taskMsgs.length - 1];
          if (lastMsg && lastMsg.role === "agent") {
            const updated = [...taskMsgs];
            updated[updated.length - 1] = {
              ...lastMsg,
              events: [...(lastMsg.events ?? []), toolEvent],
            };
            return { ...prev, [data.taskId]: updated };
          }
          const msg: ChatMessage = {
            id: `${data.taskId}-approval-${Date.now()}-${Math.random().toString(36).slice(2)}`,
            role: "agent",
            content: "",
            timestamp: data.event.timestamp ?? Date.now(),
            events: [toolEvent],
          };
          return { ...prev, [data.taskId]: [...taskMsgs, msg] };
        });
        return;
      }

      // Thread.started → captured by the orchestrator for resume; no UI surface.
      if (eventType === "thread_started") return;

      // Tool result events → attach output as detail to the matching tool card.
      // Routing strategy:
      //   1. If the event carries `itemId`, find the tool event with the same
      //      `itemId` (most precise; survives interleaving of multiple items).
      //   2. Otherwise, fall back to the last tool event in the last agent
      //      message (works for adapters that don't emit item ids).
      //
      // For streaming items, multiple tool_results arrive (one per outputDelta).
      // We append unless the event is marked `input.final: true`, in which
      // case the aggregated payload replaces any partial buffer.
      if (eventType === "tool_result") {
        const resultOutput = data.event.output;
        const resultItemId = (data.event as { itemId?: string }).itemId;
        const resultInput = data.event.input as Record<string, unknown> | undefined;
        const isFinal = !!(resultInput && resultInput.final === true);
        if (resultOutput) {
          const detailText = typeof resultOutput === "string"
            ? resultOutput
            : Array.isArray(resultOutput)
              ? resultOutput
                  .map((block: any) => (typeof block === "string" ? block : block?.text ?? ""))
                  .filter(Boolean)
                  .join("\n")
              : JSON.stringify(resultOutput, null, 2);

          // Truncate very large outputs to keep the UI responsive.
          const MAX_DETAIL_LENGTH = 12000;

          if (detailText) {
            setMessages((prev) => {
              const taskMsgs = prev[data.taskId] ?? [];

              // Locate the target tool event. Prefer itemId match across all
              // recent agent messages; fall back to the last event when no
              // id is provided or no match is found.
              let targetMsgIdx = -1;
              let targetEventIdx = -1;
              if (resultItemId) {
                for (let i = taskMsgs.length - 1; i >= 0 && targetMsgIdx === -1; i--) {
                  const msg = taskMsgs[i];
                  if (msg.role !== "agent" || !msg.events || msg.events.length === 0) continue;
                  for (let j = msg.events.length - 1; j >= 0; j--) {
                    if (msg.events[j].itemId === resultItemId) {
                      targetMsgIdx = i;
                      targetEventIdx = j;
                      break;
                    }
                  }
                }
              }
              if (targetMsgIdx === -1) {
                // Fallback: last tool event in the most recent agent message.
                for (let i = taskMsgs.length - 1; i >= 0; i--) {
                  const msg = taskMsgs[i];
                  if (msg.role === "agent" && msg.events && msg.events.length > 0) {
                    targetMsgIdx = i;
                    targetEventIdx = msg.events.length - 1;
                    break;
                  }
                }
              }
              if (targetMsgIdx === -1) return prev;

              const msg = taskMsgs[targetMsgIdx];
              const lastEvent = msg.events![targetEventIdx];

              // Decide append vs replace:
              //   - `isFinal` always replaces (final aggregated payload wins)
              //   - otherwise, streaming-capable tools append; everything else
              //     keeps the first detail and ignores subsequent ones.
              const isStreamingTool =
                lastEvent.tool === "Bash"
                || lastEvent.tool === "Write"
                || lastEvent.tool === "Edit"
                || lastEvent.tool === "FileDelete"
                || lastEvent.tool === "EnterPlanMode";

              let newDetail: string;
              if (isFinal) {
                newDetail = detailText;
              } else if (!lastEvent.detail) {
                newDetail = detailText;
              } else if (isStreamingTool) {
                newDetail = lastEvent.detail + detailText;
              } else {
                return prev; // non-streaming, already populated
              }
              const truncated = newDetail.length > MAX_DETAIL_LENGTH
                ? newDetail.slice(0, MAX_DETAIL_LENGTH) + "\n… (truncated)"
                : newDetail;
              const updated = [...taskMsgs];
              const updatedEvents = [...msg.events!];
              updatedEvents[targetEventIdx] = { ...lastEvent, detail: truncated };
              updated[targetMsgIdx] = { ...msg, events: updatedEvents };
              return { ...prev, [data.taskId]: updated };
            });
          }
        }
        return;
      }

      // Text output from the agent → show as agent message.
      // Accumulate text into the last agent message if it exists and is recent (streaming).
      if (eventType === "text") {
        const content = data.event.content;
        if (content) {
          setMessages((prev) => {
            const taskMsgs = prev[data.taskId] ?? [];
            const lastMsg = taskMsgs[taskMsgs.length - 1];

            // If the last message is a normal text message from the agent and was
            // recent (< 2s ago), append to it (streaming accumulation).
            if (
              lastMsg &&
              lastMsg.role === "agent" &&
              (lastMsg.kind === "text" || lastMsg.kind === undefined) &&
              Date.now() - lastMsg.timestamp < 2000
            ) {
              const mergedContent = mergeAgentTextContent(lastMsg.content, content);
              if (mergedContent === null) return prev;
              const updated = [...taskMsgs];
              updated[updated.length - 1] = {
                ...lastMsg,
                content: mergedContent,
                timestamp: Date.now(),
              };
              return { ...prev, [data.taskId]: updated };
            }

            // Deduplication: if the incoming text is identical to (or a subset
            // of) a recent agent message, skip it. This handles the case where
            // the adapter re-emits the full assistant message content after
            // streaming has already accumulated it.
            for (let i = taskMsgs.length - 1; i >= Math.max(0, taskMsgs.length - 5); i--) {
              const msg = taskMsgs[i];
              if (msg.role === "user") break;
              if (msg.role !== "agent") continue;
              if (msg.kind !== "text" && msg.kind !== "done" && msg.kind !== undefined) continue;
              const msgContent = msg.content.trim();
              const newContent = content.trim();
              if (
                msgContent === newContent ||
                msgContent.endsWith(newContent) ||
                (newContent.length > 0 && newContent.startsWith(msgContent))
              ) {
                return prev; // Already have this content — skip.
              }
              break; // Only check the most recent agent text/done message in this turn.
            }

            // Otherwise create a new message.
            const msg: ChatMessage = {
              id: `${data.taskId}-${Date.now()}-${Math.random().toString(36).slice(2)}`,
              role: "agent",
              content,
              kind: "text",
              timestamp: data.event.timestamp ?? Date.now(),
            };
            return { ...prev, [data.taskId]: [...taskMsgs, msg] };
          });
        }
      }

      // Done event → marks a turn boundary. When content is present,
      // deduplicate against recent agent messages to avoid showing the
      // final streamed text twice. When content is empty (agent only
      // did tool calls), still emit a done marker so deriveTurns can
      // split the conversation into proper User → AI turns.
      if (eventType === "done") {
        setMessages((prev) => {
          const taskMsgs = prev[data.taskId] ?? [];
          const doneContent = (data.event.content ?? "").trim();

          // When there's actual content, deduplicate against recent messages.
          if (doneContent.length > 0) {
            for (let i = taskMsgs.length - 1; i >= Math.max(0, taskMsgs.length - 5); i--) {
              const msg = taskMsgs[i];
              if (msg.role === "user") break;
              if (msg.role !== "agent") continue;

              const msgContent = msg.content.trim();
              // Exact match → just mark the existing message as done.
              if (msgContent === doneContent) {
                if (msg.kind !== "done") {
                  const updated = [...taskMsgs];
                  updated[i] = { ...msg, kind: "done" };
                  return { ...prev, [data.taskId]: updated };
                }
                return prev;
              }
              // The done content is a subset of an existing message → mark done.
              if (msgContent.length > 0 && msgContent.includes(doneContent)) {
                if (msg.kind !== "done") {
                  const updated = [...taskMsgs];
                  updated[i] = { ...msg, kind: "done" };
                  return { ...prev, [data.taskId]: updated };
                }
                return prev;
              }
              // The existing message is a subset of the done content → replace.
              if (
                msgContent.length > 0 &&
                (msg.kind === "text" || msg.kind === undefined) &&
                doneContent.startsWith(msgContent)
              ) {
                const updated = [...taskMsgs];
                updated[i] = { ...msg, content: data.event.content, kind: "done" };
                return { ...prev, [data.taskId]: updated };
              }
              // Stop searching after the first agent text message.
              if (msg.kind === "text" || msg.kind === undefined) break;
            }
          }

          // No dedup match (or empty content) → append a done marker.
          const msg: ChatMessage = {
            id: `${data.taskId}-done-${Date.now()}-${Math.random().toString(36).slice(2)}`,
            role: "agent",
            content: data.event.content ?? "",
            kind: "done",
            timestamp: data.event.timestamp ?? Date.now(),
          };
          return { ...prev, [data.taskId]: [...taskMsgs, msg] };
        });
      }

      // Tool use events → show as tool cards attached to the last agent message.
      // Skip tool_result events — they contain raw tool_use_ids and are just
      // the return values from tools, not user-visible actions.
      if (eventType === "file_edit" || eventType === "command_run" || eventType === "command" || eventType === "tool_use" || eventType === "file_read" || eventType === "search" || eventType === "web" || eventType === "task" || eventType === "generic") {
        // The adapter already strips worktree paths and provides clean tool names.
        const toolName = data.event.tool ?? "";
        // Skip events where the tool name looks like a raw ID (toolu_bdrk_...)
        if (toolName.startsWith("toolu_") || toolName.startsWith("tool_")) return;

        const rawPathOrCmd = data.event.path ?? data.event.command ?? "";
        // Strip worktree prefix from the summary path/command.
        const pathOrCmd = typeof rawPathOrCmd === "string"
          ? rawPathOrCmd.replace(worktreePathPrefixRegex(), "")
          : rawPathOrCmd;

        // Strip worktree prefix from input paths so tool cards show relative paths.
        const task = tasks.find((t) => t.id === data.taskId);
        const cleanedInput = stripInputPaths(data.event.input, task?.worktreePath ?? repo.path ?? repo.repo_id);

        // For TodoWrite, track previous input for delta rendering.
        let previousInput: unknown | undefined;
        if (toolName === "TodoWrite") {
          previousInput = prevTodoInputRef.current[data.taskId];
          prevTodoInputRef.current[data.taskId] = cleanedInput;
        }

        const toolEvent: ToolEvent = {
          type: eventType === "command_run" || eventType === "command" ? "command" :
                eventType === "file_edit" ? "file_edit" :
                eventType === "file_read" ? "file_read" :
                eventType === "search" ? "search" :
                eventType === "web" ? "web" :
                eventType === "task" ? "task" :
                eventType === "generic" ? "generic" :
                // For tool_use events, infer the ToolEvent type from the tool name.
                eventType === "tool_use" ? inferToolEventType(toolName) :
                "file_read",
          tool: toolName,
          summary: pathOrCmd ? `${toolName}: ${pathOrCmd}` : toolName,
          input: cleanedInput,
          previousInput,
          itemId: (data.event as { itemId?: string }).itemId,
          detail: stripWorktreeFromString(data.event.diff ?? data.event.stdout ?? undefined),
        };

        setMessages((prev) => {
          const taskMsgs = prev[data.taskId] ?? [];
          const lastMsg = taskMsgs[taskMsgs.length - 1];

          // Streaming-update merge: if this is a Codex event with an
          // itemId that already has a card, update that card in place
          // instead of pushing a duplicate. Used by:
          //   - file_edit `kind: "update"` deltas
          //   - plan streaming (`tool: "EnterPlanMode"`, status "streaming")
          //   - command_execution status transitions
          if (toolEvent.itemId) {
            for (let i = taskMsgs.length - 1; i >= Math.max(0, taskMsgs.length - 10); i--) {
              const msg = taskMsgs[i];
              if (msg.role !== "agent" || !msg.events) continue;
              const eventIdx = msg.events.findIndex((e) => e.itemId === toolEvent.itemId && e.tool === toolEvent.tool);
              if (eventIdx === -1) continue;
              const existing = msg.events[eventIdx];
              const incomingInput = (toolEvent.input ?? {}) as Record<string, unknown>;
              const existingInput = (existing.input ?? {}) as Record<string, unknown>;
              const incomingStatus = String(incomingInput.status ?? "");
              // For plan streaming, append the plan delta to the
              // existing buffer; for other items, merge inputs and
              // let the latest event win on shared keys.
              let mergedInput: Record<string, unknown>;
              if (toolEvent.tool === "EnterPlanMode" && incomingStatus === "streaming") {
                mergedInput = {
                  ...existingInput,
                  ...incomingInput,
                  plan: String(existingInput.plan ?? "") + String(incomingInput.plan ?? ""),
                };
              } else {
                mergedInput = { ...existingInput, ...incomingInput };
              }
              const merged: ToolEvent = {
                ...existing,
                ...toolEvent,
                input: mergedInput,
                detail: toolEvent.detail ?? existing.detail,
              };
              const updatedEvents = [...msg.events];
              updatedEvents[eventIdx] = merged;
              const updated = [...taskMsgs];
              updated[i] = { ...msg, events: updatedEvents };
              return { ...prev, [data.taskId]: updated };
            }
          }

          // Attach to the last agent message if it exists.
          if (lastMsg && lastMsg.role === "agent") {
            const updated = [...taskMsgs];
            updated[updated.length - 1] = {
              ...lastMsg,
              events: [...(lastMsg.events ?? []), toolEvent],
            };
            return { ...prev, [data.taskId]: updated };
          }

          // Otherwise create a new agent message with just the tool event.
          const msg: ChatMessage = {
            id: `${data.taskId}-tool-${Date.now()}-${Math.random().toString(36).slice(2)}`,
            role: "agent",
            content: "",
            timestamp: data.event.timestamp ?? Date.now(),
            events: [toolEvent],
          };
          return { ...prev, [data.taskId]: [...taskMsgs, msg] };
        });
      }

      // Live file tracking: only track actual edits (not reads).
      if (eventType === "file_edit") {
        const filePath = data.event.path;
        if (filePath && typeof filePath === "string") {
          if (!liveFilesRef.current[data.taskId]) {
            liveFilesRef.current[data.taskId] = new Map();
          }
          const fileMap = liveFilesRef.current[data.taskId];
          const existing = fileMap.get(filePath);

          fileMap.set(filePath, {
            path: filePath,
            status: existing?.status === "added" ? "added" : "modified",
            lastTouched: Date.now(),
            editCount: (existing?.editCount ?? 0) + 1,
          });
          setLiveFilesVersion((v) => v + 1);
        }
      }

      // Thinking → show as muted agent message, accumulating consecutive
      // thinking deltas into a single bubble. Codex emits one event per
      // reasoning chunk; without merging, the chat fills up with dozens
      // of separate "Thought for a moment" bubbles.
      if (eventType === "thinking" && data.event.content) {
        setMessages((prev) => {
          const taskMsgs = prev[data.taskId] ?? [];
          const lastMsg = taskMsgs[taskMsgs.length - 1];
          // Merge into the previous thinking message when it's recent
          // (< 2s gap) and from the agent. This matches the text-stream
          // accumulation rule, so the user sees a single growing block.
          if (
            lastMsg
            && lastMsg.role === "agent"
            && lastMsg.kind === "thinking"
            && Date.now() - lastMsg.timestamp < 2000
          ) {
            const updated = [...taskMsgs];
            updated[updated.length - 1] = {
              ...lastMsg,
              content: lastMsg.content + data.event.content,
              timestamp: Date.now(),
            };
            return { ...prev, [data.taskId]: updated };
          }
          const msg: ChatMessage = {
            id: `${data.taskId}-think-${Date.now()}-${Math.random().toString(36).slice(2)}`,
            role: "agent",
            content: data.event.content,
            kind: "thinking",
            timestamp: data.event.timestamp ?? Date.now(),
          };
          return { ...prev, [data.taskId]: [...taskMsgs, msg] };
        });
      }

      // Error → show as agent error message
      if (eventType === "error" && data.event.error) {
        const msg: ChatMessage = {
          id: `${data.taskId}-err-${Date.now()}-${Math.random().toString(36).slice(2)}`,
          role: "agent",
          content: `❌ ${data.event.error}`,
          kind: "error",
          timestamp: data.event.timestamp ?? Date.now(),
        };
        setMessages((prev) => ({
          ...prev,
          [data.taskId]: [...(prev[data.taskId] ?? []), msg],
        }));
      }

      // Task completed → update status
      if (eventType === "done") {
        setTasks((prev) =>
          prev.map((t) => (t.id === data.taskId ? { ...t, status: "reviewing" as any, completedAt: activityAt, lastActivityAt: activityAt } : t)),
        );
        setStreamingTasks((prev) => {
          const next = new Set(prev);
          next.delete(data.taskId);
          return next;
        });
      }

      // Streaming state — mark as streaming when meaningful events arrive.
      // Guard: don't re-add after a done/error has been received for this
      // task. This prevents late-arriving events (e.g. item/completed text
      // after turn/completed) from re-enabling the spinner.
      if (eventType === "text" || eventType === "file_edit" || eventType === "command_run" || eventType === "command" || eventType === "tool_use" || eventType === "file_read" || eventType === "search" || eventType === "web" || eventType === "task" || eventType === "generic" || eventType === "thinking") {
        if (!doneTasksRef.current.has(data.taskId)) {
          setStreamingTasks((prev) => {
            if (prev.has(data.taskId)) return prev;
            return new Set([...prev, data.taskId]);
          });
        }
      }
      if (eventType === "done" || eventType === "error") {
        doneTasksRef.current.add(data.taskId);
        setStreamingTasks((prev) => {
          const next = new Set(prev);
          next.delete(data.taskId);
          return next;
        });
      }
    };

    const unsubscribe = (window as any).crab?.orchestrator?.onEvent?.(handler);
    return () => {
      if (typeof unsubscribe === "function") unsubscribe();
    };
  }, []);

  // ── Handlers ────────────────────────────────────────────────────────

  const startWorkflowInvocation = useCallback(
    async ({
      prompt,
      workflowDefinitionIds,
      sourceTaskId,
    }: {
      prompt: string;
      workflowDefinitionIds: string[];
      sourceTaskId?: string | null;
    }) => {
      const repoPathForWorkflow = repo.path ?? repo.repo_id;
      const uniqueWorkflowIds = Array.from(new Set(workflowDefinitionIds.filter(Boolean)));
      if (!repoPathForWorkflow || uniqueWorkflowIds.length === 0) return;

      let preferredTaskId = sourceTaskId ?? null;

      for (const workflowDefinitionId of uniqueWorkflowIds) {
        const workflow = agentWorkflows.find((candidate) => candidate.id === workflowDefinitionId);
        const workflowName = workflow?.name ?? "Workflow";

        if (sourceTaskId) {
          setMessages((prev) => ({
            ...prev,
            [sourceTaskId]: [
              ...(prev[sourceTaskId] ?? []),
              {
                id: `workflow-start-${Date.now()}-${Math.random().toString(36).slice(2)}`,
                role: "agent" as const,
                content: `Starting workflow: ${workflowName}. Spawned chats will appear under this chat in the sidebar.`,
                timestamp: Date.now(),
              },
            ],
          }));
        }

        try {
          const result = await (window as any).crab?.agentWorkflows?.startRun?.(
            repoPathForWorkflow,
            workflowDefinitionId,
            {
              input: {
                prompt,
                source: "ai-chat-orchestrator",
                sourceTaskId: sourceTaskId ?? null,
              },
              trigger: { type: "chat" },
              sourceTaskId: sourceTaskId ?? undefined,
              createSourceTask: !sourceTaskId,
              sourceTaskTitle: `Workflow: ${workflowName}`,
            },
          ) as { success?: boolean; run?: { id: string }; sourceTask?: any; error?: string } | undefined;

          if (!result?.success) {
            throw new Error(result?.error ?? `Failed to start workflow ${workflowName}`);
          }

          // Track workflow run → task association for live progress in chat
          const targetTaskId = sourceTaskId ?? preferredTaskId ?? result.run?.id;
          if (result.run?.id && targetTaskId) {
            setWorkflowRunIdsByTask((prev) => {
              const next = { ...prev };
              const set = new Set(prev[targetTaskId] ?? []);
              set.add(result.run!.id);
              next[targetTaskId] = set;
              return next;
            });
          }

          if (result.sourceTask) {
            const mapped = mapBackendTask(result.sourceTask);
            preferredTaskId = preferredTaskId ?? mapped.id;
            setTasks((prev) => [mapped, ...prev.filter((task) => task.id !== mapped.id)]);
            setStreamingTasks((prev) => {
              const next = new Set(prev);
              next.delete(mapped.id);
              return next;
            });
            setMessages((prev) => {
              const existing = prev[mapped.id] ?? [];
              if (existing.length > 0) return prev;
              return {
                ...prev,
                [mapped.id]: [
                  {
                    id: `${mapped.id}-init`,
                    role: "user" as const,
                    content: prompt,
                    timestamp: Date.now(),
                  },
                  {
                    id: `${mapped.id}-started`,
                    role: "agent" as const,
                    content: `Started workflow: ${workflowName}. Spawned chats will appear below this workflow session.`,
                    timestamp: Date.now(),
                  },
                ],
              };
            });
          }

          publishToast({ kind: "success", title: `Started workflow: ${workflowName}` });
        } catch (err: any) {
          const message = err?.message ?? `Failed to start workflow ${workflowName}`;
          if (sourceTaskId) {
            setMessages((prev) => ({
              ...prev,
              [sourceTaskId]: [
                ...(prev[sourceTaskId] ?? []),
                {
                  id: `workflow-error-${Date.now()}-${Math.random().toString(36).slice(2)}`,
                  role: "agent" as const,
                  kind: "error" as const,
                  content: message,
                  timestamp: Date.now(),
                },
              ],
            }));
          }
          publishToast({ kind: "error", title: message });
        }
      }

      await refreshTasksFromBackend(preferredTaskId).catch(() => {});
      if (preferredTaskId) {
        setSelectedTaskId(preferredTaskId);
        setActiveTab("chat");
      }
    },
    [agentWorkflows, refreshTasksFromBackend, repo.path, repo.repo_id],
  );

  const handleCreateTask = useCallback(
    (data: NewTaskData) => {
      if (data.workflowDefinitionIds?.length) {
        startWorkflowInvocation({
          prompt: data.prompt,
          workflowDefinitionIds: data.workflowDefinitionIds,
          sourceTaskId: null,
        }).catch(() => {});
        return;
      }

      // Generate a task ID up front so it can be used in both the
      // terminal-only path (as agentMeta.taskId) and the optimistic update.
      const optimisticId = `pending-${Date.now()}`;
      const selectedWorktreeMode =
        data.worktree?.mode === "primary"
          ? "primary"
          : data.worktree?.mode === "existing"
            ? "existing"
            : data.worktree?.mode === "auto"
              ? "auto"
              : undefined;
      const selectedWorktreePath = data.worktree?.mode === "existing" ? data.worktree.path : undefined;
      const primaryRepoPath = repo.path ?? repo.repo_id;
      const taskCwd =
        selectedWorktreeMode === "primary"
          ? primaryRepoPath
          : selectedWorktreePath ?? primaryRepoPath;

      // ── Terminal-only agents ──────────────────────────────────────────
      // Agents that don't support the structured NDJSON protocol (e.g. pi,
      // kiro CLI) are spawned in an interactive terminal with the prompt
      // and context serialized as CLI arguments.
      const agent = agents.find((a) => a.type === data.agentName);
      if (agent?.terminalOnly) {
        // Build the CLI command with context.
        const binary = agent.binary || agent.command || agent.type;
        const parts: string[] = [binary];

        // Append context files as arguments if the agent supports them.
        if (data.contextFiles && data.contextFiles.length > 0) {
          for (const file of data.contextFiles) {
            parts.push("--file", file);
          }
        }

        // The prompt is the final positional argument, shell-quoted.
        const escapedPrompt = data.prompt.replace(/'/g, "'\\''");
        parts.push(`'${escapedPrompt}'`);

        const command = parts.join(" ");

        // The shell opens Terminal and delivers this after session restore.
        window.dispatchEvent(
          new CustomEvent("crab:spawn-agent-terminal", {
            detail: {
              name: `${agent.name} · Task`,
              command,
              cwd: taskCwd,
              agentMeta: {
                taskId: optimisticId,
                agentType: agent.type,
                worktreePath: taskCwd ?? "",
              },
            },
          }),
        );
        return;
      }

      // Optimistic update: add a placeholder task immediately so the UI
      // transitions out of the empty state without waiting for the IPC.
      const optimisticTask: OrchestratorTask = {
        id: optimisticId,
        title: data.prompt.slice(0, 80),
        agentName: data.agentName,
        status: "running",
        createdAt: Date.now(),
        lastActivityAt: Date.now(),
        worktreePath: selectedWorktreeMode === "primary" || selectedWorktreeMode === "existing"
          ? taskCwd
          : undefined,
      };
      setTasks((prev) => [optimisticTask, ...prev]);
      setSelectedTaskId(optimisticId);
      // Mark the optimistic task as streaming immediately so the UI shows
      // the loading indicator without waiting for the backend status event.
      setStreamingTasks((prev) => new Set([...prev, optimisticId]));
      setMessages((prev) => ({
        ...prev,
        [optimisticId]: [
          {
            id: `${optimisticId}-init`,
            role: "user" as const,
            content: data.prompt,
            timestamp: Date.now(),
          },
        ],
      }));

      (window as any).crab.orchestrator
        ?.createTask({
          prompt: data.prompt,
          agentType: data.agentName,
          repoPath: primaryRepoPath,
          baseRef: data.baseBranch,
          planFirst: data.planFirst,
          contextFiles: data.contextFiles,
          model: data.model || undefined,
          intelligence: data.intelligence || undefined,
          worktreeMode: selectedWorktreeMode,
          worktreePath: selectedWorktreePath,
          skills: data.skillIds,
        })
        .then((result: { success: boolean; task?: any; error?: string }) => {
          if (!result?.success || !result.task) {
            // Show the task as failed instead of removing it
            setTasks((prev) => prev.map((t) =>
              t.id === optimisticId ? { ...t, status: "failed" as any } : t,
            ));
            // Show error in chat
            if (result?.error) {
              setMessages((prev) => ({
                ...prev,
                [optimisticId]: [
                  ...(prev[optimisticId] ?? []),
                  {
                    id: `err-${Date.now()}`,
                    role: "agent" as const,
                    content: `❌ Task creation failed: ${result.error}`,
                    timestamp: Date.now(),
                  },
                ],
              }));
            }
            return;
          }
          // Replace the optimistic task with the real one
          const realTask = mapBackendTask(result.task);
          setTasks((prev) => prev.map((t) => t.id === optimisticId ? realTask : t));
          setSelectedTaskId(realTask.id);
          // Transfer streaming state from optimistic ID to real ID.
          setStreamingTasks((prev) => {
            if (!prev.has(optimisticId) && !prev.has(realTask.id)) return prev;
            const next = new Set(prev);
            next.delete(optimisticId);
            // The real task should be streaming (it's running).
            next.add(realTask.id);
            return next;
          });
          // Move messages from optimistic ID to real ID, merging with any
          // events that already arrived under the real ID (race: events can
          // arrive before the createTask IPC returns).
          setMessages((prev) => {
            const optimisticMsgs = prev[optimisticId] ?? [];
            const existingRealMsgs = prev[realTask.id] ?? [];
            // Merge: optimistic messages (user prompt) first, then any
            // agent messages that arrived under the real ID.
            const merged = [...optimisticMsgs, ...existingRealMsgs];
            const next = { ...prev, [realTask.id]: merged };
            delete next[optimisticId];
            return next;
          });
        })
        .catch((err: any) => {
          // Show the task as failed with error
          setTasks((prev) => prev.map((t) =>
            t.id === optimisticId ? { ...t, status: "failed" as any } : t,
          ));
          setMessages((prev) => ({
            ...prev,
            [optimisticId]: [
              ...(prev[optimisticId] ?? []),
              {
                id: `err-${Date.now()}`,
                role: "agent" as const,
                content: `❌ ${err?.message ?? "Task creation failed"}`,
                timestamp: Date.now(),
              },
            ],
          }));
        });
    },
    [repo.path, repo.repo_id, agents, startWorkflowInvocation],
  );

  const handleSendMessage = useCallback(
    (text: string, extras?: { attachments?: ChatAttachment[]; agentType?: string; model?: string | null; mode?: "plan" | "instant" }) => {
      if (!selectedTaskId) return;
      // Clear the done guard so the new turn's events can set streaming state.
      doneTasksRef.current.delete(selectedTaskId);
      const msg: ChatMessage = {
        id: `user-${Date.now()}-${Math.random().toString(36).slice(2)}`,
        role: "user",
        content: text,
        timestamp: Date.now(),
      };
      setMessages((prev) => ({
        ...prev,
        [selectedTaskId]: [...(prev[selectedTaskId] ?? []), msg],
      }));
      // Persist composer preferences chosen at send-time so they stick.
      if (extras?.agentType) {
        setComposerAgent((prev) => ({ ...prev, [selectedTaskId]: extras.agentType! }));
      }
      if (extras?.model !== undefined) {
        setComposerModel((prev) => ({ ...prev, [selectedTaskId]: extras.model ?? "" }));
      }
      if (extras?.mode) {
        setComposerMode((prev) => ({ ...prev, [selectedTaskId]: extras.mode! }));
      }

      const workflowDefinitionIds = extras?.attachments
        ?.filter((attachment) => attachment.kind === "workflow")
        .map((attachment) => attachment.value) ?? [];
      if (workflowDefinitionIds.length > 0) {
        startWorkflowInvocation({
          prompt: text,
          workflowDefinitionIds,
          sourceTaskId: selectedTaskId,
        }).catch(() => {});
        return;
      }

      // Prefix plan-mode instructions so existing agents respond with a plan.
      const finalText = extras?.mode === "plan"
        ? `[Plan mode] First, propose an implementation plan. List files to change, approach, and risks. Wait for approval before making changes.\n\n${text}`
        : text;
      (window as any).crab.orchestrator?.iterate(selectedTaskId, finalText).catch(() => {});
    },
    [selectedTaskId, startWorkflowInvocation],
  );

  const handleStop = useCallback(() => {
    if (!selectedTaskId) return;
    (window as any).crab.orchestrator?.cancelTask(selectedTaskId).catch(() => {});
  }, [selectedTaskId]);

  const handleApplyAll = useCallback(async () => {
    if (!selectedTaskId) return;
    try {
      const result = await (window as any).crab.orchestrator?.applyAll(selectedTaskId);
      if (result?.success) {
        publishToast({ kind: "success", title: "Changes merged into your branch" });
        setTasks((prev) =>
          prev.map((t) => t.id === selectedTaskId ? { ...t, status: "applied" as any } : t),
        );
      } else if (result?.conflicts) {
        publishToast({ kind: "warning", title: `Merge conflicts in ${result.conflicts.length} file(s)` });
      } else if (result?.error) {
        publishToast({ kind: "error", title: `Apply failed: ${result.error}` });
      }
      return result;
    } catch (err: any) {
      publishToast({ kind: "error", title: `Apply failed: ${err?.message ?? "unknown error"}` });
      return { success: false };
    }
  }, [selectedTaskId]);

  const handleCherryPick = useCallback(
    async (files: string[]) => {
      if (!selectedTaskId) return;
      try {
        const result = await (window as any).crab.orchestrator?.cherryPick(selectedTaskId, files);
        if (result?.success) {
          publishToast({ kind: "success", title: `${files.length} file(s) applied and committed` });
        } else if (result?.error) {
          publishToast({ kind: "error", title: `Cherry-pick failed: ${result.error}` });
        }
        return result;
      } catch (err: any) {
        publishToast({ kind: "error", title: `Cherry-pick failed: ${err?.message ?? "unknown error"}` });
        return { success: false };
      }
    },
    [selectedTaskId],
  );

  const handleDiscard = useCallback(() => {
    if (!selectedTaskId) return;
    (window as any).crab.orchestrator?.discard(selectedTaskId).then(() => {
      setTasks((prev) => prev.filter((t) => t.id !== selectedTaskId));
      // Clear applied files for the discarded task.
      setAppliedFilesByTask((prev) => {
        const next = { ...prev };
        delete next[selectedTaskId];
        return next;
      });
      setSelectedTaskId(null);
    }).catch(() => {});
  }, [selectedTaskId]);

  const handleDeleteTask = useCallback(
    (taskId: string) => {
      (window as any).crab.orchestrator?.deletePermanently(taskId)
        .then((result: { success: boolean }) => {
          if (!result?.success) return;
          setTasks((prev) => {
            const remaining = prev.filter((t) => t.id !== taskId);
            // If the deleted task was selected, select the next one or null.
            if (selectedTaskId === taskId) {
              const nextTask = remaining[0] ?? null;
              setSelectedTaskId(nextTask?.id ?? null);
            }
            return remaining;
          });
          // Clear applied files for the deleted task.
          setAppliedFilesByTask((prev) => {
            const next = { ...prev };
            delete next[taskId];
            return next;
          });
          // Remove from kept-alive terminals so the TerminalView unmounts.
          setActivatedTerminalTasks((prev) => {
            if (!prev.has(taskId)) return prev;
            const next = new Set(prev);
            next.delete(taskId);
            return next;
          });
        })
        .catch(() => {});
    },
    [selectedTaskId],
  );

  const handleToggleTaskPin = useCallback((taskId: string, pinned: boolean) => {
    setTasks((prev) =>
      prev.map((task) => task.id === taskId ? { ...task, pinnedAt: pinned ? Date.now() : undefined } : task),
    );
    (window as any).crab.orchestrator?.setTaskPinned(taskId, pinned)
      .then((result: { success?: boolean; task?: any }) => {
        if (!result?.success) {
          setTasks((prev) =>
            prev.map((task) => task.id === taskId ? { ...task, pinnedAt: pinned ? undefined : Date.now() } : task),
          );
          return;
        }
        if (result.task) {
          const mapped = mapBackendTask(result.task);
          setTasks((prev) => prev.map((task) => task.id === taskId ? { ...task, ...mapped } : task));
        }
      })
      .catch(() => {
        setTasks((prev) =>
          prev.map((task) => task.id === taskId ? { ...task, pinnedAt: pinned ? undefined : Date.now() } : task),
        );
      });
  }, []);

  const handleSetTaskGroup = useCallback(async (taskId: string, currentGroup?: string) => {
    const nextValue = await promptForSidebarValue(
      "Collection name. Leave blank to remove this chat from its collection.",
      currentGroup ?? "",
    );
    if (nextValue === null) return;

    const nextGroup = nextValue.trim() || undefined;
    const previousGroup = currentGroup?.trim() || undefined;

    setTasks((prev) =>
      prev.map((task) => task.id === taskId ? { ...task, sidebarGroup: nextGroup } : task),
    );

    const setTaskSidebarGroup = (window as any).crab?.orchestrator?.setTaskSidebarGroup;
    if (typeof setTaskSidebarGroup !== "function") {
      setTasks((prev) =>
        prev.map((task) => task.id === taskId ? { ...task, sidebarGroup: previousGroup } : task),
      );
      publishToast({ kind: "error", title: "Collection controls are not available" });
      return;
    }

    setTaskSidebarGroup(taskId, nextGroup ?? null)
      .then((result: { success?: boolean; task?: any; error?: string }) => {
        if (!result?.success) {
          setTasks((prev) =>
            prev.map((task) => task.id === taskId ? { ...task, sidebarGroup: previousGroup } : task),
          );
          publishToast({ kind: "error", title: result?.error ?? "Failed to update collection" });
          return;
        }
        if (result.task) {
          const mapped = mapBackendTask(result.task);
          setTasks((prev) => prev.map((task) => task.id === taskId ? { ...task, ...mapped } : task));
        }
      })
      .catch(() => {
        setTasks((prev) =>
          prev.map((task) => task.id === taskId ? { ...task, sidebarGroup: previousGroup } : task),
        );
        publishToast({ kind: "error", title: "Failed to update collection" });
      });
  }, []);

  const handleRenameTask = useCallback(async (taskId: string, currentTitle: string) => {
    const nextValue = await promptForSidebarValue("Chat name", currentTitle);
    if (nextValue === null) return;
    const nextTitle = nextValue.trim();
    if (!nextTitle || nextTitle === currentTitle) return;

    setTasks((prev) =>
      prev.map((task) => task.id === taskId ? { ...task, title: nextTitle, sidebarLabel: undefined, lastActivityAt: Date.now() } : task),
    );

    const renameTask = (window as any).crab?.orchestrator?.renameTask;
    if (typeof renameTask !== "function") {
      setTasks((prev) =>
        prev.map((task) => task.id === taskId ? { ...task, title: currentTitle } : task),
      );
      publishToast({ kind: "error", title: "Rename controls are not available" });
      return;
    }

    renameTask(taskId, nextTitle)
      .then((result: { success?: boolean; task?: any; error?: string }) => {
        if (!result?.success) {
          setTasks((prev) =>
            prev.map((task) => task.id === taskId ? { ...task, title: currentTitle } : task),
          );
          publishToast({ kind: "error", title: result?.error ?? "Failed to rename chat" });
          return;
        }
        if (result.task) {
          const mapped = mapBackendTask(result.task);
          setTasks((prev) => prev.map((task) => task.id === taskId ? { ...task, ...mapped } : task));
        }
      })
      .catch(() => {
        setTasks((prev) =>
          prev.map((task) => task.id === taskId ? { ...task, title: currentTitle } : task),
        );
        publishToast({ kind: "error", title: "Failed to rename chat" });
      });
  }, []);

  // Dispatch an action triggered from a toast notification button.
  const dispatchToastAction = useCallback(
    (action: string, taskId: string) => {
      const orch = (window as any).crab?.orchestrator;
      switch (action) {
        case "open-diff":
          setActiveTab("diff");
          break;
        case "create-pr":
          orch?.createPr(taskId).catch(() => {});
          break;
        case "retry":
          orch?.retryTask(taskId).catch(() => {});
          break;
        case "archive":
          orch?.archiveTask(taskId).catch(() => {});
          break;
        case "open-pr":
          // Open PR URL in external browser if available.
          const pr = prMetadata[taskId];
          if (pr?.url) {
            (window as any).crab?.shell?.openExternal(pr.url).catch(() => {});
          }
          break;
        default:
          break;
      }
    },
    [prMetadata],
  );

  // ── Render ──────────────────────────────────────────────────────────

  // Show setup wizard when no agents are available.
  if (setupComplete === false && tasks.length === 0) {
    return (
      <AgentSetupWizard
        onReady={(detected) => {
          setAgents(detected as any);
          setSetupComplete(true);
        }}
        onSkip={() => setSetupComplete(true)}
      />
    );
  }

  // Empty state when no tasks exist — show the Codex-style landing page.
  if (tasks.length === 0) {
    return (
      <>
        <NewChatLanding
          repoName={repo.label?.replace(/^(↳\s*)?(worktree:\s*)?/, "") ?? "this repo"}
          agents={agents}
          branches={branches}
          repoId={repo.repo_id}
          repoPath={repo.path ?? repo.repo_id}
          onOpenAgentManager={onOpenAgentManager}
          onSubmit={(data) => {
            handleCreateTask({
              prompt: data.prompt,
              agentName: data.agentType,
              baseBranch: data.baseBranch,
              worktree: data.worktree,
              planFirst: data.planFirst,
              contextFiles: data.contextFiles,
              workflowDefinitionIds: data.workflowDefinitionIds,
              model: data.model || undefined,
              intelligence: data.intelligence || undefined,
            });
          }}
          workflows={agentWorkflows}
          recentChats={[]}
          onSelectRecentChat={(id) => setSelectedTaskId(id)}
        />
        {/* Issue-based task creation dialog (deep-link fallback) */}
        <NewTaskDialog
          open={dialogOpen}
          onClose={() => setDialogOpen(false)}
          onSubmit={handleCreateTask}
          agents={agents}
          branches={branches}
          initialPrompt={initialPrompt}
          initialContextFiles={initialContextFiles}
          initialIssue={initialIssue}
          initialSource={initialSource}
        />
      </>
    );
  }

  const effectiveTaskListCompact = compact || taskSidebarCollapsed;
  const effectiveTaskListWidth = effectiveTaskListCompact ? 56 : taskListWidth;

  return (
    <div className="flex h-full w-full">
      {/* Left: Task List */}
      <div
        className="shrink-0 border-r border-gx-border bg-gx-surface"
        style={{ width: effectiveTaskListWidth }}
      >
        <TaskList
          tasks={tasks}
          selectedTaskId={selectedTaskId}
          onSelectTask={setSelectedTaskId}
          onNewTask={() => setSelectedTaskId(null)}
          onDeleteTask={handleDeleteTask}
          onTogglePin={handleToggleTaskPin}
          onSetGroup={handleSetTaskGroup}
          onRename={handleRenameTask}
          compact={effectiveTaskListCompact}
          collapsed={taskSidebarCollapsed}
          onToggleCollapsed={() => setTaskSidebarCollapsed((value) => !value)}
        />
      </div>

      {/* Resize handle for task list */}
      {!effectiveTaskListCompact && (
        <ResizeHandle
          direction="horizontal"
          onResize={(delta) => {
            setTaskListWidth((w) => Math.max(180, Math.min(420, w + delta)));
          }}
        />
      )}

      {/* Right: Workspace Detail */}
      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        {selectedTask ? (
          <>
            {/* Tabs */}
            <div className="flex shrink-0 items-center border-b border-gx-border bg-gx-surface">
              {(!tabsOverflow || activeTab === "chat") && (
                <TabButton
                  active={activeTab === "chat"}
                  onClick={() => setActiveTab("chat")}
                  icon={<MessageSquare size={13} />}
                  label="Chat"
                />
              )}
              {(!tabsOverflow || activeTab === "diff") && (
                <TabButton
                  active={activeTab === "diff"}
                  onClick={() => setActiveTab("diff")}
                  icon={<GitCompare size={13} />}
                  label="Diff"
                  badge={diffStats && diffStats.filesChanged > 0 ? (
                    <span className="ml-1.5 inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-full bg-gx-accent/15 px-1 text-[10px] font-medium tabular-nums text-gx-accent">
                      {diffStats.filesChanged}
                    </span>
                  ) : undefined}
                />
              )}
              {(!tabsOverflow || activeTab === "terminal") && (
                <TabButton
                  active={activeTab === "terminal"}
                  onClick={() => setActiveTab("terminal")}
                  icon={<Terminal size={13} />}
                  label="Terminal"
                />
              )}
              {(!tabsOverflow || activeTab === "files") && (
                <TabButton
                  active={activeTab === "files"}
                  onClick={() => setActiveTab("files")}
                  icon={<FolderTree size={13} />}
                  label="Files"
                />
              )}
              {(!tabsOverflow || activeTab === "agents") && (selectedTask.agents?.length ?? 0) > 0 && (
                <TabButton
                  active={activeTab === "agents"}
                  onClick={() => setActiveTab("agents")}
                  icon={<Users size={13} />}
                  label="Agents"
                />
              )}

              {/* Spacer + Checkpoint dropdown + Pin button */}
              <div className="flex-1" />
              <CheckpointDropdown
                task={selectedTask}
                selectedTurn={selectedCheckpointTurn}
                onRevert={(turnNumber) => {
                  setCheckpointSha(null);
                  setCheckpointFromSha(null);
                  setSelectedCheckpointTurn(null);
                  publishToast({ kind: "info", title: `Reverted to turn ${turnNumber}` });
                }}
                onSelectTurn={(range) => {
                  if (!range) {
                    setCheckpointSha(null);
                    setCheckpointFromSha(null);
                    setSelectedCheckpointTurn(null);
                  } else {
                    setCheckpointSha(range.toSha ?? "HEAD");
                    setCheckpointFromSha(range.fromSha || null);
                    setSelectedCheckpointTurn(range.turnNumber);
                    setActiveTab("diff");
                  }
                }}
              />
              <KeyboardShortcutsButton />
              <CompactViewToggle taskId={selectedTask.id} />
              <button
                type="button"
                onClick={() => setContextPanelOpen(!contextPanelOpen)}
                className={clsx(
                  "flex items-center gap-1 px-3 py-2 text-xs transition",
                  contextPanelOpen ? "text-gx-accent" : "text-gx-muted hover:text-gx-text",
                )}
                title="Toggle pinned context"
              >
                <Pin size={12} />
              </button>

              {tabsOverflow && (
                <TabOverflowMenu
                  activeTab={activeTab}
                  onSelectTab={setActiveTab}
                />
              )}
            </div>

            {/* Main area: tab content sits left, pin sidebar slides in on the right. */}
            <div className="flex min-h-0 flex-1 overflow-hidden">
            {/* Tab content */}
            <div className="min-h-0 min-w-0 flex-1 relative overflow-hidden">
              {activeTab === "chat" && (
                <div className="flex h-full">
                  {/* Chat content */}
                  <div className="min-w-0 flex-1">
                    <ChatTab
                      task={selectedTask}
                      messages={messages[selectedTask.id] ?? []}
                      isStreaming={streamingTasks.has(selectedTask.id)}
                      onSendMessage={handleSendMessage}
                      onStop={handleStop}
                      iterationCount={
                        (messages[selectedTask.id] ?? []).filter((m) => m.role === "user").length - 1
                      }
                      agents={agents
                        .filter((a) => a.available || a.type === selectedTask.agentName)
                        .map((a) => ({ type: a.type, name: a.name, available: a.available }))}
                      selectedAgentType={composerAgent[selectedTask.id] ?? selectedTask.agentName}
                      onAgentChange={(t) =>
                        setComposerAgent((prev) => ({ ...prev, [selectedTask.id]: t }))
                      }
                      models={modelsForAgent(composerAgent[selectedTask.id] ?? selectedTask.agentName)}
                      selectedModel={composerModel[selectedTask.id] ?? ""}
                      onModelChange={(m) =>
                        setComposerModel((prev) => ({ ...prev, [selectedTask.id]: m }))
                      }
                      mode={composerMode[selectedTask.id] ?? "instant"}
                      onModeChange={(m) =>
                        setComposerMode((prev) => ({ ...prev, [selectedTask.id]: m }))
                      }
                      repoId={repo.repo_id}
                      workflows={agentWorkflows}
                      activeEditorFile={
                        selectedLiveFile
                          ? (selectedTask.worktreePath
                              ? `${selectedTask.worktreePath}/${selectedLiveFile}`
                              : selectedLiveFile)
                          : null
                      }
                      activeEditorLabel={selectedLiveFile?.split("/").pop() ?? null}
                      onOpenAgentManager={onOpenAgentManager}
                      workflowRuns={workflowRuns.filter((run) => {
                        const taskRunIds = workflowRunIdsByTask[selectedTask.id];
                        return run.sourceTaskId === selectedTask.id || taskRunIds?.has(run.id);
                      })}
                      transcript={
                        <ChatTranscript
                          taskId={selectedTask.id}
                          messages={messages[selectedTask.id] ?? []}
                          feedback={
                            (messages[selectedTask.id] ?? [])
                              .filter((m) => m.role === "user")
                              .slice(1)
                              .map((m) => m.content)
                          }
                          isStreaming={streamingTasks.has(selectedTask.id)}
                          onReview={() => setActiveTab("diff")}
                          agentName={selectedTask.agentName}
                        />
                      }
                    />
                  </div>
                  {/* Live File Tree sidebar (visible only when files have been edited) */}
                  {(liveFilesRef.current[selectedTask.id]?.size ?? 0) > 0 && (
                    <>
                      <ResizeHandle
                        direction="horizontal"
                        onResize={(delta) => {
                          // Negative delta = dragging left = making tree wider
                          setLiveTreeWidth((w) => Math.max(120, Math.min(320, w - delta)));
                        }}
                      />
                      <div className="shrink-0 border-l border-gx-border" style={{ width: liveTreeWidth }}>
                        <LiveFileTree
                          files={liveFilesRef.current[selectedTask.id] ?? new Map()}
                          isStreaming={streamingTasks.has(selectedTask.id)}
                          selectedFile={selectedLiveFile}
                          onSelectFile={(filePath) => {
                            setSelectedLiveFile(filePath);
                            setActiveTab("diff");
                          }}
                        />
                      </div>
                    </>
                  )}
                </div>
              )}
              {activeTab === "diff" && (
                <DiffTab
                  task={selectedTask}
                  repo={repo}
                  active={active}
                  initialActiveFile={selectedLiveFile}
                  checkpointSha={checkpointSha}
                  checkpointFromSha={checkpointFromSha}
                  appliedFiles={currentAppliedFiles}
                  onAppliedFilesChange={setCurrentAppliedFiles}
                  onApplyAll={handleApplyAll}
                  onCherryPick={handleCherryPick}
                  onDiscard={handleDiscard}
                  onCreatePr={() => {
                    (window as any).crab.orchestrator?.createPr(selectedTask.id).then((result: any) => {
                      if (result?.success && result.pr) {
                        setPrMetadata((prev) => ({
                          ...prev,
                          [selectedTask.id]: { url: result.pr.url, number: result.pr.number },
                        }));
                        publishToast({ kind: "success", title: `PR #${result.pr.number} created` });
                      } else if (result?.error) {
                        publishToast({ kind: "error", title: `PR creation failed: ${result.error}` });
                      }
                    }).catch((err: any) => {
                      publishToast({ kind: "error", title: `PR creation failed: ${err?.message ?? "unknown error"}` });
                    });
                  }}
                  onReviewWithAi={async () => {
                    try {
                      publishToast({ kind: "info", title: "Running AI review…", duration: 2000 });
                      const result = await (window as any).crab.orchestrator?.reviewWithAi(selectedTask.id);
                      if (!result?.success || !result.reviewText) {
                        publishToast({ kind: "error", title: result?.error ?? "Failed to generate review — no diff found" });
                        return;
                      }
                      const reviewMsg: ChatMessage = {
                        id: `review-${Date.now()}`,
                        role: "agent",
                        content: `🔍 **AI Code Review**\n\n${result.reviewText}`,
                        timestamp: Date.now(),
                      };
                      setMessages((prev) => ({
                        ...prev,
                        [selectedTask.id]: [...(prev[selectedTask.id] ?? []), reviewMsg],
                      }));
                      publishToast({ kind: "success", title: "AI review complete — see Chat tab" });
                      setActiveTab("chat");
                    } catch (err: any) {
                      publishToast({ kind: "error", title: `AI Review failed: ${err?.message ?? "unknown error"}` });
                    }
                  }}
                />
              )}
              {/* Terminal stays mounted (hidden via CSS) to preserve sessions across tab AND task switches.
                  Each activated task gets its own TerminalView that persists until evicted from the keep-alive cap. */}
              <div
                className="absolute inset-0"
                style={{ display: activeTab === "terminal" ? "block" : "none" }}
              >
                {/* Render a TerminalView for each activated task — only the selected one is visible */}
                {Array.from(activatedTerminalTasks).map((taskId) => {
                  const task = tasks.find((t) => t.id === taskId);
                  if (!task?.worktreePath) return null;
                  const isVisible = active && activeTab === "terminal" && selectedTaskId === taskId;
                  return (
                    <div
                      key={task.worktreePath}
                      className="absolute inset-0"
                      style={{ display: isVisible ? "block" : "none" }}
                    >
                      <TerminalView
                        repo={{
                          ...repo,
                          repo_id: `task:${task.id}`,
                          label: task.title
                            ? `${task.title} · agent`
                            : `agent · ${task.id.slice(0, 8)}`,
                          path: task.worktreePath,
                        }}
                        visible={isVisible}
                        defaultSessionType="daemon"
                        onSendErrorToAgent={(feedback) => {
                          handleSendMessage(feedback);
                          setActiveTab("chat");
                        }}
                      />
                    </div>
                  );
                })}
                {/* Show placeholder when selected task has no worktreePath yet */}
                {selectedTask && !selectedTask.worktreePath && (
                  <div className="flex h-full items-center justify-center text-xs text-gx-muted">
                    Preparing workspace…
                  </div>
                )}
              </div>

              {/* Files tab: file tree + editor */}
              {activeTab === "files" && (
                <FilesTab
                  task={selectedTask}
                  selectedFile={selectedLiveFile}
                  onSelectFile={setSelectedLiveFile}
                />
              )}

              {/* Multi-Agent view */}
              {activeTab === "agents" && (
                <MultiAgentView
                  task={selectedTask}
                  isStreaming={streamingTasks.has(selectedTask.id)}
                  daemonSessions={agentDaemonSessions.filter(
                    (s) => s.taskId === selectedTask.id,
                  )}
                  onAddAgent={(agentType, role) => {
                    (window as any).crab.orchestrator?.addAgent(selectedTask.id, agentType, role)
                      .then((result: any) => {
                        if (result?.success) {
                          publishToast({ kind: "success", title: `Added ${role} agent` });
                          (window as any).crab.orchestrator
                            ?.listTasks({ repoPath: repo.path ?? repo.repo_id })
                            .then((listResult: { success?: boolean; tasks?: any[] }) => {
                              if (listResult?.success && listResult.tasks) {
                                setTasks(listResult.tasks.map(mapBackendTask));
                              }
                            })
                            .catch(() => {});
                        } else {
                          publishToast({ kind: "error", title: result?.error ?? "Failed to add agent" });
                        }
                      })
                      .catch(() => {});
                  }}
                />
              )}
            </div>

            {/* Context Pin Panel (slides in from the right, inside the horizontal flex) */}
            {contextPanelOpen && selectedTask && (
              <ContextPinPanel
                task={selectedTask}
                repoId={repo.repo_id}
                onClose={() => setContextPanelOpen(false)}
              />
            )}
            </div>

            {/* Action bar */}
            <ActionBar
              task={selectedTask}
              isStreaming={streamingTasks.has(selectedTask.id)}
              onStop={handleStop}
              onRetry={() => {
                (window as any).crab.orchestrator?.retryTask(selectedTask.id).catch(() => {});
              }}
              onDiscard={handleDiscard}
              onArchive={() => {
                (window as any).crab.orchestrator?.archiveTask(selectedTask.id).catch(() => {});
              }}
              onUndo={() => {
                (window as any).crab.orchestrator?.undoApply(selectedTask.id).catch(() => {});
              }}
              onApprovePlan={() => {
                (window as any).crab.orchestrator?.approvePlan(selectedTask.id).catch(() => {});
              }}
              onRefinePlan={() => setActiveTab("chat")}
              onRejectPlan={() => {
                (window as any).crab.orchestrator?.rejectPlan(selectedTask.id).catch(() => {});
              }}
            />
          </>
        ) : (
          <NewChatLanding
            repoName={repo.label?.replace(/^(↳\s*)?(worktree:\s*)?/, "") ?? "this repo"}
            agents={agents}
            branches={branches}
            repoId={repo.repo_id}
            repoPath={repo.path ?? repo.repo_id}
            onOpenAgentManager={onOpenAgentManager}
            onSubmit={(data) => {
              handleCreateTask({
                prompt: data.prompt,
                agentName: data.agentType,
                baseBranch: data.baseBranch,
                worktree: data.worktree,
                planFirst: data.planFirst,
                contextFiles: data.contextFiles,
                workflowDefinitionIds: data.workflowDefinitionIds,
                model: data.model || undefined,
                intelligence: data.intelligence || undefined,
              });
            }}
            workflows={agentWorkflows}
            recentChats={tasks.slice(0, 3).map((t) => ({
              id: t.id,
              title: t.title ?? t.id,
              agentType: t.agentName ?? "codex",
              timestamp: t.createdAt,
            }))}
            onSelectRecentChat={(id) => setSelectedTaskId(id)}
          />
        )}
      </div>

      {/* New Task Dialog */}
      <NewTaskDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onSubmit={handleCreateTask}
        agents={agents}
        branches={branches}
        initialPrompt={initialPrompt}
        initialContextFiles={initialContextFiles}
        initialIssue={initialIssue}
        initialSource={initialSource}
      />

      {/* Budget Warning Dialog */}
      {budgetWarning && (
        <BudgetWarningDialog
          cost={budgetWarning.cost}
          limit={budgetWarning.limit}
          workspaceTitle={budgetWarning.workspaceTitle}
          onContinue={() => {
            (window as any).crab.orchestrator?.resumeBudget(budgetWarning.taskId).catch(() => {});
            setBudgetWarning(null);
          }}
          onStop={() => {
            (window as any).crab.orchestrator?.cancelTask(budgetWarning.taskId).catch(() => {});
            setBudgetWarning(null);
          }}
        />
      )}
    </div>
  );
}

// ── Tab button ──────────────────────────────────────────────────────

function TabButton({
  active,
  onClick,
  icon,
  label,
  badge,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  badge?: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "flex items-center gap-1.5 border-b-2 px-4 py-2 text-xs transition",
        active
          ? "border-gx-accent text-gx-text"
          : "border-transparent text-gx-muted hover:text-gx-text",
      )}
      aria-selected={active}
      role="tab"
    >
      {icon}
      {label}
      {badge}
    </button>
  );
}

// ── Tab overflow menu (responsive, below 900px) ─────────────────────

function TabOverflowMenu({
  activeTab,
  onSelectTab,
}: {
  activeTab: WorkspaceTab;
  onSelectTab: (tab: WorkspaceTab) => void;
}) {
  const [open, setOpen] = useState(false);
  const allTabs: { id: WorkspaceTab; label: string }[] = [
    { id: "chat", label: "Chat" },
    { id: "diff", label: "Diff" },
    { id: "files", label: "Files" },
    { id: "terminal", label: "Terminal" },
    { id: "agents", label: "Agents" },
  ];
  const hiddenTabs = allTabs.filter((t) => t.id !== activeTab);

  return (
    <div className="relative">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex items-center gap-1 px-3 py-2 text-xs text-gx-muted hover:text-gx-text transition"
        aria-label="More tabs"
      >
        <MoreHorizontal size={13} />
      </button>
      {open && (
        <div className="absolute right-0 top-full z-50 mt-1 rounded-md border border-gx-border bg-gx-surface shadow-lg">
          {hiddenTabs.map((tab) => (
            <button
              key={tab.id}
              type="button"
              onClick={() => { onSelectTab(tab.id); setOpen(false); }}
              className="block w-full px-4 py-2 text-left text-xs text-gx-text hover:bg-gx-surface-hover transition"
            >
              {tab.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Toast notification helper (removed — now uses publishToast from ../lib/toasts) ──
