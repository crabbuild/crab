import { useCallback, useEffect, useState } from "react";
import { Paperclip, X } from "lucide-react";
import { IssueSelector, IssueItem, IssueRef } from "./IssueSelector";
import { WorkspaceRepoSelector } from "./WorkspaceRepoSelector";
import { SkillPicker } from "./SkillPicker";
import { useStore, getActiveWorkspace } from "../../state";
import type { WorktreeChoice } from "./NewChatLanding";

// ── Types ───────────────────────────────────────────────────────────

interface DetectedAgent {
  /** Registry identifier (e.g. "claude-code", "codex"). Used when creating tasks. */
  type: string;
  /** Human-readable display name ("Claude Code", "Codex"). */
  name: string;
  version?: string;
  available: boolean;
}

type TaskSource = "manual" | "github" | "linear";

interface Props {
  open: boolean;
  onClose: () => void;
  onSubmit: (data: NewTaskData) => void;
  /** Agents detected on the system PATH. */
  agents: DetectedAgent[];
  /** Available branches for the base branch picker. */
  branches: string[];
  /** Pre-filled context (e.g. from "Delegate to agent" action). */
  initialPrompt?: string;
  initialContextFiles?: string[];
  /** Pre-loaded issue (e.g. from deep-link). */
  initialIssue?: IssueItem;
  initialSource?: TaskSource;
}

export interface NewTaskData {
  prompt: string;
  agentName: string;
  baseBranch: string;
  planFirst: boolean;
  contextFiles: string[];
  workflowDefinitionIds?: string[];
  issueRef?: IssueRef;
  /** When workspace is active, the relative path of the target repo. */
  targetRepoPath?: string;
  /** Model override (e.g. "gpt-5.5", "claude-sonnet-4-6"). Empty = agent default. */
  model?: string;
  /** Intelligence/reasoning effort level (e.g. "high", "extra-high"). */
  intelligence?: string;
  /** Worktree/session target chosen from the new chat composer. */
  worktree?: WorktreeChoice;
  /** Skill IDs to activate for this task. */
  skillIds?: string[];
}

// ── Component ───────────────────────────────────────────────────────

export function NewTaskDialog({
  open,
  onClose,
  onSubmit,
  agents,
  branches,
  initialPrompt,
  initialContextFiles,
  initialIssue,
  initialSource,
}: Props) {
  const { state } = useStore();
  const workspace = getActiveWorkspace(state);

  // Derive the default target repo from the currently active repo in the workspace.
  // When only one repo exists, auto-select it so the user doesn't have to interact
  // with the dropdown at all. When no repos are discovered yet, fall back to empty
  // string and skip the repo requirement in handleSubmit.
  const defaultRepoPath = (() => {
    if (!workspace) return "";
    // No repos discovered yet — don't require selection.
    if (workspace.repos.length === 0) return "";
    // Single repo → auto-select without requiring user interaction.
    if (workspace.repos.length === 1) return workspace.repos[0].relativePath;
    // Multi-repo → use the currently active repo if available.
    if (!state.activeRepoId) return "";
    const entry = workspace.repos.find((e) => e.openRepoId === state.activeRepoId);
    return entry?.relativePath ?? "";
  })();

  // Whether the workspace repo selector should be shown and required.
  // Skip the requirement when workspace has no repos (discovery pending)
  // or exactly one repo (auto-selected).
  const requiresRepoSelection = !!workspace && workspace.repos.length > 1;

  const [prompt, setPrompt] = useState(initialPrompt ?? "");
  // `agent` is the registry TYPE (e.g. "codex"), not the display name.
  // This is what's passed to `createTask` as `agentType`.
  const [agent, setAgent] = useState(agents[0]?.type ?? "");
  const [baseBranch, setBaseBranch] = useState(branches[0] ?? "main");
  const [planFirst, setPlanFirst] = useState(false);
  const [contextFiles, setContextFiles] = useState<string[]>(initialContextFiles ?? []);
  const [source, setSource] = useState<TaskSource>(initialSource ?? "manual");
  const [selectedIssue, setSelectedIssue] = useState<IssueItem | null>(initialIssue ?? null);
  const [targetRepoPath, setTargetRepoPath] = useState(defaultRepoPath);
  const [selectedSkills, setSelectedSkills] = useState<string[]>([]);

  // Reset form when dialog opens with new initial values.
  useEffect(() => {
    if (open) {
      setPrompt(initialPrompt ?? "");
      setContextFiles(initialContextFiles ?? []);
      if (agents.length > 0 && !agent) setAgent(agents[0].type);
      if (branches.length > 0) setBaseBranch(branches[0]);
      setTargetRepoPath(defaultRepoPath);
      setSelectedSkills([]);
      if (initialIssue) {
        setSelectedIssue(initialIssue);
        setSource(initialSource ?? "github");
        populateFromIssue(initialIssue);
      } else {
        setSource(initialSource ?? "manual");
        setSelectedIssue(null);
      }
    }
  }, [open, initialPrompt, initialContextFiles, initialIssue, initialSource, agents, branches, defaultRepoPath]);

  const populateFromIssue = useCallback((issue: IssueItem) => {
    // Auto-populate prompt from issue title + body + discussion.
    let text = `[Issue #${issue.number}] ${issue.title}`;
    if (issue.body) {
      text += `\n\n${issue.body}`;
    }
    if (issue.discussion) {
      text += `\n\n--- Discussion ---\n${issue.discussion}`;
    }
    setPrompt(text);
  }, []);

  const handleIssueSelect = useCallback(
    (issue: IssueItem) => {
      setSelectedIssue(issue);
      populateFromIssue(issue);
    },
    [populateFromIssue],
  );

  const handleSourceChange = useCallback((newSource: TaskSource) => {
    setSource(newSource);
    if (newSource === "manual") {
      setSelectedIssue(null);
      // Don't clear prompt — user may have typed something.
    }
  }, []);

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!prompt.trim()) return;
      // Only require explicit repo selection for multi-repo workspaces.
      if (requiresRepoSelection && !targetRepoPath) return;

      // Build issueRef if an issue is selected.
      let issueRef: IssueRef | undefined;
      if (selectedIssue && source !== "manual") {
        issueRef = {
          provider: source === "linear" ? "linear" : "github",
          number: selectedIssue.number,
          url: selectedIssue.url,
        };
      }

      onSubmit({
        prompt: prompt.trim(),
        agentName: agent,
        baseBranch,
        planFirst,
        contextFiles,
        issueRef,
        targetRepoPath: workspace ? targetRepoPath || undefined : undefined,
        skillIds: selectedSkills.length > 0 ? selectedSkills : undefined,
      });
      onClose();
    },
    [prompt, agent, baseBranch, planFirst, contextFiles, selectedSkills, selectedIssue, source, workspace, targetRepoPath, requiresRepoSelection, onSubmit, onClose],
  );

  const addContextFile = useCallback(async () => {
    try {
      const picked = await (window as any).crab.orchestrator.pickContextFiles();
      if (picked && Array.isArray(picked)) {
        setContextFiles((prev) => [...new Set([...prev, ...picked])]);
      }
    } catch {
      // User cancelled or IPC unavailable.
    }
  }, []);

  const removeContextFile = useCallback((path: string) => {
    setContextFiles((prev) => prev.filter((f) => f !== path));
  }, []);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-full max-w-lg max-h-[85vh] overflow-y-auto rounded-lg border border-gx-border bg-gx-bg p-5 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="mb-4 text-sm font-semibold text-gx-text">New Task</h2>

        <form onSubmit={handleSubmit} className="space-y-4">
          {/* Source selection */}
          <div>
            <label className="mb-1.5 block text-[11px] font-medium text-gx-muted">Source</label>
            <div className="flex gap-3">
              <label className="flex items-center gap-1.5 text-xs text-gx-text cursor-pointer">
                <input
                  type="radio"
                  name="task-source"
                  value="manual"
                  checked={source === "manual"}
                  onChange={() => handleSourceChange("manual")}
                  className="h-3 w-3 accent-gx-accent"
                />
                Describe manually
              </label>
              <label className="flex items-center gap-1.5 text-xs text-gx-text cursor-pointer">
                <input
                  type="radio"
                  name="task-source"
                  value="github"
                  checked={source === "github"}
                  onChange={() => handleSourceChange("github")}
                  className="h-3 w-3 accent-gx-accent"
                />
                From GitHub issue
              </label>
              <label className="flex items-center gap-1.5 text-xs text-gx-text cursor-pointer">
                <input
                  type="radio"
                  name="task-source"
                  value="linear"
                  checked={source === "linear"}
                  onChange={() => handleSourceChange("linear")}
                  className="h-3 w-3 accent-gx-accent"
                />
                From Linear issue
              </label>
            </div>
          </div>

          {/* Issue selector (shown for GitHub/Linear sources) */}
          {source !== "manual" && (
            <IssueSelector
              provider={source === "linear" ? "linear" : "github"}
              onSelect={handleIssueSelect}
              selectedIssueNumber={selectedIssue?.number}
            />
          )}

          {/* Prompt */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-gx-muted">
              What should the agent do?
            </label>
            <textarea
              className="w-full resize-none rounded-md border border-gx-border bg-gx-bg-sub px-3 py-2 text-xs text-gx-text placeholder:text-gx-muted/60 focus:border-gx-accent focus:outline-none"
              rows={4}
              placeholder="Describe the task…"
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              autoFocus={source === "manual"}
            />
          </div>

          {/* Workspace repo selector (shown when workspace is active) */}
          {workspace && (
            <WorkspaceRepoSelector
              selectedRepoPath={targetRepoPath}
              onSelect={setTargetRepoPath}
            />
          )}

          {/* Skills */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-gx-muted">
              Skills <span className="text-gx-muted/60">(optional)</span>
            </label>
            <SkillPicker
              selected={selectedSkills}
              onChange={setSelectedSkills}
              repoPath={targetRepoPath}
            />
          </div>

          {/* Agent selector */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-gx-muted">Agent</label>
            <select
              className="w-full rounded-md border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-xs text-gx-text focus:border-gx-accent focus:outline-none"
              value={agent}
              onChange={(e) => setAgent(e.target.value)}
            >
              {agents.map((a) => (
                <option
                  key={a.type}
                  value={a.type}
                  disabled={!a.available}
                >
                  {a.name} {a.version ? `(${a.version})` : ""}
                  {!a.available ? " — not installed" : ""}
                </option>
              ))}
              {agents.length === 0 && <option value="">No agents detected</option>}
            </select>
          </div>

          {/* Base branch */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-gx-muted">Base branch</label>
            <select
              className="w-full rounded-md border border-gx-border bg-gx-bg-sub px-3 py-1.5 text-xs text-gx-text focus:border-gx-accent focus:outline-none"
              value={baseBranch}
              onChange={(e) => setBaseBranch(e.target.value)}
            >
              {branches.map((b) => (
                <option key={b} value={b}>
                  {b}
                </option>
              ))}
            </select>
          </div>

          {/* Plan first toggle */}
          <div className="flex items-center gap-2">
            <input
              type="checkbox"
              id="plan-first"
              checked={planFirst}
              onChange={(e) => setPlanFirst(e.target.checked)}
              className="h-3.5 w-3.5 rounded border-gx-border accent-gx-accent"
            />
            <label htmlFor="plan-first" className="text-xs text-gx-text">
              Plan first (agent proposes plan before coding)
            </label>
          </div>

          {/* Context files */}
          <div>
            <label className="mb-1 block text-[11px] font-medium text-gx-muted">Context files</label>
            <div className="space-y-1">
              {contextFiles.map((f) => (
                <div
                  key={f}
                  className="flex items-center gap-2 rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-[10px] text-gx-muted"
                >
                  <Paperclip size={10} className="shrink-0" />
                  <span className="min-w-0 flex-1 truncate font-mono">{f}</span>
                  <button
                    type="button"
                    onClick={() => removeContextFile(f)}
                    className="shrink-0 text-gx-muted hover:text-gx-text"
                    aria-label={`Remove ${f}`}
                  >
                    <X size={10} />
                  </button>
                </div>
              ))}
            </div>
            <button
              type="button"
              onClick={addContextFile}
              className="mt-1.5 flex items-center gap-1 text-[11px] text-gx-accent hover:underline"
            >
              <Paperclip size={10} /> Add files
            </button>
          </div>

          {/* Actions */}
          <div className="flex justify-end gap-2 pt-2">
            <button
              type="button"
              onClick={onClose}
              className="rounded-md border border-gx-border px-3 py-1.5 text-xs text-gx-muted hover:text-gx-text transition"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={!prompt.trim() || !agent || (requiresRepoSelection && !targetRepoPath)}
              className="rounded-md bg-gx-accent px-3 py-1.5 text-xs font-medium text-white transition hover:bg-gx-accent/80 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              Create &amp; Start
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
