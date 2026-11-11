import { useEffect, useRef, useState } from "react";
import { Activity, ArrowLeft, ArrowRight, ChevronDown, ChevronRight, Cloud, FolderGit2, FolderTree, PanelLeftOpen, Terminal } from "lucide-react";
import clsx from "clsx";
import { OpenRepo, WorkspaceRepoEntry, useStore } from "../state";
import { createPresetManager, type StartupCommand } from "../lib/terminal-presets";
import { AgentIcon, agentAccentColor } from "./orchestrator/AgentIcon";

interface Props {
  /** Whether the outer Repositories sidebar is visible. */
  repoSidebarOpen: boolean;
  /** Called when the user clicks the title-bar "expand repositories" button. */
  onExpandRepoSidebar: () => void;
  /** Currently-focused repo, surfaced in the chrome so the main
   *  pane recovers the vertical space. `null` when no repo is open. */
  activeRepo: OpenRepo | null;
  /** Navigation history controls. */
  canBack: boolean;
  canForward: boolean;
  onBack: () => void;
  onForward: () => void;
  /** Open the full-page terminal. */
  terminalActive: boolean;
  onOpenTerminal: () => void;
  onShowOverview: () => void;
  activityDrawerOpen: boolean;
  onToggleActivityDrawer: () => void;
  /** Called when user selects an agent preset from the dropdown. */
  onSpawnAgentTerminal?: (cmd: StartupCommand) => void;
  /** Workspace name (when a workspace is active). */
  workspaceName?: string | null;
  /** All repos in the active workspace (for the dropdown switcher). */
  workspaceRepos?: WorkspaceRepoEntry[];
  /** Currently active workspace repo relative path. */
  activeWorkspaceRelativePath?: string | null;
  /** Called when user selects a repo from the workspace dropdown. */
  onWorkspaceRepoSelect?: (relativePath: string) => void;
}

/**
 * Top chrome (title-bar replacement on macOS).
 *
 * Electron's `titleBarStyle: "hiddenInset"` hides the native titlebar
 * but still renders the traffic lights in the top-left. For the
 * window to remain draggable we designate a chrome region ourselves
 * via `-webkit-app-region: drag`; interactive elements inside that
 * region need `-webkit-app-region: no-drag` or clicks fall through
 * to the OS window manager.
 *
 * macOS traffic lights occupy ~78 px from the window's left edge,
 * so we pad the content to keep the logo clear of them.
 *
 * The centre region now carries the active repo's label + URL so
 * we can drop the per-pane header in `<RepoDetail/>`. Cache dir /
 * remote indicator live in Settings → Workspace → Cache (less
 * valuable for the at-a-glance view).
 */
export function TopBar({
  repoSidebarOpen,
  onExpandRepoSidebar,
  activeRepo,
  canBack,
  canForward,
  onBack,
  onForward,
  terminalActive,
  onOpenTerminal,
  onShowOverview,
  activityDrawerOpen,
  onToggleActivityDrawer,
  onSpawnAgentTerminal,
  workspaceName,
  workspaceRepos,
  activeWorkspaceRelativePath,
  onWorkspaceRepoSelect,
}: Props) {
  const { state } = useStore();
  const running = Object.values(state.jobs).filter(job => job.status === "running").length;
  // Derive worktree display info from the active repo label.
  const isWorktree = activeRepo?.label.startsWith("↳") ?? false;
  const worktreeName = isWorktree
    ? activeRepo!.label.replace(/^↳\s*(?:worktree:|wt:)?/, "")
    : null;

  // Wrap onSpawnAgentTerminal to inject agentMeta derived from the active
  // workspace and the selected agent preset before passing it downstream.
  const handleSpawnAgentTerminalWithMeta = onSpawnAgentTerminal
    ? (cmd: StartupCommand) => {
        const worktreePath = activeRepo?.path ?? activeRepo?.cache_dir ?? "";
        const agentType = cmd.icon ?? cmd.id;
        const taskId = `topbar-${agentType}-${Date.now()}`;
        onSpawnAgentTerminal({
          ...cmd,
          agentMeta: { taskId, agentType, worktreePath },
        });
      }
    : undefined;

  // Workspace repo dropdown state.
  const [wsDropdownOpen, setWsDropdownOpen] = useState(false);
  const wsDropdownRef = useRef<HTMLDivElement>(null);

  // Close workspace dropdown on outside click.
  useEffect(() => {
    if (!wsDropdownOpen) return;
    function handleClick(e: MouseEvent) {
      if (wsDropdownRef.current && !wsDropdownRef.current.contains(e.target as Node)) {
        setWsDropdownOpen(false);
      }
    }
    function handleKey(e: KeyboardEvent) {
      if (e.key === "Escape") setWsDropdownOpen(false);
    }
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [wsDropdownOpen]);

  // Update the window title to reflect the active repo/worktree context.
  useEffect(() => {
    if (!activeRepo) {
      document.title = "Crab Desktop";
      return;
    }
    if (isWorktree && worktreeName) {
      // Extract parent repo name from the URL or path for context.
      const parentName = activeRepo.url
        ? activeRepo.url.split("/").filter(Boolean).pop() ?? "repo"
        : "repo";
      document.title = `Crab Desktop — ${parentName} / ${worktreeName}`;
    } else {
      document.title = `Crab Desktop — ${activeRepo.label}`;
    }
  }, [activeRepo, isWorktree, worktreeName]);

  return (
    <header
      className="flex h-11 shrink-0 items-center gap-3 border-b border-gx-border bg-gx-surface pr-3 pl-[84px]"
      style={{ WebkitAppRegion: "drag" } as React.CSSProperties}
    >
      <div
        className="flex shrink-0 items-center gap-1"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        {!repoSidebarOpen && (
          <button
            type="button"
            className="flex h-7 w-7 items-center justify-center rounded text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={onExpandRepoSidebar}
            title="Show Repositories (⇧⌘B)"
            aria-label="Show Repositories"
          >
            <PanelLeftOpen size={14} />
          </button>
        )}
        <button
          type="button"
          className="flex h-7 w-7 items-center justify-center rounded text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text disabled:opacity-30 disabled:pointer-events-none"
          onClick={onBack}
          disabled={!canBack}
          title="Go Back (⌘[)"
          aria-label="Go back"
        >
          <ArrowLeft size={14} />
        </button>
        <button
          type="button"
          className="flex h-7 w-7 items-center justify-center rounded text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text disabled:opacity-30 disabled:pointer-events-none"
          onClick={onForward}
          disabled={!canForward}
          title="Go Forward (⌘])"
          aria-label="Go forward"
        >
          <ArrowRight size={14} />
        </button>
      </div>

      {/* Centre slot — workspace repo switcher or single repo identity.
          Logic:
          1. Workspace mode: active repo IS part of a workspace → workspace icon + name + dropdown
          2. Single repo mode: active repo is standalone (not in any workspace) → repo icon + name, no dropdown
       */}
      <div className="flex min-w-0 flex-1 items-center justify-center">
        {workspaceName && workspaceRepos && workspaceRepos.length > 0 && activeWorkspaceRelativePath && onWorkspaceRepoSelect ? (
          /* Workspace mode: active repo belongs to a workspace — show workspace icon + name + repo dropdown */
          <div
            className="relative"
            ref={wsDropdownRef}
            style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
          >
            <div className="flex max-w-full items-center rounded-md border border-gx-border/60 bg-gx-bg-sub text-xs">
              <button type="button" aria-label="Repository overview" title="Repository overview" onClick={onShowOverview} className="flex min-w-0 items-center gap-2 rounded-l-md px-3 py-1 hover:bg-gx-surface-hover">
                <FolderGit2 size={12} className="shrink-0 text-gx-accent" />
                <span className="max-w-32 truncate text-gx-muted">{workspaceName}</span>
                {activeRepo && <><ChevronRight size={10} className="shrink-0 text-gx-muted" /><span className="max-w-40 truncate text-gx-text">{activeRepo.label}</span></>}
              </button>
              <button type="button" aria-label="Switch repository" aria-expanded={wsDropdownOpen} onClick={() => setWsDropdownOpen((value) => !value)} className="rounded-r-md border-l border-gx-border px-2 py-2 text-gx-muted hover:bg-gx-surface-hover"><ChevronDown size={10} /></button>
            </div>

            {/* Dropdown list of workspace repos */}
            {wsDropdownOpen && (
              <div className="absolute left-1/2 -translate-x-1/2 top-full z-50 mt-1 w-64 max-h-[320px] overflow-y-auto rounded-lg border border-gx-border bg-gx-surface shadow-xl py-1">
                {workspaceRepos.map((entry) => {
                  const isActive = entry.relativePath === activeWorkspaceRelativePath;
                  const isDirty = (entry.dirtyCount ?? 0) > 0;
                  return (
                    <button
                      key={entry.relativePath}
                      type="button"
                      className={clsx(
                        "flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] transition",
                        isActive
                          ? "bg-gx-accent/10 text-gx-text"
                          : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
                      )}
                      onClick={() => {
                        setWsDropdownOpen(false);
                        onWorkspaceRepoSelect(entry.relativePath);
                      }}
                    >
                      <span className="min-w-0 flex-1 truncate font-medium">{entry.name}</span>
                      {entry.branch && (
                        <span className="shrink-0 text-[10px] text-gx-muted font-mono max-w-[80px] truncate">
                          {entry.branch}
                        </span>
                      )}
                      {isDirty && (
                        <span className="shrink-0 h-2 w-2 rounded-full bg-orange-400" title={`${entry.dirtyCount} changes`} />
                      )}
                      {isActive && (
                        <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-gx-accent" />
                      )}
                    </button>
                  );
                })}
                {workspaceRepos.length === 0 && (
                  <div className="px-3 py-2 text-[11px] text-gx-muted text-center">No repos in workspace</div>
                )}
              </div>
            )}
          </div>
        ) : activeRepo ? (
          /* Single repo mode: standalone repo — show repo icon + name, no dropdown */
          <button
            className="flex min-w-0 max-w-full items-center gap-2 rounded-md border border-gx-border/60 bg-gx-bg-sub px-3 py-1 text-[12px]"
            style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
            title={`${activeRepo.label}\n${activeRepo.url}`}
            type="button"
            aria-label="Repository overview"
            onClick={onShowOverview}
          >
            {isWorktree ? (
              <FolderTree size={12} className="shrink-0 text-gx-accent" />
            ) : activeRepo.is_remote ? (
              <Cloud size={12} className="shrink-0 text-gx-accent" />
            ) : (
              <FolderGit2 size={12} className="shrink-0 text-gx-muted" />
            )}
            {isWorktree && worktreeName ? (
              <>
                <span className="shrink-0 text-gx-muted text-[11px]">
                  {activeRepo.url
                    ? activeRepo.url.split("/").filter(Boolean).pop() ?? "repo"
                    : "repo"}
                </span>
                <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
                <span className="min-w-0 truncate font-medium text-gx-accent">
                  {worktreeName}
                </span>
              </>
            ) : (
              <span className="min-w-0 truncate font-medium text-gx-text">
                {activeRepo.label}
              </span>
            )}
          </button>
        ) : (
          <div
            className="select-none text-[11px] text-gx-muted"
            style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
          >
            No repository open
          </div>
        )}
      </div>

      <div
        className="flex shrink-0 items-center gap-2"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        <button
          type="button"
          onClick={onToggleActivityDrawer}
          className={clsx(
            "flex h-7 items-center gap-1.5 rounded px-2 text-[11px] text-gx-muted transition hover:bg-gx-surface-hover hover:text-gx-text",
            activityDrawerOpen && "bg-gx-accent/15 text-gx-accent",
            running > 0 && !activityDrawerOpen && "text-gx-warn",
          )}
          title="Toggle Activity (background jobs)"
          aria-label="Activity"
          aria-expanded={activityDrawerOpen}
          aria-controls="activity-drawer"
        >
          <Activity size={14} />
          Activity
          {running > 0 && <span className="rounded bg-gx-warn/20 px-1 tabular-nums text-gx-warn">{running}</span>}
        </button>
        <TerminalSplitButton
          terminalActive={terminalActive}
          onOpenTerminal={onOpenTerminal}
          onSpawnAgentTerminal={handleSpawnAgentTerminalWithMeta}
        />
      </div>
    </header>
  );
}

// --- Terminal Split Button with Agent Dropdown ---

interface TerminalSplitButtonProps {
  terminalActive: boolean;
  onOpenTerminal: () => void;
  onSpawnAgentTerminal?: (cmd: StartupCommand) => void;
}

function TerminalSplitButton({
  terminalActive,
  onOpenTerminal,
  onSpawnAgentTerminal,
}: TerminalSplitButtonProps) {
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const [hoveredId, setHoveredId] = useState<string | null>(null);
  const [inputForSub, setInputForSub] = useState<{ presetId: string; subId: string; command: string; placeholder: string } | null>(null);
  const [inputValue, setInputValue] = useState("");
  const [choicesForSub, setChoicesForSub] = useState<string | null>(null);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const hoverTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [manager] = useState(() => createPresetManager());
  const grouped = manager.getGrouped();

  // Close dropdown on outside click
  useEffect(() => {
    if (!dropdownOpen) return;
    function handleClick(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setDropdownOpen(false);
        setHoveredId(null);
      }
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [dropdownOpen]);

  // Cleanup hover timer on unmount
  useEffect(() => {
    return () => {
      if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    };
  }, []);

  function handleMouseEnter(id: string) {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    hoverTimerRef.current = setTimeout(() => setHoveredId(id), 80);
  }

  function handleMouseLeave() {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    hoverTimerRef.current = setTimeout(() => setHoveredId(null), 150);
  }

  function handleSubMenuEnter() {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
  }

  function handleSubMenuLeave() {
    if (hoverTimerRef.current) clearTimeout(hoverTimerRef.current);
    hoverTimerRef.current = setTimeout(() => setHoveredId(null), 150);
  }

  function handlePresetSelect(cmd: StartupCommand) {
    setDropdownOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    setChoicesForSub(null);
    onSpawnAgentTerminal?.(cmd);
  }

  function handleSubOptionSelect(parent: StartupCommand, sub: { id: string; label: string; command: string; description?: string; needsInput?: boolean; inputPlaceholder?: string; choices?: { id: string; label: string; value: string; description?: string }[] }) {
    // If sub-option has known choices, toggle the choices sub-menu
    if (sub.needsInput && sub.choices && sub.choices.length > 0) {
      setChoicesForSub((prev) => (prev === sub.id ? null : sub.id));
      return;
    }
    // If sub-option needs free-text input, show inline input
    if (sub.needsInput || sub.command.endsWith(" ")) {
      setInputForSub({
        presetId: parent.id,
        subId: sub.id,
        command: sub.command,
        placeholder: sub.inputPlaceholder || "value",
      });
      setInputValue("");
      return;
    }
    // Otherwise execute immediately
    setDropdownOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    setChoicesForSub(null);
    onSpawnAgentTerminal?.({
      ...parent,
      name: `${parent.name} · ${sub.label}`,
      command: sub.command,
    });
  }

  function handleChoiceSelect(parent: StartupCommand, sub: { id: string; label: string; command: string }, choice: { label: string; value: string }) {
    setDropdownOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    setChoicesForSub(null);
    onSpawnAgentTerminal?.({
      ...parent,
      name: `${parent.name} · ${sub.label} · ${choice.label}`,
      command: `${sub.command}${choice.value}`,
    });
  }

  function handleInputSubmit(parent: StartupCommand, sub: { id: string; label: string; command: string }) {
    if (!inputValue.trim()) return;
    setDropdownOpen(false);
    setHoveredId(null);
    setInputForSub(null);
    onSpawnAgentTerminal?.({
      ...parent,
      name: `${parent.name} · ${sub.label} · ${inputValue.trim()}`,
      command: `${sub.command}${inputValue.trim()}`,
    });
  }

  return (
    <div className="relative inline-flex" ref={dropdownRef}>
      {/* Main button — opens the terminal page */}
      <button
        type="button"
        className={clsx(
          "btn text-xs rounded-r-none border-r-0 pr-1.5",
          terminalActive && "border-gx-accent text-gx-accent",
        )}
        onClick={onOpenTerminal}
        title="Open Terminal (⌃`)"
        aria-label="Open Terminal"
      >
        <Terminal size={14} />
        Terminal
      </button>

      {/* Dropdown chevron — opens agent preset menu */}
      <button
        type="button"
        className={clsx(
          "btn text-xs rounded-l-none border-l-0 pl-1 pr-1.5",
          terminalActive && "border-gx-accent text-gx-accent",
          dropdownOpen && "bg-gx-surface-hover",
        )}
        onClick={() => setDropdownOpen((o) => !o)}
        title="Open terminal with AI agent"
        aria-label="Terminal options"
        aria-haspopup="true"
        aria-expanded={dropdownOpen}
      >
        <ChevronDown size={10} />
      </button>

      {/* Dropdown menu */}
      {dropdownOpen && (
        <div className="absolute right-0 top-full z-50 mt-1 min-w-[260px] rounded-lg border border-gx-border bg-gx-surface shadow-xl">
          <div className="py-1">
            {/* Plain terminal option — always first */}
            <button
              type="button"
              onClick={() => {
                setDropdownOpen(false);
                onOpenTerminal();
              }}
              className="flex w-full items-center gap-2 px-3 py-2 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover"
            >
              <Terminal size={13} className="shrink-0 text-gx-muted" />
              <span className="font-medium">Open Terminal</span>
              <span className="ml-auto text-gx-muted text-[10px]">⌃`</span>
            </button>

            {/* Separator */}
            <div className="border-t border-gx-border my-1" />

            {/* Agent CLI presets header */}
            <div className="px-3 py-1 text-[10px] font-medium uppercase text-gx-muted tracking-wide">
              AI Code Agents
            </div>

            {/* Built-in agent presets */}
            {grouped.builtin.map((preset) => (
              <div
                key={preset.id}
                className="relative"
                onMouseEnter={() => handleMouseEnter(preset.id)}
                onMouseLeave={handleMouseLeave}
              >
                <div className="flex w-full items-center hover:bg-gx-surface-hover">
                  <button
                    type="button"
                    onClick={() => handlePresetSelect(preset)}
                    className="flex flex-1 items-center gap-2 pl-3 pr-1 py-1.5 text-left text-[11px] text-gx-text"
                  >
                    <AgentIcon agentName={preset.icon} size={13} className={agentAccentColor(preset.icon)} />
                    <span className="truncate font-medium">{preset.name}</span>
                    <span className="ml-auto text-gx-muted font-mono text-[10px]">{preset.command}</span>
                  </button>
                  {preset.subOptions && preset.subOptions.length > 0 && (
                    <button
                      type="button"
                      onClick={(e) => {
                        e.stopPropagation();
                        setHoveredId((prev) => (prev === preset.id ? null : preset.id));
                      }}
                      className="flex items-center justify-center px-2 py-1.5 text-gx-muted hover:text-gx-text shrink-0"
                    >
                      <ChevronRight size={10} />
                    </button>
                  )}
                </div>

                {/* Flyout sub-menu for agent modes/flags */}
                {hoveredId === preset.id && preset.subOptions && preset.subOptions.length > 0 && (
                  <div
                    className="fixed z-[9999] min-w-[240px] max-w-[300px] rounded-lg border border-gx-border bg-gx-surface py-1 shadow-xl"
                    ref={(el) => {
                      if (!el) return;
                      const parent = el.parentElement;
                      if (!parent) return;
                      const parentRect = parent.getBoundingClientRect();
                      // Position to the left since we're near the right edge
                      const elWidth = 240;
                      if (parentRect.left - elWidth - 4 > 0) {
                        el.style.left = `${parentRect.left - elWidth - 2}px`;
                      } else {
                        el.style.left = `${parentRect.right + 2}px`;
                      }
                      el.style.top = `${parentRect.top}px`;
                      const elRect = el.getBoundingClientRect();
                      if (elRect.bottom > window.innerHeight - 8) {
                        el.style.top = `${window.innerHeight - 8 - elRect.height}px`;
                      }
                    }}
                    onMouseEnter={handleSubMenuEnter}
                    onMouseLeave={handleSubMenuLeave}
                  >
                    <div className="px-3 py-1 text-[10px] font-medium uppercase text-gx-muted tracking-wide">
                      {preset.name} Modes
                    </div>
                    {preset.subOptions.map((sub) => (
                      <div key={sub.id}>
                        {/* Sub-option with choices: expandable nested list */}
                        {sub.needsInput && sub.choices && sub.choices.length > 0 ? (
                          <div>
                            <button
                              type="button"
                              onClick={() => setChoicesForSub((prev) => (prev === sub.id ? null : sub.id))}
                              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                              title={sub.description}
                            >
                              <span className="truncate">{sub.label}</span>
                              <ChevronRight size={9} className="ml-auto shrink-0 opacity-60" />
                            </button>
                            {choicesForSub === sub.id && (
                              <div className="border-l-2 border-gx-accent/30 ml-3 pl-1 py-0.5">
                                {sub.choices.map((choice) => (
                                  <button
                                    key={choice.id}
                                    type="button"
                                    onClick={() => handleChoiceSelect(preset, sub, choice)}
                                    className="flex w-full items-center gap-2 px-2 py-1 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text rounded"
                                    title={choice.description}
                                  >
                                    <span className="truncate">{choice.label}</span>
                                    <span className="ml-auto font-mono text-[9px] opacity-60">{choice.value}</span>
                                  </button>
                                ))}
                                {/* Custom input fallback */}
                                <div className="px-2 py-1">
                                  <form
                                    onSubmit={(e) => {
                                      e.preventDefault();
                                      if (inputValue.trim()) {
                                        handleChoiceSelect(preset, sub, { label: inputValue.trim(), value: inputValue.trim() });
                                      }
                                    }}
                                    className="flex items-center gap-1"
                                  >
                                    <input
                                      type="text"
                                      value={inputForSub?.subId === sub.id ? inputValue : ""}
                                      onChange={(e) => { setInputValue(e.target.value); setInputForSub({ presetId: preset.id, subId: sub.id, command: sub.command, placeholder: sub.inputPlaceholder || "custom" }); }}
                                      placeholder={sub.inputPlaceholder || "custom value"}
                                      className="flex-1 min-w-0 px-1.5 py-0.5 text-[10px] rounded border border-gx-border bg-gx-bg text-gx-text font-mono focus:outline-none focus:border-gx-accent"
                                    />
                                    <button type="submit" className="text-[10px] text-gx-accent hover:text-gx-text px-1">↵</button>
                                  </form>
                                </div>
                              </div>
                            )}
                          </div>
                        ) : sub.needsInput || sub.command.endsWith(" ") ? (
                          /* Sub-option that needs free-text input */
                          <div>
                            <button
                              type="button"
                              onClick={() => {
                                setInputForSub({
                                  presetId: preset.id,
                                  subId: sub.id,
                                  command: sub.command,
                                  placeholder: sub.inputPlaceholder || "value",
                                });
                                setInputValue("");
                              }}
                              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                              title={sub.description}
                            >
                              <span className="truncate">{sub.label}</span>
                              <span className="ml-auto text-[9px] italic opacity-60">requires input</span>
                            </button>
                            {inputForSub?.subId === sub.id && (
                              <div className="px-3 py-1">
                                <form
                                  onSubmit={(e) => { e.preventDefault(); handleInputSubmit(preset, sub); }}
                                  className="flex items-center gap-1"
                                >
                                  <input
                                    type="text"
                                    value={inputValue}
                                    onChange={(e) => setInputValue(e.target.value)}
                                    placeholder={inputForSub.placeholder}
                                    className="flex-1 min-w-0 px-1.5 py-0.5 text-[10px] rounded border border-gx-border bg-gx-bg text-gx-text font-mono focus:outline-none focus:border-gx-accent"
                                    autoFocus
                                  />
                                  <button
                                    type="submit"
                                    disabled={!inputValue.trim()}
                                    className="text-[10px] text-gx-accent hover:text-gx-text px-1 disabled:opacity-30"
                                  >
                                    ↵
                                  </button>
                                </form>
                              </div>
                            )}
                          </div>
                        ) : (
                          /* Normal sub-option — execute immediately */
                          <button
                            type="button"
                            onClick={() => handleSubOptionSelect(preset, sub)}
                            className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                            title={sub.description}
                          >
                            <span className="truncate">{sub.label}</span>
                            <span className="ml-auto font-mono text-[9px] opacity-70 truncate max-w-[120px]">{sub.command}</span>
                          </button>
                        )}
                      </div>
                    ))}
                  </div>
                )}
              </div>
            ))}

            {/* Custom commands section */}
            {grouped.custom.length > 0 && (
              <>
                <div className="border-t border-gx-border my-1" />
                <div className="px-3 py-1 text-[10px] font-medium uppercase text-gx-muted tracking-wide">
                  Custom Commands
                </div>
                {grouped.custom.map((cmd) => (
                  <button
                    key={cmd.id}
                    type="button"
                    onClick={() => handlePresetSelect(cmd)}
                    className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] text-gx-text hover:bg-gx-surface-hover"
                  >
                    <Terminal size={11} className="shrink-0 text-gx-muted" />
                    <span className="truncate">{cmd.name}</span>
                    <span className="ml-auto font-mono text-[10px] text-gx-muted truncate max-w-[100px]">{cmd.command}</span>
                  </button>
                ))}
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
