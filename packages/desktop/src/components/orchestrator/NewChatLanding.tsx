import { Input } from "@/components/ui/input";
import { NativeSelect } from "@/components/ui/native-select";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  AlertTriangle,
  ChevronDown,
  FolderTree,
  GitBranch,
  Plus,
} from "lucide-react";
import {
  ChatInput,
  ChatInputAgent,
  ChatInputHandle,
  ChatInputSubmission,
  ChatMode,
} from "./ChatInput";
import { AgentIcon } from "./AgentIcon";
import { SlashCommand } from "./SlashCommandDropdown";
import { WorkspaceTemplate } from "./TemplatePicker";
import { AgentWorkflowDefinition, HydratedWorktreeEntry, rpc } from "../../ipc";
import { useAgentModels } from "./model-catalog";
import { ModelDiscoveryStatus } from "./ModelDiscoveryStatus";

// ── Types ───────────────────────────────────────────────────────────

interface DetectedAgent {
  type: string;
  name: string;
  version?: string;
  available: boolean;
}

export type WorktreeChoice =
  | { mode: "auto" }
  | { mode: "primary" }
  | { mode: "existing"; path: string }
  | { mode: "new"; name: string; baseBranch: string };

export interface NewChatSubmission {
  prompt: string;
  agentType: string;
  model: string | null;
  /** Intelligence/reasoning effort level for the selected model. */
  intelligence: string | null;
  baseBranch: string;
  worktree: WorktreeChoice;
  planFirst: boolean;
  contextFiles: string[];
  workflowDefinitionIds?: string[];
}

interface RecentChat {
  id: string;
  title: string;
  agentType: string;
  timestamp: number;
}

interface Props {
  repoName: string;
  agents: DetectedAgent[];
  branches: string[];
  onSubmit: (data: NewChatSubmission) => void;
  recentChats?: RecentChat[];
  onSelectRecentChat?: (chatId: string) => void;
  templates?: WorkspaceTemplate[];
  onSelectTemplate?: (templateName: string) => void;
  slashCommands?: SlashCommand[];
  onSlashSelect?: (name: string) => void;
  onCreateCommand?: (initialName?: string) => void;
  repoId?: string | null;
  repoPath?: string | null;
  workflows?: AgentWorkflowDefinition[];
  onOpenAgentManager?: () => void;
}

// ── Component ───────────────────────────────────────────────────────

export function NewChatLanding({
  repoName,
  agents,
  branches,
  onSubmit,
  recentChats = [],
  onSelectRecentChat,
  templates = [],
  onSelectTemplate,
  slashCommands = [],
  onSlashSelect,
  onCreateCommand,
  repoId = null,
  repoPath = null,
  workflows = [],
  onOpenAgentManager,
}: Props) {
  const [agentType, setAgentType] = useState(agents.find((a) => a.available)?.type ?? agents[0]?.type ?? "");
  const [selectedModel, setSelectedModel] = useState("");
  const modelDiscovery = useAgentModels(agentType, repoPath);
  const models = modelDiscovery.models;
  const [intelligence, setIntelligence] = useState<string | null>(null);
  const [baseBranch, setBaseBranch] = useState(branches[0] ?? "main");
  const [worktreeChoice, setWorktreeChoice] = useState<WorktreeChoice>({ mode: "primary" });
  const [branchTouched, setBranchTouched] = useState(false);
  const [mode, setMode] = useState<ChatMode>("instant");
  const [showWorktreeMenu, setShowWorktreeMenu] = useState(false);
  const [showBranchMenu, setShowBranchMenu] = useState(false);
  const [showCreateWorktreeDialog, setShowCreateWorktreeDialog] = useState(false);
  const [worktrees, setWorktrees] = useState<HydratedWorktreeEntry[]>([]);
  const [loadedCommands, setLoadedCommands] = useState<SlashCommand[]>([]);
  const chatInputRef = useRef<ChatInputHandle>(null);

  // Load slash commands on mount.
  useEffect(() => {
    (window as any).crab?.orchestrator
      ?.listCommands?.("")
      .then((result: { success?: boolean; commands?: SlashCommand[] }) => {
        if (result?.success && result.commands) {
          setLoadedCommands(result.commands);
        }
      })
      .catch(() => {});
  }, []);

  // Load existing worktrees.
  useEffect(() => {
    if (!repoId) return;
    rpc.worktreeList(repoId)
      .then((r) => setWorktrees(r.worktrees))
      .catch(() => {});
  }, [repoId]);

  const effectiveCommands = slashCommands.length > 0 ? slashCommands : loadedCommands;

  // Focus the input on mount.
  useEffect(() => {
    chatInputRef.current?.focus();
  }, []);

  // Update agent when agents list changes.
  useEffect(() => {
    if (agents.length > 0 && !agentType) {
      setAgentType(agents.find((a) => a.available)?.type ?? agents[0].type);
    }
  }, [agents, agentType]);

  // Reset model and intelligence when agent changes.
  useEffect(() => {
    setSelectedModel("");
    setIntelligence(null);
  }, [agentType]);

  // Set default intelligence when model changes.
  useEffect(() => {
    const model = models.find(model => model.value === selectedModel);
    if (model?.defaultIntelligence) {
      setIntelligence(model.defaultIntelligence);
    } else {
      setIntelligence(null);
    }
  }, [selectedModel, models]);

  // Update branch when branches list changes. The branch list is ordered with
  // the currently checked-out branch first, so keep that as the default until
  // the user explicitly chooses a branch.
  useEffect(() => {
    if (branches.length === 0) return;
    if (!branchTouched) {
      setBaseBranch(branches[0]);
    } else if (!branches.includes(baseBranch)) {
      setBranchTouched(false);
      setBaseBranch(branches[0]);
    }
  }, [branches, baseBranch, branchTouched]);

  useEffect(() => {
    setBranchTouched(false);
  }, [repoId]);

  const chatInputAgents: ChatInputAgent[] = useMemo(
    () => agents.map((a) => ({ type: a.type, name: a.name, available: a.available })),
    [agents],
  );

  const availableLevels = models.find(model => model.value === selectedModel)?.intelligenceLevels ?? [];

  // Detect worktree/branch conflicts.
  const worktreeConflict = useMemo(() => {
    if (worktreeChoice.mode !== "existing") return null;
    const wt = worktrees.find((w) => w.path === worktreeChoice.path);
    if (!wt) return null;
    if (wt.branch && wt.branch !== baseBranch) {
      return `This worktree is on branch "${wt.branch}". Selecting branch "${baseBranch}" will be ignored — the worktree's branch takes precedence.`;
    }
    return null;
  }, [worktreeChoice, worktrees, baseBranch]);

  const primaryWorkspaceNotice = useMemo(() => {
    if (worktreeChoice.mode !== "primary") return null;
    return `This task will switch the working branch to "${baseBranch}" before running and leave changes there.`;
  }, [worktreeChoice, baseBranch]);

  const handleSubmit = useCallback(
    (submission: ChatInputSubmission) => {
      if (!submission.text.trim()) return;
      onSubmit({
        prompt: submission.text.trim(),
        agentType: submission.agentType,
        model: submission.model,
        intelligence,
        baseBranch,
        worktree: worktreeChoice,
        planFirst: submission.mode === "plan",
        contextFiles: submission.attachments
          .filter((a) => a.kind === "file" || a.kind === "folder")
          .map((a) => a.value),
        workflowDefinitionIds: submission.attachments
          .filter((a) => a.kind === "workflow")
          .map((a) => a.value),
      });
    },
    [baseBranch, worktreeChoice, intelligence, onSubmit],
  );

  const handlePickContextFiles = useCallback(async () => {
    try {
      const picked = await (window as any).crab.orchestrator.pickContextFiles();
      return picked && Array.isArray(picked) ? picked : null;
    } catch {
      return null;
    }
  }, []);

  // Worktree display label.
  const worktreeLabel = useMemo(() => {
    switch (worktreeChoice.mode) {
      case "auto": return "Auto worktree";
      case "primary": return "Work locally";
      case "existing": {
        const wt = worktrees.find((w) => w.path === worktreeChoice.path);
        const name = wt?.meta?.display_name || worktreeChoice.path.split("/").pop() || "worktree";
        return name;
      }
      case "new": return `New: ${worktreeChoice.name || "unnamed"}`;
    }
  }, [worktreeChoice, worktrees]);

  return (
    <div className="flex h-full flex-col items-center justify-center px-6">
      <div className="w-full max-w-[640px] space-y-5">
        {/* Heading */}
        <h1 className="text-center text-lg font-semibold text-foreground">
          What should we build in{" "}
          <span className="text-primary">{repoName}</span>?
        </h1>

        {/* Main input area */}
        <div className="relative">
          <ModelDiscoveryStatus discovery={modelDiscovery} />
          <ChatInput
            ref={chatInputRef}
            placeholder="Ask anything. / for commands, @ for context, # for files…"
            isStreaming={false}
            onSubmit={handleSubmit}
            onStop={() => {}}
            agents={chatInputAgents}
            agentType={agentType}
            onAgentChange={setAgentType}
            models={models}
            model={selectedModel}
            onModelChange={setSelectedModel}
            intelligenceLevels={availableLevels}
            intelligence={intelligence ?? undefined}
            onIntelligenceChange={(level) => setIntelligence(level)}
            mode={mode}
            onModeChange={setMode}
            slashCommands={effectiveCommands}
            onSlashSelect={onSlashSelect}
            onCreateCommand={onCreateCommand}
            onPickContextFiles={handlePickContextFiles}
            onOpenAgentManager={onOpenAgentManager}
            repoId={repoId}
            workflows={workflows}
          />
        </div>

        {/* Session controls — right-aligned row below input */}
        <div className="flex items-center justify-end gap-2 flex-wrap">
          {/* Worktree selector */}
          <WorktreeSelector
            worktrees={worktrees}
            choice={worktreeChoice}
            onChange={(c) => {
              if (c.mode === "new" && c.name === "" && c.baseBranch === "") {
                // User clicked "Create new worktree manually" — show dialog.
                setShowCreateWorktreeDialog(true);
              } else {
                setWorktreeChoice(c);
              }
            }}
            open={showWorktreeMenu}
            onToggle={() => setShowWorktreeMenu(!showWorktreeMenu)}
            onClose={() => setShowWorktreeMenu(false)}
            label={worktreeLabel}
            primaryPath={repoPath}
          />

          {/* Branch selector */}
          <DropdownButton
            open={showBranchMenu}
            onToggle={() => setShowBranchMenu(!showBranchMenu)}
            onClose={() => setShowBranchMenu(false)}
            label={
              <span className="flex items-center gap-1.5">
                <GitBranch size={11} />
                <span>{baseBranch}</span>
              </span>
            }
            items={branches.map((b) => ({ id: b, label: b }))}
            onSelect={(id) => {
              setBaseBranch(id);
              setBranchTouched(true);
            }}
            selectedId={baseBranch}
          />
        </div>

        {/* Worktree warning */}
        {(worktreeConflict || primaryWorkspaceNotice) && (
          <div className="flex items-start gap-2 rounded-lg border border-warning/40 bg-warning/5 px-3 py-2 text-[11px] text-warning">
            <AlertTriangle size={13} className="mt-0.5 shrink-0" />
            <span>{worktreeConflict || primaryWorkspaceNotice}</span>
          </div>
        )}

        {/* Recent chats / suggestions */}
        {recentChats.length > 0 && (
          <div className="space-y-2">
            {recentChats.slice(0, 3).map((chat) => (
              <button
                key={chat.id}
                type="button"
                onClick={() => onSelectRecentChat?.(chat.id)}
                className="flex w-full items-center gap-3 rounded-lg border border-border bg-background px-4 py-3 text-left transition hover:border-primary/40 hover:bg-accent"
              >
                <AgentIcon agentName={chat.agentType} size={14} />
                <span className="min-w-0 flex-1 truncate text-xs text-foreground">
                  {chat.title}
                </span>
                <span className="shrink-0 text-[10px] text-muted-foreground">
                  {formatRelativeTime(chat.timestamp)}
                </span>
              </button>
            ))}
          </div>
        )}

        {/* Templates (when no recent chats) */}
        {recentChats.length === 0 && templates.length > 0 && (
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
            {templates.slice(0, 6).map((t) => (
              <button
                key={t.name}
                type="button"
                onClick={() => onSelectTemplate?.(t.name)}
                className="flex flex-col items-start gap-1 rounded-lg border border-border bg-background p-3 text-left transition hover:border-primary/40 hover:bg-accent"
              >
                {t.icon && <span className="text-base">{t.icon}</span>}
                <span className="text-[11px] font-medium text-foreground">{t.name}</span>
                {t.description && (
                  <span className="text-[10px] text-muted-foreground line-clamp-2">{t.description}</span>
                )}
              </button>
            ))}
          </div>
        )}
      </div>

      {/* Create Worktree Dialog */}
      {showCreateWorktreeDialog && repoId && (
        <CreateWorktreeDialog
          repoId={repoId}
          branches={branches}
          onCreated={(createdPath) => {
            // Refresh worktree list and select the newly created one.
            rpc.worktreeList(repoId)
              .then((r) => {
                setWorktrees(r.worktrees);
                if (createdPath) {
                  setWorktreeChoice({ mode: "existing", path: createdPath });
                }
              })
              .catch(() => {});
            setShowCreateWorktreeDialog(false);
          }}
          onClose={() => setShowCreateWorktreeDialog(false)}
        />
      )}
    </div>
  );
}

// ── Create Worktree Dialog ──────────────────────────────────────────

function CreateWorktreeDialog({
  repoId,
  branches,
  onCreated,
  onClose,
}: {
  repoId: string;
  branches: string[];
  onCreated: (path: string) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState("");
  const [startPoint, setStartPoint] = useState(branches[0] ?? "main");
  const [path, setPath] = useState("");
  const [mode, setMode] = useState<"new-branch" | "existing-branch">("new-branch");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const handlePickPath = useCallback(async () => {
    try {
      const selected = await (window as any).crab.pickWorktreePath();
      if (selected) setPath(selected);
    } catch {
      // User cancelled.
    }
  }, []);

  const handleSubmit = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault();
      if (!name.trim()) return;
      setBusy(true);
      setError(null);

      try {
        const opts: any = {
          // If user picked a path, use it. Otherwise let the backend
          // use a default location (sibling to the repo).
          path: path.trim() || undefined,
          branch: name.trim(),
        };

        if (mode === "new-branch") {
          opts.create_branch = true;
          opts.start_point = startPoint.trim() || undefined;
        }

        const result = await rpc.gitWorktreeAdd(repoId, opts);
        if (result?.created && result.path) {
          onCreated(result.path);
        } else {
          // Fallback — still close and select by branch name.
          onCreated(result?.path ?? "");
        }
      } catch (err: any) {
        setError(err?.message ?? "Failed to create worktree");
        setBusy(false);
      }
    },
    [name, path, startPoint, mode, repoId, onCreated],
  );

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-full max-w-sm rounded-lg border border-border bg-background p-4 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="mb-3 text-sm font-semibold text-foreground">Create Worktree</h3>

        <form onSubmit={handleSubmit} className="space-y-3">
          {/* Mode tabs */}
          <div className="flex gap-1 text-[10px]">
            <button
              type="button"
              className={clsx(
                "rounded px-2.5 py-1 transition",
                mode === "new-branch"
                  ? "bg-primary/20 text-foreground font-medium"
                  : "text-muted-foreground hover:text-foreground",
              )}
              onClick={() => setMode("new-branch")}
            >
              New branch
            </button>
            <button
              type="button"
              className={clsx(
                "rounded px-2.5 py-1 transition",
                mode === "existing-branch"
                  ? "bg-primary/20 text-foreground font-medium"
                  : "text-muted-foreground hover:text-foreground",
              )}
              onClick={() => setMode("existing-branch")}
            >
              Existing branch
            </button>
          </div>

          {/* Branch name / selector */}
          {mode === "new-branch" ? (
            <div>
              <label className="mb-1 block text-[11px] font-medium text-muted-foreground">
                Branch name
              </label>
              <Input
                ref={inputRef}
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="feature/my-task"
                className="w-full rounded-md border border-border bg-background px-3 py-1.5 text-xs text-foreground placeholder:text-muted-foreground/60 focus:border-primary focus:outline-none"
              />
            </div>
          ) : (
            <div>
              <label className="mb-1 block text-[11px] font-medium text-muted-foreground">
                Branch to check out
              </label>
              <NativeSelect
                value={name}
                onChange={(e) => setName(e.target.value)}
                className="w-full rounded-md border border-border bg-background px-3 py-1.5 text-xs text-foreground focus:border-primary focus:outline-none"
              >
                <option value="">Select a branch…</option>
                {branches.map((b) => (
                  <option key={b} value={b}>{b}</option>
                ))}
              </NativeSelect>
            </div>
          )}

          {/* Start point (for new branch mode) */}
          {mode === "new-branch" && (
            <div>
              <label className="mb-1 block text-[11px] font-medium text-muted-foreground">
                Start from
              </label>
              <NativeSelect
                value={startPoint}
                onChange={(e) => setStartPoint(e.target.value)}
                className="w-full rounded-md border border-border bg-background px-3 py-1.5 text-xs text-foreground focus:border-primary focus:outline-none"
              >
                {branches.map((b) => (
                  <option key={b} value={b}>{b}</option>
                ))}
              </NativeSelect>
            </div>
          )}

          {/* Folder path (optional) */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-muted-foreground">
              Location <span className="font-normal text-muted-foreground/60">(optional — uses default if empty)</span>
            </label>
            <div className="flex items-center gap-1.5">
              <button
                type="button"
                onClick={handlePickPath}
                className="flex flex-1 items-center justify-between rounded-md border border-border bg-background px-3 py-1.5 text-left text-xs transition hover:border-muted-foreground/60 focus:border-primary focus:outline-none"
              >
                <span className={path ? "truncate text-foreground" : "text-muted-foreground/60"}>
                  {path || "Default location (sibling to repo)"}
                </span>
                <FolderTree size={11} className="shrink-0 text-muted-foreground ml-2" />
              </button>
              {path && (
                <button
                  type="button"
                  onClick={() => setPath("")}
                  className="shrink-0 rounded p-1 text-muted-foreground hover:text-foreground"
                  aria-label="Clear path"
                >
                  ×
                </button>
              )}
            </div>
          </div>

          {/* Error */}
          {error && (
            <div className="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-1.5 text-[11px] text-destructive">
              {error}
            </div>
          )}

          {/* Actions */}
          <div className="flex justify-end gap-2 pt-1">
            <button
              type="button"
              onClick={onClose}
              disabled={busy}
              className="rounded-md border border-border px-3 py-1.5 text-xs text-muted-foreground hover:text-foreground transition disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={!name.trim() || busy}
              className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition hover:bg-primary/80 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {busy ? "Creating…" : "Create"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

// ── Worktree Selector ───────────────────────────────────────────────

function WorktreeSelector({
  worktrees,
  choice,
  onChange,
  open,
  onToggle,
  onClose,
  label,
  primaryPath,
}: {
  worktrees: HydratedWorktreeEntry[];
  choice: WorktreeChoice;
  onChange: (c: WorktreeChoice) => void;
  open: boolean;
  onToggle: () => void;
  onClose: () => void;
  label: string;
  primaryPath?: string | null;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open, onClose]);

  const nonMainWorktrees = worktrees.filter((w) => !w.is_main);
  const mainWorktree = worktrees.find((w) => w.is_main);
  const primaryDisplayPath = mainWorktree?.path ?? primaryPath ?? "";
  const primaryBranch = mainWorktree?.branch || "current branch";

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={onToggle}
        className={clsx(
          "flex items-center gap-1 rounded-md px-2.5 py-1.5 text-[11px] transition border",
          open
            ? "border-primary/50 bg-accent text-foreground"
            : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
      >
        <FolderTree size={11} />
        <span>{label}</span>
        <ChevronDown size={10} className={clsx("transition", open && "rotate-180")} />
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-50 mb-1 min-w-[260px] max-h-[300px] overflow-auto rounded-lg border border-border bg-background shadow-lg">
          <div className="px-3 py-1.5 border-b border-border/50 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
            Worktree
          </div>
          <div className="py-1">
            {/* Auto option */}
            <button
              type="button"
              onClick={() => { onChange({ mode: "auto" }); onClose(); }}
              className={clsx(
                "flex w-full items-start gap-2 px-3 py-2 text-left transition",
                choice.mode === "auto" ? "bg-primary/10" : "hover:bg-accent",
              )}
            >
              <div className="min-w-0 flex-1">
                <div className="text-[11px] font-medium text-foreground">Auto (recommended)</div>
                <div className="text-[10px] text-muted-foreground">Crab creates an isolated worktree automatically</div>
              </div>
              {choice.mode === "auto" && <span className="mt-0.5 text-primary">✓</span>}
            </button>

            {/* Primary workspace option */}
            <button
              type="button"
              onClick={() => { onChange({ mode: "primary" }); onClose(); }}
              className={clsx(
                "flex w-full items-start gap-2 px-3 py-2 text-left transition",
                choice.mode === "primary" ? "bg-primary/10" : "hover:bg-accent",
              )}
            >
              <div className="min-w-0 flex-1">
                <div className="text-[11px] font-medium text-foreground">Work locally</div>
                <div className="text-[10px] text-muted-foreground">Run directly in the open repo folder</div>
                {(primaryBranch || primaryDisplayPath) && (
                  <div className="mt-0.5 flex items-center gap-2 text-[10px] text-muted-foreground">
                    <span className="flex items-center gap-0.5">
                      <GitBranch size={9} />
                      {primaryBranch}
                    </span>
                    {primaryDisplayPath && (
                      <span className="truncate font-mono opacity-60">
                        {primaryDisplayPath.split("/").slice(-2).join("/")}
                      </span>
                    )}
                  </div>
                )}
              </div>
              {choice.mode === "primary" && <span className="mt-0.5 text-primary">✓</span>}
            </button>

            {/* Existing worktrees */}
            {nonMainWorktrees.length > 0 && (
              <>
                <div className="px-3 py-1 text-[9px] font-semibold uppercase tracking-wider text-muted-foreground/70 mt-1">
                  Existing worktrees
                </div>
                {nonMainWorktrees.map((wt) => {
                  const name = wt.meta?.display_name || wt.path.split("/").pop() || wt.path;
                  const isSelected = choice.mode === "existing" && choice.path === wt.path;
                  return (
                    <button
                      key={wt.path}
                      type="button"
                      onClick={() => { onChange({ mode: "existing", path: wt.path }); onClose(); }}
                      className={clsx(
                        "flex w-full items-start gap-2 px-3 py-2 text-left transition",
                        isSelected ? "bg-primary/10" : "hover:bg-accent",
                      )}
                    >
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-1.5 text-[11px] font-medium text-foreground">
                          <span className="truncate">{name}</span>
                          {wt.locked && (
                            <span className="shrink-0 text-[9px] text-warning">locked</span>
                          )}
                        </div>
                        <div className="flex items-center gap-2 text-[10px] text-muted-foreground">
                          <span className="flex items-center gap-0.5">
                            <GitBranch size={9} />
                            {wt.branch || "detached"}
                          </span>
                          <span className="truncate font-mono opacity-60">{wt.path.split("/").slice(-2).join("/")}</span>
                        </div>
                      </div>
                      {isSelected && <span className="mt-0.5 text-primary">✓</span>}
                    </button>
                  );
                })}
              </>
            )}

            {/* Create new worktree */}
            <div className="border-t border-border/50 mt-1 pt-1">
              <button
                type="button"
                onClick={() => { onChange({ mode: "new", name: "", baseBranch: "" }); onClose(); }}
                className="flex w-full items-center gap-2 px-3 py-2 text-left text-[11px] text-muted-foreground hover:bg-accent hover:text-foreground transition"
              >
                <Plus size={11} />
                <span>Create new worktree manually…</span>
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ── Dropdown Button ─────────────────────────────────────────────────

interface DropdownItem {
  id: string;
  label: string;
  disabled?: boolean;
  suffix?: string;
}

interface DropdownButtonProps {
  open: boolean;
  onToggle: () => void;
  onClose: () => void;
  label: React.ReactNode;
  items: DropdownItem[];
  onSelect: (id: string) => void;
  selectedId: string;
}

function DropdownButton({ open, onToggle, onClose, label, items, onSelect, selectedId }: DropdownButtonProps) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open, onClose]);

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={onToggle}
        className={clsx(
          "flex items-center gap-1 rounded-md px-2.5 py-1.5 text-[11px] transition border",
          open
            ? "border-primary/50 bg-accent text-foreground"
            : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
      >
        {label}
        <ChevronDown size={10} className={clsx("transition", open && "rotate-180")} />
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-50 mb-1 min-w-[160px] max-h-[240px] overflow-auto rounded-lg border border-border bg-background shadow-lg">
          <div className="py-1">
            {items.map((item) => (
              <button
                key={item.id}
                type="button"
                disabled={item.disabled}
                onClick={() => { onSelect(item.id); onClose(); }}
                className={clsx(
                  "flex w-full items-center gap-2 px-3 py-1.5 text-[11px] transition",
                  item.id === selectedId
                    ? "bg-primary/10 text-primary"
                    : "text-foreground hover:bg-accent",
                  item.disabled && "opacity-50 cursor-not-allowed",
                )}
              >
                <span className="flex-1 text-left">{item.label}</span>
                {item.suffix && (
                  <span className="text-[9px] text-muted-foreground">{item.suffix}</span>
                )}
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatRelativeTime(timestamp: number): string {
  const diff = Date.now() - timestamp;
  const minutes = Math.floor(diff / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}
